// SPDX-License-Identifier: GPL-3.0-or-later
//! Apple model identifiers (`SenderModelName`, `ReceiverModelName`) as device kinds.

use ldrop_core::{DeviceKind, Platform};

/// The kind of device behind a model identifier such as `iPhone15,2` or `MacBookPro18,3`.
pub fn kind(model: &str) -> DeviceKind {
    let family = model.trim_end_matches(|c: char| c.is_ascii_digit() || c == ',');
    match family {
        "iPhone" | "iPod" => DeviceKind::Phone,
        "iPad" => DeviceKind::Tablet,
        "MacBook" | "MacBookAir" | "MacBookPro" => DeviceKind::Laptop,
        "iMac" | "iMacPro" | "Macmini" | "MacPro" => DeviceKind::Desktop,
        // Newer Macs are all `MacNN,M`, laptops and desktops alike; most that AirDrop are
        // MacBooks. Linux senders (Owly, ldrop) say `Linux`.
        "Mac" | "Linux" => DeviceKind::Laptop,
        "RealityDevice" => DeviceKind::Xr,
        _ => DeviceKind::Unknown,
    }
}

/// A device name from the network as it can be shown: no control characters, no surrounding
/// space, at most [`MAX_NAME`] characters.
pub fn display_name(name: &str) -> String {
    let clean: String = name.chars().filter(|c| !c.is_control()).collect();
    clean.trim().chars().take(MAX_NAME).collect::<String>().trim_end().to_owned()
}

/// What the daemon accepts as a device name, too.
pub const MAX_NAME: usize = 100;

/// Apple, unless the peer is an AirDrop implementation on Linux.
pub fn platform(model: &str) -> Platform {
    if model == "Linux" { Platform::Linux } else { Platform::Apple }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(kind("iPhone15,2"), DeviceKind::Phone);
        assert_eq!(kind("iPad13,1"), DeviceKind::Tablet);
        assert_eq!(kind("MacBookPro18,3"), DeviceKind::Laptop);
        assert_eq!(kind("Mac14,2"), DeviceKind::Laptop);
        assert_eq!(kind("iMac21,1"), DeviceKind::Desktop);
        assert_eq!(kind("Macmini9,1"), DeviceKind::Desktop);
        assert_eq!(kind("RealityDevice14,1"), DeviceKind::Xr);
        assert_eq!(kind("AppleTV14,1"), DeviceKind::Unknown);
        assert_eq!(kind(""), DeviceKind::Unknown);
        assert_eq!(platform("Linux"), Platform::Linux);
        assert_eq!(platform("iPhone15,2"), Platform::Apple);
    }

    #[test]
    fn names() {
        assert_eq!(display_name("  Anna\u{0}'s\n iPhone \t"), "Anna's iPhone");
        assert_eq!(display_name("\u{7}"), "");
        assert_eq!(display_name(&"x".repeat(150)).len(), MAX_NAME);
    }
}
