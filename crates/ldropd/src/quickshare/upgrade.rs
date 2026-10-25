// SPDX-License-Identifier: GPL-3.0-or-later
//! Moving a Bluetooth connection to Wi-Fi (bandwidth upgrade, SPEC §5.4).
//!
//! - **Wi-Fi LAN:** both on the same network. Hosting is a TCP listener; joining a TCP
//!   connection. Nothing changes on the computer.
//! - **Hotspot / Wi-Fi Direct:** the phone hosts a network and we join it, or we host a
//!   hotspot for the phone, through NetworkManager ([`ldrop_wifi::nm`]). The card leaves its
//!   current network meanwhile, so this needs `quickshare.allow_wifi_switch` unless the card
//!   is not connected anyway.

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ldrop_core::Medium as ConfigMedium;
use ldrop_quickshare::Error;
use ldrop_quickshare::link::{
    BoxFuture, BoxStream, Hosted, Joined, LocalLinks, Medium, PeerLinks, UpgradePath, Upgrader, WifiNetwork,
};
use ldrop_wifi::nm::{self, WifiDevice};
use nix::ifaddrs::getifaddrs;
use nix::net::if_::InterfaceFlags;
use rand::Rng;
use rand::distr::Alphanumeric;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::state::Shared;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A phone needs a moment after its network is up before it listens.
const CONNECT_ATTEMPTS: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(1);

/// One of our IPv4 addresses and its network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LanAddr {
    ip: Ipv4Addr,
    prefix: u8,
}

impl LanAddr {
    fn contains(&self, ip: Ipv4Addr) -> bool {
        let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
        u32::from(self.ip) & mask == u32::from(ip) & mask
    }
}

pub struct WifiUpgrader {
    lan: Vec<LanAddr>,
    /// The card, when hotspots and Wi-Fi Direct groups may be used.
    wifi: Option<WifiDevice>,
    join_hotspot: bool,
    join_wfd: bool,
    host_hotspot: bool,
    /// Port for hosted listeners; 0 picks one.
    port: u16,
    /// Our address on the peer's network, chosen in [`Upgrader::hostable`].
    lan_host: Mutex<Option<Ipv4Addr>>,
}

/// An upgrader for a session on Bluetooth, or `None` when nothing faster is possible.
pub async fn prepare(s: &Shared) -> Option<Arc<dyn Upgrader>> {
    let (uses, allow_switch, port) = {
        let st = s.state();
        let qs = &st.config.quickshare;
        let uses = |m: ConfigMedium| qs.enabled && qs.mediums.contains(&m);
        (
            [uses(ConfigMedium::Lan), uses(ConfigMedium::Hotspot), uses(ConfigMedium::Wfd)],
            qs.allow_wifi_switch,
            // The LAN listener's port + 1, so one firewall rule covers both.
            qs.tcp_port.checked_add(1).filter(|_| qs.tcp_port != 0).unwrap_or(0),
        )
    };
    let [lan_on, hotspot_on, wfd_on] = uses;
    let lan = if lan_on { lan_addrs() } else { Vec::new() };
    let wifi = if hotspot_on || wfd_on {
        match nm::wifi_device().await {
            Ok(Some(wifi)) if allow_switch || !wifi.connected => Some(wifi),
            Ok(Some(_)) => {
                tracing::debug!("Quick Share: not switching Wi-Fi networks (quickshare.allow_wifi_switch is off)");
                None
            }
            Ok(None) => None,
            Err(e) => {
                tracing::debug!("Quick Share: no hotspots: {e}");
                None
            }
        }
    } else {
        None
    };
    let upgrader = WifiUpgrader {
        join_hotspot: hotspot_on && wifi.is_some(),
        join_wfd: wfd_on && wifi.is_some(),
        host_hotspot: hotspot_on && wifi.as_ref().is_some_and(|w| w.can_host),
        lan,
        wifi,
        port,
        lan_host: Mutex::new(None),
    };
    if upgrader.lan.is_empty() && upgrader.wifi.is_none() {
        return None;
    }
    Some(Arc::new(upgrader))
}

/// IPv4 addresses on interfaces that are up, not loopback, and do multicast.
fn lan_addrs() -> Vec<LanAddr> {
    let wanted = InterfaceFlags::IFF_UP | InterfaceFlags::IFF_RUNNING;
    let Ok(addrs) = getifaddrs() else { return Vec::new() };
    addrs
        .filter(|ifa| ifa.flags.contains(wanted) && !ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK))
        .filter_map(|ifa| {
            let ip = ifa.address.as_ref()?.as_sockaddr_in()?.ip();
            let mask = ifa.netmask.as_ref()?.as_sockaddr_in()?.ip();
            Some(LanAddr { ip, prefix: u32::from(mask).count_ones() as u8 })
        })
        .collect()
}

impl Upgrader for WifiUpgrader {
    fn local_links(&self) -> LocalLinks {
        let mut mediums = Vec::new();
        if !self.lan.is_empty() {
            mediums.push(Medium::WifiLan);
        }
        if self.join_wfd {
            mediums.push(Medium::WifiDirect);
        }
        if self.join_hotspot {
            mediums.push(Medium::WifiHotspot);
        }
        LocalLinks { mediums, ip: self.lan.first().map(|a| a.ip), supports_5ghz: false }
    }

    fn hostable(&self, peer: &PeerLinks) -> Vec<Medium> {
        let mut mediums = Vec::new();
        let shared = peer.ip.and_then(|ip| self.lan.iter().find(|a| a.contains(ip)));
        if let Some(addr) = shared {
            *self.lan_host.lock().unwrap_or_else(|e| e.into_inner()) = Some(addr.ip);
            mediums.push(Medium::WifiLan);
        }
        if self.host_hotspot {
            mediums.push(Medium::WifiHotspot);
        }
        mediums
    }

    fn host(&self, medium: Medium) -> BoxFuture<Result<Hosted, Error>> {
        let lan_host = *self.lan_host.lock().unwrap_or_else(|e| e.into_inner());
        let wifi = self.wifi.clone();
        let port = self.port;
        Box::pin(async move {
            match medium {
                Medium::WifiLan => {
                    let ip = lan_host.ok_or_else(|| failed("no address on the peer's network"))?;
                    let (port, incoming, accepting) = listen(port).await?;
                    let path = UpgradePath::WifiLan { addr: SocketAddr::new(IpAddr::V4(ip), port) };
                    Ok(Hosted { path, incoming, lease: Box::new(accepting) })
                }
                Medium::WifiHotspot => {
                    let wifi = wifi.ok_or_else(|| failed("no Wi-Fi card to host on"))?;
                    let (ssid, password) = hotspot_credentials();
                    tracing::info!("Quick Share: hosting the hotspot {ssid} on {}", wifi.interface);
                    let session = wifi.host(&ssid, &password).await.map_err(failed)?;
                    let (port, incoming, accepting) = listen(port).await?;
                    let path = UpgradePath::WifiHotspot(WifiNetwork {
                        ssid,
                        password,
                        port,
                        gateway: Some(IpAddr::V4(session.address)),
                        frequency: None,
                    });
                    Ok(Hosted { path, incoming, lease: Box::new((accepting, session)) })
                }
                Medium::WifiDirect | Medium::Bluetooth => Err(failed("cannot host this medium")),
            }
        })
    }

    fn join(&self, path: UpgradePath) -> BoxFuture<Result<Joined, Error>> {
        let wifi = self.wifi.clone();
        Box::pin(async move {
            match path {
                UpgradePath::WifiLan { addr } => {
                    let stream = connect(addr).await?;
                    Ok(Joined { stream, lease: Box::new(()) })
                }
                UpgradePath::WifiHotspot(net) | UpgradePath::WifiDirect(net) => {
                    let wifi = wifi.ok_or_else(|| failed("not allowed to switch Wi-Fi networks"))?;
                    tracing::info!("Quick Share: joining {} on {}", net.ssid, wifi.interface);
                    let session = wifi.join(&net.ssid, &net.password).await.map_err(failed)?;
                    let host = net
                        .gateway
                        .or(session.gateway.map(IpAddr::V4))
                        .ok_or_else(|| failed("the network has no gateway to connect to"))?;
                    let stream = connect(SocketAddr::new(host, net.port)).await?;
                    Ok(Joined { stream, lease: Box::new(session) })
                }
            }
        })
    }
}

fn failed(why: impl ToString) -> Error {
    Error::Io(io::Error::other(why.to_string()))
}

/// A random hotspot name and WPA2 passphrase, the way Nearby makes them.
fn hotspot_credentials() -> (String, String) {
    let mut rng = rand::rng();
    let mut text = |len: usize| -> String { (&mut rng).sample_iter(Alphanumeric).take(len).map(char::from).collect() };
    (format!("DIRECT-{}-ldrop", text(2)), text(16))
}

/// Stops accepting when dropped.
struct Accepting(JoinHandle<()>);

impl Drop for Accepting {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Listens on all addresses, on `port` if it is free, else on any.
async fn listen(port: u16) -> Result<(u16, mpsc::Receiver<BoxStream>, Accepting), Error> {
    let listener = match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await {
        Ok(l) => l,
        Err(_) if port != 0 => TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).await?,
        Err(e) => return Err(e.into()),
    };
    let port = listener.local_addr()?.port();
    let (tx, rx) = mpsc::channel(4);
    let task = tokio::spawn(async move {
        while let Ok((stream, addr)) = listener.accept().await {
            tracing::debug!("Quick Share: upgrade connection from {addr}");
            let _ = stream.set_nodelay(true);
            if tx.send(Box::new(stream) as BoxStream).await.is_err() {
                return;
            }
        }
    });
    Ok((port, rx, Accepting(task)))
}

async fn connect(addr: SocketAddr) -> Result<BoxStream, Error> {
    let mut last = failed("not tried");
    for attempt in 0..CONNECT_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(RETRY_DELAY).await;
        }
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(Box::new(stream));
            }
            Ok(Err(e)) => last = e.into(),
            Err(_) => last = failed(format!("{addr} timed out")),
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnets() {
        let a = LanAddr { ip: Ipv4Addr::new(192, 168, 1, 20), prefix: 24 };
        assert!(a.contains(Ipv4Addr::new(192, 168, 1, 99)));
        assert!(!a.contains(Ipv4Addr::new(192, 168, 2, 99)));
        assert!(LanAddr { ip: Ipv4Addr::new(10, 0, 0, 1), prefix: 0 }.contains(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!LanAddr { ip: Ipv4Addr::new(10, 0, 0, 1), prefix: 32 }.contains(Ipv4Addr::new(10, 0, 0, 2)));
    }

    #[test]
    fn hotspot_credentials_look_like_nearby() {
        let (ssid, password) = hotspot_credentials();
        assert!(ssid.starts_with("DIRECT-") && ssid.len() == "DIRECT-ab-ldrop".len());
        assert_eq!(password.len(), 16);
    }

    fn upgrader(lan: Vec<LanAddr>) -> WifiUpgrader {
        WifiUpgrader {
            lan,
            wifi: None,
            join_hotspot: false,
            join_wfd: false,
            host_hotspot: false,
            port: 0,
            lan_host: Mutex::new(None),
        }
    }

    #[test]
    fn lan_is_offered_only_on_a_shared_network() {
        let u = upgrader(vec![LanAddr { ip: Ipv4Addr::new(192, 168, 1, 20), prefix: 24 }]);
        assert_eq!(u.local_links().mediums, [Medium::WifiLan]);
        let peer =
            |ip: [u8; 4]| PeerLinks { mediums: vec![Medium::WifiLan], ip: Some(ip.into()), ..Default::default() };
        assert!(u.hostable(&peer([10, 0, 0, 5])).is_empty());
        assert_eq!(u.hostable(&peer([192, 168, 1, 7])), [Medium::WifiLan]);
        assert_eq!(*u.lan_host.lock().unwrap(), Some(Ipv4Addr::new(192, 168, 1, 20)));
    }

    #[tokio::test]
    async fn hosting_and_joining_on_the_lan() {
        let u = upgrader(vec![LanAddr { ip: Ipv4Addr::LOCALHOST, prefix: 8 }]);
        let peer = PeerLinks { mediums: vec![Medium::WifiLan], ip: Some(Ipv4Addr::LOCALHOST), ..Default::default() };
        assert_eq!(u.hostable(&peer), [Medium::WifiLan]);
        let Hosted { path, mut incoming, lease } = u.host(Medium::WifiLan).await.unwrap();
        let joined = u.join(path).await.unwrap();
        assert!(incoming.recv().await.is_some());
        drop((joined, lease));
    }
}
