// SPDX-License-Identifier: GPL-3.0-or-later
//! mDNS for Quick Share on the LAN (SPEC §5.2): publish our service while visible, browse
//! while a discovery session runs. A thin async layer over `mdns-sd`, which runs its own
//! thread.

use std::collections::HashMap;
use std::net::{SocketAddr, SocketAddrV6};

use mdns_sd::{IfKind, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use rand::Rng;

pub use mdns_sd::Error;

/// One responder/browser per process.
#[derive(Clone)]
pub struct Mdns {
    daemon: ServiceDaemon,
}

impl Mdns {
    pub fn new() -> Result<Self, Error> {
        let daemon = ServiceDaemon::new()?;
        // Peers can never reach a loopback address.
        daemon.disable_interface(IfKind::LoopbackV4)?;
        daemon.disable_interface(IfKind::LoopbackV6)?;
        Ok(Mdns { daemon })
    }

    /// Publishes `instance` under `service_type` on every interface, with addresses kept up to
    /// date. Unpublished when the returned handle is dropped.
    pub fn publish(
        &self,
        service_type: &str,
        instance: &str,
        port: u16,
        txt: &[(&str, &str)],
    ) -> Result<Published, Error> {
        // A random host name per publication: the real one would identify the machine.
        let host = format!("ldrop-{:08x}.local.", rand::rng().random::<u32>());
        let info = ServiceInfo::new(service_type, instance, &host, "", port, txt)?.enable_addr_auto();
        let fullname = info.get_fullname().to_owned();
        self.daemon.register(info)?;
        tracing::debug!("mDNS: published {fullname} on port {port}");
        Ok(Published { daemon: self.daemon.clone(), fullname })
    }

    /// Browses `service_type` until the returned handle is dropped.
    pub fn browse(&self, service_type: &str) -> Result<Browser, Error> {
        let rx = self.daemon.browse(service_type)?;
        Ok(Browser { daemon: self.daemon.clone(), service_type: service_type.to_owned(), rx })
    }
}

pub struct Published {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Published {
    fn drop(&mut self) {
        // Sends goodbye packets; the status reply is not needed.
        if let Err(e) = self.daemon.unregister(&self.fullname) {
            tracing::warn!("mDNS: cannot unpublish {}: {e}", self.fullname);
        } else {
            tracing::debug!("mDNS: unpublished {}", self.fullname);
        }
    }
}

/// A resolved service instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    /// Instance label, unescaped.
    pub instance: String,
    /// Every address it announced, with the port. IPv6 link-local addresses carry their scope.
    pub addrs: Vec<SocketAddr>,
    /// TXT entries with values as UTF-8 (lossy); keys without a value map to "".
    pub txt: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Resolved(Service),
    Removed { instance: String },
}

pub struct Browser {
    daemon: ServiceDaemon,
    service_type: String,
    rx: mdns_sd::Receiver<ServiceEvent>,
}

impl Browser {
    /// The next resolution or removal; `None` once the daemon stops.
    pub async fn next(&mut self) -> Option<Event> {
        loop {
            match self.rx.recv_async().await.ok()? {
                ServiceEvent::ServiceResolved(service) => {
                    let Some(instance) = instance_label(service.get_fullname(), &self.service_type) else { continue };
                    let port = service.get_port();
                    let mut addrs: Vec<SocketAddr> =
                        service.get_addresses().iter().map(|ip| socket_addr(ip, port)).collect();
                    // IPv4 first: link-local IPv6 needs the right scope and fails more often.
                    addrs.sort_by_key(|a| (a.is_ipv6(), *a));
                    let txt = service
                        .get_properties()
                        .iter()
                        .map(|p| {
                            (
                                p.key().to_owned(),
                                p.val().map(|v| String::from_utf8_lossy(v).into_owned()).unwrap_or_default(),
                            )
                        })
                        .collect();
                    return Some(Event::Resolved(Service { instance, addrs, txt }));
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let Some(instance) = instance_label(&fullname, &self.service_type) else { continue };
                    return Some(Event::Removed { instance });
                }
                _ => {}
            }
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.daemon.stop_browse(&self.service_type);
    }
}

fn socket_addr(ip: &ScopedIp, port: u16) -> SocketAddr {
    match ip {
        ScopedIp::V6(v6) => SocketAddr::V6(SocketAddrV6::new(*v6.addr(), port, 0, v6.scope_id().index)),
        other => SocketAddr::new(other.to_ip_addr(), port),
    }
}

/// `instance._type._tcp.local.` → `instance`, matching the type without regard to case.
fn instance_label(fullname: &str, service_type: &str) -> Option<String> {
    let cut = fullname.len().checked_sub(service_type.len() + 1)?;
    if !fullname.is_char_boundary(cut)
        || !fullname[cut + 1..].eq_ignore_ascii_case(service_type)
        || fullname.as_bytes()[cut] != b'.'
    {
        return None;
    }
    let mut out = String::with_capacity(cut);
    let mut chars = fullname[..cut].chars();
    while let Some(c) = chars.next() {
        out.push(if c == '\\' { chars.next()? } else { c });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::*;

    const TY: &str = "_FC9F5ED42C8A._tcp.local.";

    #[test]
    fn instance_labels() {
        assert_eq!(instance_label("I0FCQ0T8n14AAA._FC9F5ED42C8A._tcp.local.", TY).as_deref(), Some("I0FCQ0T8n14AAA"));
        assert_eq!(instance_label("abc._fc9f5ed42c8a._tcp.local.", TY).as_deref(), Some("abc"));
        assert_eq!(instance_label(r"a\.b._FC9F5ED42C8A._tcp.local.", TY).as_deref(), Some("a.b"));
        assert_eq!(instance_label("abc._other._tcp.local.", TY), None);
        assert_eq!(instance_label("x", TY), None);
        assert_eq!(instance_label("abc_FC9F5ED42C8A._tcp.local.", TY), None);
    }

    #[test]
    fn scoped_addresses() {
        let v4 = ScopedIp::from(IpAddr::from([192, 168, 1, 2]));
        assert_eq!(socket_addr(&v4, 5000), "192.168.1.2:5000".parse().unwrap());
    }
}
