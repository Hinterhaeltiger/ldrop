// SPDX-License-Identifier: GPL-3.0-or-later
//! The BLE advert an Apple device sends while its AirDrop share sheet is open (Owly SPEC §2).
//! Receivers in Everyone mode bring their AWDL link up when they see it; ldrop does the same.

/// Apple's Bluetooth company ID.
pub const APPLE_COMPANY_ID: u16 = 0x004c;
/// The AirDrop TLV type in Apple's manufacturer data.
pub const AIRDROP_TLV: u8 = 0x05;
/// AD type of manufacturer-specific data.
pub const AD_MANUFACTURER_DATA: u8 = 0xff;

/// A BlueZ advertisement-monitor pattern, `(start, AD type, content)`, that matches an AirDrop
/// advert: the company ID, little-endian, then the AirDrop TLV type.
pub fn monitor_pattern() -> (u8, u8, Vec<u8>) {
    let [lo, hi] = APPLE_COMPANY_ID.to_le_bytes();
    (0, AD_MANUFACTURER_DATA, vec![lo, hi, AIRDROP_TLV])
}

/// Whether Apple manufacturer data (without the company ID, as BlueZ reports it) carries an
/// AirDrop TLV. Apple packs several TLVs into one advert.
pub fn is_airdrop(data: &[u8]) -> bool {
    let mut rest = data;
    while let [kind, len, tail @ ..] = rest {
        let len = usize::from(*len);
        if tail.len() < len {
            return false;
        }
        if *kind == AIRDROP_TLV {
            return true;
        }
        rest = &tail[len..];
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_the_airdrop_tlv() {
        let mut airdrop = vec![0x05, 0x12, 0x40, 1, 2, 3, 0, 0, 0, 0, 0x03];
        airdrop.extend([0; 9]);
        assert!(is_airdrop(&airdrop));
        // Nearby Info first, then AirDrop.
        let mut both = vec![0x10, 0x02, 0x0b, 0x1c];
        both.extend(&airdrop);
        assert!(is_airdrop(&both));
        assert!(!is_airdrop(&[0x10, 0x02, 0x0b, 0x1c]));
        assert!(!is_airdrop(&[0x05]));
        assert!(!is_airdrop(&[0x10, 0x09, 0x00]), "truncated");
        assert!(!is_airdrop(&[]));
        assert_eq!(monitor_pattern(), (0, 0xff, vec![0x4c, 0x00, 0x05]));
    }
}
