// SPDX-License-Identifier: GPL-3.0-or-later
//! Quick Share over Bluetooth (SPEC §5.2): the classic device name a visible device takes,
//! its RFCOMM service, and the BLE advertisements that announce it. Formats from Nearby
//! Connections [R44] (`bluetooth_device_name.cc`, `ble_advertisement.cc`, see
//! docs/research/quickshare-upgrade.md).
//!
//! Linux and ChromeOS make themselves visible the way Nearby does there: Bluetooth classic
//! discoverable under the encoded device name, an RFCOMM server, and a short legacy BLE
//! advertisement ([`LEGACY_WAKE_DATA`]) that makes Android start a classic discovery.

use base64::Engine;
use base64::alphabet::URL_SAFE;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use crate::endpoint::{EndpointId, EndpointInfo, OPAQUE_LEN, PCP, SERVICE_ID_HASH, truncate_utf8};

/// RFCOMM service: UUID v3 of "NearbySharing".
pub const RFCOMM_SERVICE_UUID: &str = "a82efa21-ae5c-3dde-9bbc-f16da7b16c5a";
/// Google's Copresence service, which carries Nearby BLE advertisements.
pub const COPRESENCE_SERVICE_UUID: u16 = 0xFEF3;
/// Service data of the "advertising for legacy device" BLE advertisement
/// (`kDummyAdvertisementValue`).
pub const LEGACY_WAKE_DATA: &[u8; 23] = b"QCAAABACAAADAAAAAAAAAAA";

/// Endpoint info in a device name or regular advertisement is cut to this.
const MAX_INFO_LEN: usize = 131;
/// Device name bytes before the endpoint info: PCP, ID, hash, flags, reserved, length.
const NAME_HEADER_LEN: usize = 1 + 4 + 3 + 1 + 6 + 1;
const V1: u8 = 1;
const V2: u8 = 2;
const PCP_BITS: u8 = 0x1f;

/// absl's web-safe base64: no padding written, either accepted.
const WEB_SAFE: GeneralPurpose = GeneralPurpose::new(
    &URL_SAFE,
    GeneralPurposeConfig::new().with_encode_padding(false).with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// A Quick Share device found over Bluetooth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertised {
    pub id: EndpointId,
    pub info: EndpointInfo,
}

/// Endpoint info that fits [`MAX_INFO_LEN`], shortening the name if needed.
fn fitting_info(info: &EndpointInfo) -> Vec<u8> {
    let mut info = info.clone();
    let fixed = EndpointInfo { name: Some(String::new()), ..info.clone() }.encode().len();
    if let Some(name) = &mut info.name {
        *name = truncate_utf8(name, MAX_INFO_LEN.saturating_sub(fixed)).to_owned();
    }
    let mut encoded = info.encode();
    // Only TLVs could still be in the way; a cut TLV is ignored by readers.
    encoded.truncate(MAX_INFO_LEN);
    encoded
}

/// The Bluetooth name a visible device takes: web-safe base64 of the PCP byte, endpoint ID,
/// service ID hash, a flags byte (WebRTC off), 6 reserved bytes and the endpoint info.
pub fn device_name(id: EndpointId, info: &EndpointInfo) -> String {
    let info = fitting_info(info);
    let mut raw = Vec::with_capacity(NAME_HEADER_LEN + info.len());
    raw.push(PCP);
    raw.extend_from_slice(id.as_str().as_bytes());
    raw.extend_from_slice(&SERVICE_ID_HASH);
    raw.push(0);
    raw.extend([0; 6]);
    raw.push(info.len() as u8);
    raw.extend_from_slice(&info);
    WEB_SAFE.encode(raw)
}

/// A Quick Share device name, `None` for any other Bluetooth name.
pub fn parse_device_name(name: &str) -> Option<Advertised> {
    let raw = WEB_SAFE.decode(name).ok()?;
    if raw.len() < NAME_HEADER_LEN || raw[0] != PCP || raw[5..8] != SERVICE_ID_HASH {
        return None;
    }
    let id = EndpointId::from_bytes(raw[1..5].try_into().ok()?)?;
    let len = usize::from(raw[NAME_HEADER_LEN - 1]);
    let info = raw.get(NAME_HEADER_LEN..NAME_HEADER_LEN + len)?;
    Some(Advertised { id, info: EndpointInfo::decode(info).ok()? })
}

/// Splits `n` bytes off the front.
fn take<'a>(rest: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, tail) = rest.split_at_checked(n)?;
    *rest = tail;
    Some(head)
}

/// Parses the Copresence service data of a BLE advertisement from a Quick Share device:
/// the medium framing (version, socket version, fast flag, data, device token) around the
/// connections-level advertisement (PCP, ID, endpoint info, Bluetooth address, …).
///
/// `None` for other services, advertisement headers that point to GATT, and the legacy
/// wake-up advertisement.
pub fn parse_ble_advertisement(service_data: &[u8]) -> Option<Advertised> {
    let mut rest = service_data;
    let first = take(&mut rest, 1)?[0];
    let (version, socket_version, fast) = (first >> 5, (first >> 2) & 0x07, first & 0x02 != 0);
    if !(V1..=V2).contains(&version) || !(V1..=V2).contains(&socket_version) {
        return None;
    }
    let data = if fast {
        let len = take(&mut rest, 1)?[0];
        take(&mut rest, usize::from(len))?
    } else {
        if take(&mut rest, 3)? != SERVICE_ID_HASH {
            return None;
        }
        let len = u32::from_be_bytes(take(&mut rest, 4)?.try_into().ok()?);
        take(&mut rest, usize::try_from(len).ok()?)?
    };
    parse_connections_advertisement(data, fast)
}

fn parse_connections_advertisement(mut rest: &[u8], fast: bool) -> Option<Advertised> {
    let first = take(&mut rest, 1)?[0];
    if first >> 5 != V1 || first & PCP_BITS != PCP & PCP_BITS {
        return None;
    }
    if !fast && take(&mut rest, 3)? != SERVICE_ID_HASH {
        return None;
    }
    let id = EndpointId::from_bytes(take(&mut rest, 4)?.try_into().ok()?)?;
    let len = take(&mut rest, 1)?[0];
    let info = take(&mut rest, usize::from(len))?;
    // Fast advertisements carry the info without a name (hidden devices only).
    if info.len() < 1 + OPAQUE_LEN || info.len() > MAX_INFO_LEN {
        return None;
    }
    Some(Advertised { id, info: EndpointInfo::decode(info).ok()? })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DeviceType;

    fn id() -> EndpointId {
        EndpointId::parse("AB12").unwrap()
    }

    #[test]
    fn device_names_round_trip() {
        let info = EndpointInfo::visible("Ole's ThinkPad", DeviceType::Laptop);
        let name = device_name(id(), &info);
        assert!(!name.contains('='), "absl does not pad");
        let parsed = parse_device_name(&name).unwrap();
        assert_eq!(parsed.id, id());
        assert_eq!(parsed.info.name.as_deref(), Some("Ole's ThinkPad"));
        assert_eq!(parsed.info.device_type, DeviceType::Laptop);
        // Padded names from other implementations parse too.
        let padded = format!("{name}{}", "=".repeat((4 - name.len() % 4) % 4));
        assert_eq!(parse_device_name(&padded).unwrap().id, id());
    }

    #[test]
    fn device_names_fit_bluetooth_and_nearby_limits() {
        let info = EndpointInfo::visible(&"ä".repeat(200), DeviceType::Laptop);
        let name = device_name(id(), &info);
        // Bluetooth names are at most 248 bytes.
        assert!(name.len() <= 248, "{}", name.len());
        let raw = WEB_SAFE.decode(&name).unwrap();
        assert_eq!(raw.len() - NAME_HEADER_LEN, MAX_INFO_LEN - 1, "one byte lost to the cut at a char boundary");
        let parsed = parse_device_name(&name).unwrap();
        assert!(parsed.info.name.unwrap().chars().all(|c| c == 'ä'));
    }

    #[test]
    fn other_names_are_ignored() {
        assert_eq!(parse_device_name("Pixel 9"), None);
        assert_eq!(parse_device_name(""), None);
        let mut raw = WEB_SAFE.decode(device_name(id(), &EndpointInfo::visible("x", DeviceType::Phone))).unwrap();
        raw[5] ^= 1;
        assert_eq!(parse_device_name(&WEB_SAFE.encode(&raw)), None, "another service");
    }

    /// Medium framing as `ble_advertisement.cc` writes it.
    fn medium(fast: bool, data: &[u8]) -> Vec<u8> {
        let mut out = vec![(V2 << 5) | (V2 << 2) | (u8::from(fast) << 1)];
        if fast {
            out.push(data.len() as u8);
        } else {
            out.extend_from_slice(&SERVICE_ID_HASH);
            out.extend((data.len() as u32).to_be_bytes());
        }
        out.extend_from_slice(data);
        out.extend([0x12, 0x34]); // device token
        out
    }

    #[test]
    fn regular_advertisement() {
        let info = EndpointInfo::visible("Pixel 9", DeviceType::Phone).encode();
        let mut data = vec![PCP];
        data.extend_from_slice(&SERVICE_ID_HASH);
        data.extend_from_slice(b"WXYZ");
        data.push(info.len() as u8);
        data.extend_from_slice(&info);
        data.extend([0xaa; 6]); // Bluetooth address
        data.extend([0, 0]); // no UWB address, no extra fields
        let parsed = parse_ble_advertisement(&medium(false, &data)).unwrap();
        assert_eq!(parsed.id.as_str(), "WXYZ");
        assert_eq!(parsed.info.name.as_deref(), Some("Pixel 9"));
    }

    #[test]
    fn fast_advertisement() {
        let info = EndpointInfo { device_type: DeviceType::Phone, name: None, qr_data: None, vendor: None }.encode();
        assert_eq!(info.len(), 17);
        let mut data = vec![PCP];
        data.extend_from_slice(b"Q1W2");
        data.push(info.len() as u8);
        data.extend_from_slice(&info);
        let parsed = parse_ble_advertisement(&medium(true, &data)).unwrap();
        assert_eq!(parsed.id.as_str(), "Q1W2");
        assert_eq!(parsed.info.name, None);
    }

    #[test]
    fn wake_and_foreign_advertisements_are_ignored() {
        assert_eq!(parse_ble_advertisement(LEGACY_WAKE_DATA), None);
        assert_eq!(parse_ble_advertisement(&crate::ble_wake_service_data()), None);
        assert_eq!(parse_ble_advertisement(&[]), None);
        let info = EndpointInfo::visible("x", DeviceType::Phone).encode();
        let mut data = vec![PCP];
        data.extend_from_slice(&[1, 2, 3]); // another service's hash
        data.extend_from_slice(b"WXYZ");
        data.push(info.len() as u8);
        data.extend_from_slice(&info);
        data.extend([0; 6]);
        assert_eq!(parse_ble_advertisement(&medium(false, &data)), None);
    }
}
