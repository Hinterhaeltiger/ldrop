// SPDX-License-Identifier: GPL-3.0-or-later
//! Endpoint identity as peers see it (SPEC §5.1, §5.2): endpoint ID, the endpoint-info blob
//! sent in connection requests and in the mDNS TXT record, and the mDNS instance name.
//!
//! Layout from NearDrop PROTOCOL.md [R43]; rquickshare `utils.rs` [R42] encodes the same.

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;

use crate::Error;

/// mDNS service type: `_` + hex of the first 6 bytes of SHA-256("NearbySharing") (SPEC §5.2).
pub const MDNS_SERVICE_TYPE: &str = "_FC9F5ED42C8A._tcp.local.";

/// First 3 bytes of SHA-256("NearbySharing").
const SERVICE_ID_HASH: [u8; 3] = [0xFC, 0x9F, 0x5E];
/// First byte of the mDNS instance name ("PCP" in Google's code).
const PCP: u8 = 0x23;
/// Salt + encrypted metadata key: meaningless without Google's servers, random is accepted.
const OPAQUE_LEN: usize = 16;
const TLV_QR_CODE: u8 = 1;
const TLV_VENDOR: u8 = 2;
pub const VENDOR_SAMSUNG: u8 = 1;
/// Raw endpoint-info bytes that fit one TXT string: base64 of 189 bytes is 252 characters.
const MAX_TXT_INFO_LEN: usize = 189;

/// 4 random alphanumeric ASCII characters, new for each visibility session (SPEC §14).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct EndpointId([u8; 4]);

impl EndpointId {
    pub fn random() -> Self {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
        let mut rng = rand::rng();
        EndpointId(std::array::from_fn(|_| CHARS[rng.random_range(0..CHARS.len())]))
    }

    /// Accepts any printable ASCII: peers are not bound to alphanumerics.
    pub fn from_bytes(bytes: [u8; 4]) -> Option<Self> {
        bytes.iter().all(|b| b.is_ascii_graphic()).then_some(EndpointId(bytes))
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::from_bytes(s.as_bytes().try_into().ok()?)
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("endpoint ids are ASCII")
    }
}

impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EndpointId({})", self.as_str())
    }
}

/// Device type in endpoint info; Android picks the icon from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Unknown = 0,
    Phone = 1,
    Tablet = 2,
    Laptop = 3,
    Car = 4,
    Foldable = 5,
    Xr = 6,
}

impl DeviceType {
    fn from_bits(bits: u8) -> Self {
        match bits {
            1 => DeviceType::Phone,
            2 => DeviceType::Tablet,
            3 => DeviceType::Laptop,
            4 => DeviceType::Car,
            5 => DeviceType::Foldable,
            6 => DeviceType::Xr,
            _ => DeviceType::Unknown,
        }
    }
}

/// The endpoint-info blob (SPEC §5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    pub device_type: DeviceType,
    /// `None` when the device advertises itself as hidden (only reachable by QR code).
    pub name: Option<String>,
    /// TLV 1: advertising token or encrypted name (QR mode, SPEC §5.6).
    pub qr_data: Option<Vec<u8>>,
    /// TLV 2: vendor ID (1 = Samsung).
    pub vendor: Option<u8>,
}

impl EndpointInfo {
    pub fn visible(name: &str, device_type: DeviceType) -> Self {
        EndpointInfo { device_type, name: Some(name.to_owned()), qr_data: None, vendor: None }
    }

    /// Byte 0 is version (3 bits, 0) | hidden (1 bit) | device type (3 bits) | reserved (1 bit),
    /// MSB first. Names are cut to 255 bytes at a character boundary.
    pub fn encode(&self) -> Vec<u8> {
        let hidden = u8::from(self.name.is_none());
        let mut out = vec![(hidden << 4) | ((self.device_type as u8) << 1)];
        out.extend(rand::rng().random::<[u8; OPAQUE_LEN]>());
        if let Some(name) = &self.name {
            let name = truncate_utf8(name, 255);
            out.push(name.len() as u8);
            out.extend_from_slice(name.as_bytes());
        }
        if let Some(qr) = &self.qr_data {
            let qr = &qr[..qr.len().min(255)];
            out.extend([TLV_QR_CODE, qr.len() as u8]);
            out.extend_from_slice(qr);
        }
        if let Some(vendor) = self.vendor {
            out.extend([TLV_VENDOR, 1, vendor]);
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let short = || Error::protocol("endpoint info too short");
        let first = *bytes.first().ok_or_else(short)?;
        let mut rest = bytes.get(1 + OPAQUE_LEN..).ok_or_else(short)?;
        let hidden = first & 0x10 != 0;
        let name = if hidden {
            None
        } else {
            let (&len, tail) = rest.split_first().ok_or_else(short)?;
            let raw = tail.get(..usize::from(len)).ok_or_else(short)?;
            rest = &tail[usize::from(len)..];
            Some(String::from_utf8_lossy(raw).into_owned())
        };
        let mut info =
            EndpointInfo { device_type: DeviceType::from_bits((first >> 1) & 0x07), name, qr_data: None, vendor: None };
        // Trailing TLVs; a truncated record ends parsing, as in NearDrop.
        while let [kind, len, tail @ ..] = rest {
            let Some(value) = tail.get(..usize::from(*len)) else { break };
            match *kind {
                TLV_QR_CODE => info.qr_data = Some(value.to_vec()),
                TLV_VENDOR => info.vendor = value.first().copied(),
                _ => {}
            }
            rest = &tail[usize::from(*len)..];
        }
        Ok(info)
    }

    /// Value of the mDNS TXT key `n`. A TXT string holds 255 bytes including `n=`, so long
    /// names are cut shorter here than in connection requests.
    pub fn to_txt(&self) -> String {
        let mut info = self.clone();
        if let Some(name) = &mut info.name {
            let fixed = EndpointInfo { name: Some(String::new()), ..self.clone() }.encode().len();
            *name = truncate_utf8(name, MAX_TXT_INFO_LEN.saturating_sub(fixed)).to_owned();
        }
        URL_SAFE_NO_PAD.encode(info.encode())
    }

    pub fn from_txt(value: &str) -> Result<Self, Error> {
        let bytes = URL_SAFE_NO_PAD
            .decode(value.trim_end_matches('='))
            .map_err(|_| Error::protocol("endpoint info is not base64"))?;
        Self::decode(&bytes)
    }
}

/// mDNS instance name: URL-safe base64 of `0x23 | endpoint id | FC 9F 5E | 00 00`.
pub fn mdns_instance_name(id: EndpointId) -> String {
    let mut raw = vec![PCP];
    raw.extend_from_slice(&id.0);
    raw.extend_from_slice(&SERVICE_ID_HASH);
    raw.extend([0, 0]);
    URL_SAFE_NO_PAD.encode(raw)
}

/// The endpoint ID in a Quick Share mDNS instance name, `None` for anything else.
pub fn parse_mdns_instance_name(name: &str) -> Option<EndpointId> {
    let raw = URL_SAFE_NO_PAD.decode(name.trim_end_matches('=')).ok()?;
    if raw.len() < 8 || raw[0] != PCP || raw[5..8] != SERVICE_ID_HASH {
        return None;
    }
    EndpointId::from_bytes(raw[1..5].try_into().ok()?)
}

fn truncate_utf8(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_info_round_trip() {
        let info = EndpointInfo { vendor: Some(VENDOR_SAMSUNG), ..EndpointInfo::visible("Pixel 9", DeviceType::Phone) };
        let bytes = info.encode();
        assert_eq!(bytes[0], 0b0000_0010);
        assert_eq!(bytes[17], 7);
        assert_eq!(&bytes[18..25], b"Pixel 9");
        assert_eq!(EndpointInfo::decode(&bytes).unwrap(), info);
        assert_eq!(EndpointInfo::from_txt(&info.to_txt()).unwrap(), info);
    }

    #[test]
    fn laptop_type_matches_neardrop_and_rquickshare() {
        // Both encode `device_type << 1` with version 0 and the visible bit clear.
        assert_eq!(EndpointInfo::visible("x", DeviceType::Laptop).encode()[0], 3 << 1);
    }

    #[test]
    fn hidden_endpoint_with_qr_tlv() {
        let info =
            EndpointInfo { device_type: DeviceType::Phone, name: None, qr_data: Some(vec![9; 16]), vendor: None };
        let bytes = info.encode();
        assert_eq!(bytes[0] & 0x10, 0x10);
        assert_eq!(bytes.len(), 17 + 2 + 16);
        assert_eq!(EndpointInfo::decode(&bytes).unwrap(), info);
    }

    #[test]
    fn rejects_truncated_endpoint_info() {
        assert!(EndpointInfo::decode(&[]).is_err());
        assert!(EndpointInfo::decode(&[0; 17]).is_err());
        let mut bytes = EndpointInfo::visible("Pixel", DeviceType::Phone).encode();
        bytes.truncate(20);
        assert!(EndpointInfo::decode(&bytes).is_err());
        // A truncated trailing TLV is ignored, not an error.
        let mut bytes = EndpointInfo::visible("P", DeviceType::Phone).encode();
        bytes.extend([TLV_VENDOR, 5, 1]);
        assert_eq!(EndpointInfo::decode(&bytes).unwrap().vendor, None);
    }

    #[test]
    fn long_names_are_cut_at_char_boundaries() {
        let name = "ä".repeat(200);
        let bytes = EndpointInfo::visible(&name, DeviceType::Laptop).encode();
        assert_eq!(bytes[17], 254);
        assert_eq!(EndpointInfo::decode(&bytes).unwrap().name.unwrap(), "ä".repeat(127));
    }

    #[test]
    fn txt_value_fits_one_txt_string() {
        let info = EndpointInfo::visible(&"ä".repeat(200), DeviceType::Laptop);
        let txt = info.to_txt();
        assert!("n=".len() + txt.len() <= 255, "{}", txt.len());
        let name = EndpointInfo::from_txt(&txt).unwrap().name.unwrap();
        assert!(name.chars().all(|c| c == 'ä') && name.len() >= 160);
        let short = EndpointInfo::visible("Pixel", DeviceType::Phone);
        assert_eq!(EndpointInfo::from_txt(&short.to_txt()).unwrap(), short);
    }

    #[test]
    fn instance_names() {
        let id = EndpointId::parse("AB12").unwrap();
        let name = mdns_instance_name(id);
        // 10 bytes → 14 base64 characters without padding.
        assert_eq!(name.len(), 14);
        assert_eq!(parse_mdns_instance_name(&name), Some(id));
        assert_eq!(parse_mdns_instance_name("not-quick-share"), None);
        assert_eq!(parse_mdns_instance_name(&URL_SAFE_NO_PAD.encode([0x23, b'A', b'B', b'C', b'D', 1, 2, 3])), None);
        assert!(EndpointId::random().as_str().bytes().all(|b| b.is_ascii_alphanumeric()));
    }
}
