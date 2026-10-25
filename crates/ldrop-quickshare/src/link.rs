// SPDX-License-Identifier: GPL-3.0-or-later
//! Mediums, and the hooks a caller provides so a connection can move to a faster one
//! (bandwidth upgrade, SPEC §5.4).
//!
//! This crate runs the upgrade negotiation; setting up the medium itself (a TCP listener, a
//! hotspot, joining a network) belongs to the caller's [`Upgrader`].

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::Error;
use crate::proto::location::nearby::connections::bandwidth_upgrade_negotiation_frame::UpgradePathInfo;
use crate::proto::location::nearby::connections::bandwidth_upgrade_negotiation_frame::upgrade_path_info::{
    self as path_info, WifiDirectCredentials, WifiHotspotCredentials, WifiLanSocket,
};
use crate::proto::location::nearby::connections::connection_request_frame::Medium as RequestMedium;

/// Any byte stream a channel can run over: TCP, RFCOMM, or an in-memory pipe in tests.
pub trait Stream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> Stream for T {}

pub type BoxStream = Box<dyn Stream>;
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
/// Keeps a medium set up (a listener, a hotspot, a joined network) while a channel uses it.
pub type Lease = Box<dyn Send>;

/// Payload chunk size on slow links: Nearby's default packet size.
const SLOW_CHUNK_LEN: usize = 64 * 1024;
/// File chunk size on Wi-Fi [R43].
const FAST_CHUNK_LEN: usize = 512 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Medium {
    Bluetooth,
    WifiLan,
    WifiHotspot,
    WifiDirect,
}

impl Medium {
    pub(crate) fn chunk_len(self) -> usize {
        match self {
            Medium::Bluetooth => SLOW_CHUNK_LEN,
            Medium::WifiLan | Medium::WifiHotspot | Medium::WifiDirect => FAST_CHUNK_LEN,
        }
    }

    /// The same numbering is used in connection requests and upgrade paths.
    pub(crate) fn to_proto(self) -> i32 {
        let m = match self {
            Medium::Bluetooth => RequestMedium::Bluetooth,
            Medium::WifiLan => RequestMedium::WifiLan,
            Medium::WifiHotspot => RequestMedium::WifiHotspot,
            Medium::WifiDirect => RequestMedium::WifiDirect,
        };
        m as i32
    }

    pub(crate) fn from_proto(value: i32) -> Option<Self> {
        match RequestMedium::try_from(value).ok()? {
            RequestMedium::Bluetooth => Some(Medium::Bluetooth),
            RequestMedium::WifiLan => Some(Medium::WifiLan),
            RequestMedium::WifiHotspot => Some(Medium::WifiHotspot),
            RequestMedium::WifiDirect => Some(Medium::WifiDirect),
            _ => None,
        }
    }

    /// Whether the new channel may drop the extra encryption layer. Nearby offers it for
    /// networks that are WPA2-protected with a password sent over the encrypted channel.
    pub(crate) fn may_disable_encryption(self) -> bool {
        matches!(self, Medium::WifiHotspot | Medium::WifiDirect)
    }
}

/// A Wi-Fi network one side hosts for the other: a hotspot or a Wi-Fi Direct group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetwork {
    pub ssid: String,
    pub password: String,
    /// TCP port the host listens on.
    pub port: u16,
    /// Host address; `None` means the network's gateway.
    pub gateway: Option<IpAddr>,
    /// Frequency in MHz, or a band; `None` if not given.
    pub frequency: Option<i32>,
}

/// Where to reach the host on the new medium.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpgradePath {
    WifiLan { addr: SocketAddr },
    WifiHotspot(WifiNetwork),
    WifiDirect(WifiNetwork),
}

impl UpgradePath {
    pub fn medium(&self) -> Medium {
        match self {
            UpgradePath::WifiLan { .. } => Medium::WifiLan,
            UpgradePath::WifiHotspot(_) => Medium::WifiHotspot,
            UpgradePath::WifiDirect(_) => Medium::WifiDirect,
        }
    }

    pub(crate) fn to_proto(&self) -> UpgradePathInfo {
        let medium = self.medium();
        let mut info = UpgradePathInfo {
            medium: Some(medium.to_proto()),
            supports_client_introduction_ack: Some(true),
            supports_disabling_encryption: medium.may_disable_encryption().then_some(true),
            ..Default::default()
        };
        let gateway = |net: &WifiNetwork| net.gateway.map(|ip| ip.to_string());
        match self {
            UpgradePath::WifiLan { addr } => {
                let ip = match addr.ip() {
                    IpAddr::V4(ip) => ip.octets().to_vec(),
                    IpAddr::V6(ip) => ip.octets().to_vec(),
                };
                info.wifi_lan_socket =
                    Some(WifiLanSocket { ip_address: Some(ip), wifi_port: Some(i32::from(addr.port())) });
            }
            UpgradePath::WifiHotspot(net) => {
                info.wifi_hotspot_credentials = Some(WifiHotspotCredentials {
                    ssid: Some(net.ssid.clone()),
                    password: Some(net.password.clone()),
                    port: Some(i32::from(net.port)),
                    gateway: gateway(net),
                    frequency: net.frequency,
                });
            }
            UpgradePath::WifiDirect(net) => {
                info.wifi_direct_credentials = Some(WifiDirectCredentials {
                    ssid: Some(net.ssid.clone()),
                    password: Some(net.password.clone()),
                    port: Some(i32::from(net.port)),
                    frequency: net.frequency,
                    gateway: gateway(net),
                    ip_v6_address: None,
                });
            }
        }
        info
    }

    /// `None` for mediums and credentials we cannot use.
    pub(crate) fn from_proto(info: &UpgradePathInfo) -> Option<Self> {
        let port = |p: Option<i32>| p.and_then(|p| u16::try_from(p).ok()).filter(|&p| p != 0);
        // "0.0.0.0" is the proto default and means "the gateway of the network".
        let gateway = |g: Option<&str>| g.and_then(|g| g.parse::<IpAddr>().ok()).filter(|ip| !ip.is_unspecified());
        let frequency = |f: Option<i32>| f.filter(|&f| f > 0);
        match info.medium() {
            path_info::Medium::WifiLan => {
                let socket = info.wifi_lan_socket.as_ref()?;
                let ip = match socket.ip_address() {
                    [a, b, c, d] => IpAddr::from([*a, *b, *c, *d]),
                    bytes => IpAddr::from(<[u8; 16]>::try_from(bytes).ok()?),
                };
                Some(UpgradePath::WifiLan { addr: SocketAddr::new(ip, port(socket.wifi_port)?) })
            }
            path_info::Medium::WifiHotspot => {
                let c = info.wifi_hotspot_credentials.as_ref()?;
                Some(UpgradePath::WifiHotspot(WifiNetwork {
                    ssid: c.ssid.clone().filter(|s| !s.is_empty())?,
                    password: c.password().to_owned(),
                    port: port(c.port)?,
                    gateway: gateway(c.gateway.as_deref()),
                    frequency: frequency(c.frequency),
                }))
            }
            path_info::Medium::WifiDirect => {
                let c = info.wifi_direct_credentials.as_ref()?;
                Some(UpgradePath::WifiDirect(WifiNetwork {
                    ssid: c.ssid.clone().filter(|s| !s.is_empty())?,
                    password: c.password().to_owned(),
                    port: port(c.port)?,
                    gateway: gateway(c.gateway.as_deref()),
                    frequency: frequency(c.frequency),
                }))
            }
            _ => None,
        }
    }
}

/// What a connection request says about the sender's links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerLinks {
    /// Mediums the peer can move to, best first.
    pub mediums: Vec<Medium>,
    /// The peer's address on its current Wi-Fi network, if it said.
    pub ip: Option<Ipv4Addr>,
    pub supports_5ghz: bool,
}

/// What our connection requests say about our links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LocalLinks {
    /// Mediums we can move to as the joining side, best first.
    pub mediums: Vec<Medium>,
    pub ip: Option<Ipv4Addr>,
    pub supports_5ghz: bool,
}

/// A medium set up by the host side, waiting for the peer.
pub struct Hosted {
    pub path: UpgradePath,
    /// Connections arriving on the new medium. The first one that introduces itself as the
    /// peer becomes the new channel.
    pub incoming: mpsc::Receiver<BoxStream>,
    pub lease: Lease,
}

/// A connection to the medium the peer hosts.
pub struct Joined {
    pub stream: BoxStream,
    pub lease: Lease,
}

/// Sets up mediums for the bandwidth upgrade. Leases are dropped when the channel ends or
/// the attempt is abandoned; dropping one tears its medium down.
pub trait Upgrader: Send + Sync {
    /// Advertised in our connection requests.
    fn local_links(&self) -> LocalLinks;
    /// Mediums to offer `peer`, best first. Empty keeps the connection where it is.
    fn hostable(&self, peer: &PeerLinks) -> Vec<Medium>;
    fn host(&self, medium: Medium) -> BoxFuture<Result<Hosted, Error>>;
    fn join(&self, path: UpgradePath) -> BoxFuture<Result<Joined, Error>>;
}

/// How a session's connection was made and where it may go.
#[derive(Clone)]
pub struct Link {
    /// What the stream runs over.
    pub medium: Medium,
    /// `None` keeps the connection on `medium` and declines upgrades.
    pub upgrader: Option<Arc<dyn Upgrader>>,
}

impl Link {
    /// A connection that stays where it is.
    pub fn fixed(medium: Medium) -> Self {
        Link { medium, upgrader: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip() {
        let paths = [
            UpgradePath::WifiLan { addr: "192.168.1.20:41234".parse().unwrap() },
            UpgradePath::WifiHotspot(WifiNetwork {
                ssid: "DIRECT-ab".into(),
                password: "secret123".into(),
                port: 5000,
                gateway: Some("192.168.49.1".parse().unwrap()),
                frequency: Some(2437),
            }),
            UpgradePath::WifiDirect(WifiNetwork {
                ssid: "DIRECT-cd".into(),
                password: "pw".into(),
                port: 6000,
                gateway: None,
                frequency: None,
            }),
        ];
        for path in paths {
            assert_eq!(UpgradePath::from_proto(&path.to_proto()).as_ref(), Some(&path));
        }
    }

    #[test]
    fn incomplete_paths_are_refused() {
        let mut info = UpgradePath::WifiLan { addr: "10.0.0.2:1".parse().unwrap() }.to_proto();
        info.wifi_lan_socket.as_mut().unwrap().ip_address = Some(vec![1, 2, 3]);
        assert_eq!(UpgradePath::from_proto(&info), None);
        let info = UpgradePathInfo { medium: Some(path_info::Medium::WifiHotspot as i32), ..Default::default() };
        assert_eq!(UpgradePath::from_proto(&info), None);
        let info = UpgradePathInfo { medium: Some(path_info::Medium::WebRtc as i32), ..Default::default() };
        assert_eq!(UpgradePath::from_proto(&info), None);
    }

    #[test]
    fn unspecified_gateway_means_none() {
        let mut info = UpgradePath::WifiHotspot(WifiNetwork {
            ssid: "x".into(),
            password: String::new(),
            port: 1,
            gateway: None,
            frequency: None,
        })
        .to_proto();
        info.wifi_hotspot_credentials.as_mut().unwrap().gateway = Some("0.0.0.0".into());
        let UpgradePath::WifiHotspot(net) = UpgradePath::from_proto(&info).unwrap() else { panic!() };
        assert_eq!(net.gateway, None);
        // The proto default frequency is -1.
        assert_eq!(net.frequency, None);
    }
}
