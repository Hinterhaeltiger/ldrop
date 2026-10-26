// SPDX-License-Identifier: GPL-3.0-or-later
//! The AWDL link for AirDrop, leased from Owly's radio service `owlyd` on the system bus
//! (SPEC §6, ADR-002). owlyd brings `awdl0` up while a lease is held and puts Wi-Fi back when
//! the last lease ends, also when the holder's bus connection closes, so a crashed ldropd cannot
//! leave the card in AWDL mode. Acquiring is guarded by polkit (`io.github.t4t5.Owly.acquire`,
//! allowed for active local sessions).

use std::collections::HashMap;
use std::net::Ipv6Addr;
use std::time::Duration;

use futures_util::StreamExt;
use ldrop_core::capabilities::OwlyReport;
use serde::Deserialize;
use zbus::proxy::CacheProperties;
use zbus::{Connection, OwnedVariant, proxy};

pub const SERVICE: &str = "io.github.t4t5.Owly";
const PATH: &str = "/io/github/t4t5/Owly";

#[proxy(
    interface = "io.github.t4t5.Owly.Radio1",
    default_service = "io.github.t4t5.Owly",
    default_path = "/io/github/t4t5/Owly"
)]
trait Radio1 {
    /// The capability report as JSON.
    fn probe(&self) -> zbus::Result<String>;
    /// Brings the link up for `duration` seconds, 0 = until released.
    fn acquire(&self, duration: u32) -> zbus::Result<(u64, HashMap<String, OwnedVariant>)>;
    fn release(&self, lease: u64) -> zbus::Result<()>;
    /// `idle`, `starting`, `up` or `stopping`.
    #[zbus(property, name = "State")]
    fn link_state(&self) -> zbus::Result<String>;
    /// The AWDL peers in range: `mac` (s) and `rssi` (n).
    #[zbus(property)]
    fn peers(&self) -> zbus::Result<Vec<HashMap<String, OwnedVariant>>>;
    #[zbus(signal)]
    fn state_changed(&self, lease: u64, state: String, reason: String) -> zbus::Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("Owly's radio service (owlyd) is not installed")]
    NotInstalled,
    #[error("not allowed to use the Wi-Fi card for AirDrop: {0}")]
    NotAuthorized(String),
    #[error("the Wi-Fi card is busy: {0}")]
    Busy(String),
    /// Trying again later may work (Wi-Fi isn't connected, say).
    #[error("{0}")]
    NotNow(String),
    #[error("{0}")]
    NoRadio(String),
    #[error("owlyd: {0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<zbus::Error> for Error {
    fn from(e: zbus::Error) -> Self {
        let (name, detail) = match &e {
            zbus::Error::MethodError(name, detail, _) => (name.to_string(), detail.clone().unwrap_or_default()),
            zbus::Error::FDO(fdo) => (zbus::DBusError::name(fdo.as_ref()).to_string(), fdo.to_string()),
            _ => return Error::Other(e.to_string()),
        };
        match name.rsplit('.').next().unwrap_or_default() {
            "ServiceUnknown" | "NameHasNoOwner" | "ServiceNotFound" => Error::NotInstalled,
            "NotAuthorized" | "AccessDenied" | "AuthFailed" | "InteractiveAuthorizationRequired" => {
                Error::NotAuthorized(detail)
            }
            "Busy" => Error::Busy(detail),
            "NotNow" => Error::NotNow(detail),
            "NoRadio" => Error::NoRadio(detail),
            _ => Error::Other(if detail.is_empty() { name } else { detail }),
        }
    }
}

/// The link while a lease holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// `awdl0`; only `index` matters to sockets.
    pub interface: String,
    pub index: u32,
    pub mac: String,
    /// Our link-local address on it.
    pub address: Ipv6Addr,
    /// How a receiver repeats its mDNS announcement, when the radio does not carry the records
    /// itself.
    pub announce: Option<Announce>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Announce {
    pub count: u32,
    /// Between copies.
    pub interval: Duration,
    /// From the end of one burst to the start of the next.
    pub pause: Duration,
}

impl Link {
    fn from_dict(d: &HashMap<String, OwnedVariant>) -> Result<Link> {
        let bad = |key: &str| Error::Other(format!("the link has no valid `{key}`"));
        let text =
            |key: &str| d.get(key).and_then(|v| String::try_from(v.try_clone().ok()?).ok()).ok_or_else(|| bad(key));
        let index = d.get("index").and_then(|v| u32::try_from(v).ok()).ok_or_else(|| bad("index"))?;
        let address = text("address")?.parse().map_err(|_| bad("address"))?;
        let announce = d.get("announce").and_then(|v| <(u32, u32, u32)>::try_from(v.try_clone().ok()?).ok()).map(
            |(count, interval, pause)| Announce {
                count,
                interval: Duration::from_micros(interval.into()),
                pause: Duration::from_micros(pause.into()),
            },
        );
        Ok(Link { interface: text("interface")?, index, mac: text("mac")?, address, announce })
    }
}

/// An AWDL peer in range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub mac: String,
    pub rssi: i16,
}

/// A client of owlyd.
#[derive(Clone)]
pub struct Owly {
    proxy: Radio1Proxy<'static>,
}

impl Owly {
    /// owlyd on the system bus. Does not start it; the first call does (D-Bus activation).
    pub async fn system() -> Result<Owly> {
        let conn = Connection::system().await.map_err(|e| Error::Other(format!("system bus: {e}")))?;
        Self::on(&conn).await
    }

    /// owlyd, or a stand-in, on `conn`.
    pub async fn on(conn: &Connection) -> Result<Owly> {
        let proxy = Radio1Proxy::builder(conn)
            .destination(SERVICE)
            .path(PATH)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        Ok(Owly { proxy })
    }

    pub async fn probe(&self) -> Result<Probe> {
        let json = self.proxy.probe().await?;
        serde_json::from_str(&json).map_err(|e| Error::Other(format!("cannot parse the probe report: {e}")))
    }

    /// Brings the link up until the lease is released or dropped.
    pub async fn acquire(&self) -> Result<Lease> {
        // Subscribed first so the end of a short-lived link is not missed.
        let changes = self.proxy.receive_state_changed().await?;
        let (id, link) = self.proxy.acquire(0).await?;
        let link = Link::from_dict(&link)?;
        Ok(Lease { proxy: self.proxy.clone(), id, link, changes: Some(changes), released: false })
    }

    pub async fn peers(&self) -> Result<Vec<Peer>> {
        let peers = self.proxy.peers().await?;
        Ok(peers
            .iter()
            .filter_map(|p| {
                let mac = String::try_from(p.get("mac")?.try_clone().ok()?).ok()?;
                let rssi = p.get("rssi").and_then(|v| i16::try_from(v).ok()).unwrap_or(0);
                Some(Peer { mac, rssi })
            })
            .collect())
    }
}

/// The link, held until released or dropped.
pub struct Lease {
    proxy: Radio1Proxy<'static>,
    id: u64,
    pub link: Link,
    changes: Option<StateChangedStream>,
    released: bool,
}

impl Lease {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Resolves when owlyd ends the lease on its own (the link was lost, or Wi-Fi went away),
    /// with its reason.
    pub async fn ended(&mut self) -> String {
        let Some(changes) = &mut self.changes else { return std::future::pending().await };
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            if *args.lease() == self.id && matches!(args.state().as_str(), "stopping" | "idle") {
                self.released = true;
                return args.reason().clone();
            }
        }
        // owlyd left the bus, which ends every lease.
        self.released = true;
        "owlyd stopped".into()
    }

    pub async fn release(mut self) {
        self.released = true;
        if let Err(e) = self.proxy.release(self.id).await {
            tracing::debug!("owlyd: releasing lease {}: {e}", self.id);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let (proxy, id) = (self.proxy.clone(), self.id);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = proxy.release(id).await;
            });
        }
    }
}

/// owlyd's capability report (`owly_radio::Capabilities`), leniently: fields it may add later
/// are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Probe {
    pub station: Option<String>,
    pub driver: Option<String>,
    pub device: Option<String>,
    /// `tested`, `untested` or `broken`.
    pub status: Option<String>,
    /// `shared`, `exclusive` or `active-monitor`.
    pub mode: Option<String>,
    pub channel: Option<u32>,
    pub hardware: bool,
    pub missing: Vec<Missing>,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Missing {
    /// `hardware`, `backend` or `radio`.
    pub id: String,
    pub say: String,
}

/// What `ldrop doctor` reports about owlyd: its probe, or why there is none.
pub async fn report() -> OwlyReport {
    let probe = match Owly::system().await {
        Ok(owly) => owly.probe().await,
        Err(e) => Err(e),
    };
    match probe {
        Ok(p) => OwlyReport {
            present: true,
            error: None,
            station: p.station,
            driver: p.driver,
            device: p.device,
            status: p.status,
            mode: p.mode,
            channel: p.channel,
            hardware: p.hardware,
            missing: p.missing.into_iter().map(|m| (m.id, m.say)).collect(),
            issues: p.issues,
        },
        Err(Error::NotInstalled) => OwlyReport::default(),
        Err(e) => OwlyReport { error: Some(e.to_string()), ..OwlyReport::default() },
    }
}

#[cfg(test)]
mod tests {
    use zbus::{Structure, Variant};

    use super::*;

    #[test]
    fn probe_json() {
        let json = r#"{"station":"wlp1s0","driver":"rtw89_8852ae","device":"10ec:8852","status":"untested",
            "mode":"shared","channel":6,"card":{"phy":"phy0"},"hardware":true,
            "missing":[{"id":"radio","say":"Set allow_untested."}],"issues":[],"future":1}"#;
        let p: Probe = serde_json::from_str(json).unwrap();
        assert_eq!(p.mode.as_deref(), Some("shared"));
        assert_eq!(p.channel, Some(6));
        assert_eq!(p.missing, [Missing { id: "radio".into(), say: "Set allow_untested.".into() }]);
        assert!(serde_json::from_str::<Probe>("{}").is_ok());
    }

    #[test]
    fn link_dict() {
        let mut d = HashMap::new();
        let v = |v: Variant<'static>| OwnedVariant::try_from(v).unwrap();
        d.insert("interface".into(), v("awdl0".into()));
        d.insert("index".into(), v(7u32.into()));
        d.insert("mac".into(), v("02:1a:2b:3c:4d:5e".into()));
        d.insert("address".into(), v("fe80::1a:2bff:fe3c:4d5e".into()));
        let link = Link::from_dict(&d).unwrap();
        assert_eq!((link.interface.as_str(), link.index, link.announce), ("awdl0", 7, None));
        d.insert("announce".into(), v(Variant::from(Structure::from((3u32, 50_000u32, 3_000_000u32)))));
        let announce = Link::from_dict(&d).unwrap().announce.unwrap();
        assert_eq!((announce.count, announce.interval, announce.pause.as_secs()), (3, Duration::from_millis(50), 3));
        d.remove("index");
        assert!(Link::from_dict(&d).is_err());
    }
}
