// SPDX-License-Identifier: GPL-3.0-or-later
//! Addresses on the AWDL link. Peers there use the link-local address made from their AWDL MAC
//! (modified EUI-64), which is how a sender reaches a receiver whose mDNS it never heard.

use std::net::{Ipv6Addr, SocketAddrV6};

/// Port an Apple receiver serves `/Discover`, `/Ask` and `/Upload` on.
pub const APPLE_PORT: u16 = 8770;
/// Port ldrop's receiver listens on, like Owly's: senders take it from the mDNS SRV record.
/// The firewall must let it in on the AWDL interface.
pub const RECEIVER_PORT: u16 = 8771;

/// Parses `aa:bb:cc:dd:ee:ff` (either case, `-` also allowed).
pub fn parse_mac(text: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = text.split([':', '-']);
    for byte in &mut mac {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *byte = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// `fe80::` plus the modified EUI-64 interface ID of `mac` (RFC 4291 appendix A).
pub fn link_local(mac: [u8; 6]) -> Ipv6Addr {
    let [a, b, c, d, e, f] = mac;
    Ipv6Addr::new(
        0xfe80,
        0,
        0,
        0,
        u16::from_be_bytes([a ^ 0x02, b]),
        u16::from_be_bytes([c, 0xff]),
        u16::from_be_bytes([0xfe, d]),
        u16::from_be_bytes([e, f]),
    )
}

/// The socket address of the peer with `mac` on the interface with index `scope`.
pub fn peer_socket(mac: [u8; 6], port: u16, scope: u32) -> SocketAddrV6 {
    SocketAddrV6::new(link_local(mac), port, 0, scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parsing() {
        assert_eq!(parse_mac("02:1A:2b:3c:4d:5e"), Some([0x02, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e]));
        assert_eq!(parse_mac("02-1a-2b-3c-4d-5e"), Some([0x02, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e]));
        assert_eq!(parse_mac("02:1a:2b:3c:4d"), None);
        assert_eq!(parse_mac("02:1a:2b:3c:4d:5e:6f"), None);
        assert_eq!(parse_mac("2:1a:2b:3c:4d:5e"), None);
        assert_eq!(parse_mac("zz:1a:2b:3c:4d:5e"), None);
    }

    #[test]
    fn eui64() {
        let mac = [0x02, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e];
        assert_eq!(link_local(mac), "fe80::1a:2bff:fe3c:4d5e".parse::<Ipv6Addr>().unwrap());
        assert_eq!(link_local([0; 6]), "fe80::200:ff:fe00:0".parse::<Ipv6Addr>().unwrap());
        let sock = peer_socket(mac, APPLE_PORT, 7);
        assert_eq!((sock.port(), sock.scope_id()), (8770, 7));
    }
}
