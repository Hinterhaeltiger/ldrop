// SPDX-License-Identifier: GPL-3.0-or-later
//! Passive watching for advertisements through BlueZ's `AdvertisementMonitorManager1`: we
//! export an `org.bluez.AdvertisementMonitor1` with `or_patterns` under an object manager on
//! the system bus, and BlueZ says when a device whose advertisements match comes into range and
//! when it leaves. There is no discovery session of our own, so other programs see no scan.

use std::sync::atomic::{AtomicU32, Ordering};

use tokio::sync::mpsc;
use zbus::fdo::{DBusProxy, ObjectManager, ObjectManagerProxy};
use zbus::message::Header;
use zbus::names::{BusName, OwnedUniqueName};
use zbus::{Connection, ObjectPath, OwnedObjectPath, interface, proxy};

use crate::BLUEZ;

#[proxy(interface = "org.bluez.AdvertisementMonitorManager1", default_service = "org.bluez")]
trait MonitorManager {
    fn register_monitor(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
    fn unregister_monitor(&self, application: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Matches advertising data of type `ad_type` whose bytes from `start` on begin with `content`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern {
    pub start: u8,
    pub ad_type: u8,
    pub content: Vec<u8>,
}

/// When a device counts as in range: at `high` dBm or stronger for `high_timeout` seconds. It
/// leaves after `low_timeout` seconds below `low` dBm, or without advertising.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub high: i16,
    pub high_timeout: u16,
    pub low: i16,
    pub low_timeout: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// A matching device came into range: its BlueZ object path.
    Found(String),
    Lost(String),
}

struct MonitorIface {
    patterns: Vec<(u8, u8, Vec<u8>)>,
    range: Range,
    /// Only BlueZ may report devices; anyone on the system bus can call us.
    bluez: OwnedUniqueName,
    tx: mpsc::UnboundedSender<Seen>,
}

impl MonitorIface {
    fn sent_by_bluez(&self, hdr: &Header<'_>) -> bool {
        hdr.sender().is_some_and(|s| *s == self.bluez)
    }
}

#[interface(name = "org.bluez.AdvertisementMonitor1")]
impl MonitorIface {
    /// BlueZ dropped the monitor, for example because the adapter went away.
    fn release(&self) {
        tracing::debug!("BLE: BlueZ released the advertisement monitor");
    }

    fn activate(&self) {
        tracing::debug!("BLE: advertisement monitor active");
    }

    fn device_found(&self, device: OwnedObjectPath, #[zbus(header)] hdr: Header<'_>) {
        if self.sent_by_bluez(&hdr) {
            let _ = self.tx.send(Seen::Found(device.to_string()));
        }
    }

    fn device_lost(&self, device: OwnedObjectPath, #[zbus(header)] hdr: Header<'_>) {
        if self.sent_by_bluez(&hdr) {
            let _ = self.tx.send(Seen::Lost(device.to_string()));
        }
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> &str {
        "or_patterns"
    }

    #[zbus(property, name = "RSSIHighThreshold")]
    fn rssi_high_threshold(&self) -> i16 {
        self.range.high
    }

    #[zbus(property, name = "RSSIHighTimeout")]
    fn rssi_high_timeout(&self) -> u16 {
        self.range.high_timeout
    }

    #[zbus(property, name = "RSSILowThreshold")]
    fn rssi_low_threshold(&self) -> i16 {
        self.range.low
    }

    #[zbus(property, name = "RSSILowTimeout")]
    fn rssi_low_timeout(&self) -> u16 {
        self.range.low_timeout
    }

    #[zbus(property)]
    fn patterns(&self) -> Vec<(u8, u8, Vec<u8>)> {
        self.patterns.clone()
    }
}

/// A registered monitor. Unregistered when dropped (in the background) or by [`stop`].
///
/// [`stop`]: Monitor::stop
pub struct Monitor {
    conn: Connection,
    adapter: OwnedObjectPath,
    root: OwnedObjectPath,
    rx: mpsc::UnboundedReceiver<Seen>,
    stopped: bool,
}

impl Monitor {
    /// The next device that came or went. Never `None` while registered.
    pub async fn next(&mut self) -> Option<Seen> {
        self.rx.recv().await
    }

    pub async fn stop(mut self) {
        self.stopped = true;
        unregister(&self.conn, &self.adapter, &self.root).await;
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (conn, adapter, root) = (self.conn.clone(), self.adapter.clone(), self.root.clone());
        rt.spawn(async move { unregister(&conn, &adapter, &root).await });
    }
}

async fn unregister(conn: &Connection, adapter: &OwnedObjectPath, root: &OwnedObjectPath) {
    if let Ok(manager) = MonitorManagerProxy::builder(conn).path(adapter.clone()).build().await
        && let Err(e) = manager.unregister_monitor(root).await
    {
        tracing::debug!("BLE: cannot unregister the monitor {root}: {e}");
    }
    let server = conn.object_server();
    let _ = server.remove::<MonitorIface, _>(monitor_path(root)).await;
    let _ = server.remove::<ObjectManager, _>(root).await;
}

fn monitor_path(root: &OwnedObjectPath) -> String {
    format!("{root}/0")
}

/// Watches for devices advertising any of `patterns` on the first powered adapter that can.
/// `Ok(None)` when there is none; adapters are never powered on (SPEC §3.5).
pub async fn monitor(patterns: &[Pattern], range: Range) -> zbus::Result<Option<Monitor>> {
    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    let conn = Connection::system().await?;
    let Some(adapter) = monitoring_adapter(&conn).await? else { return Ok(None) };
    let bluez = DBusProxy::new(&conn).await?.get_name_owner(BusName::from_static_str(BLUEZ)?).await?;
    let root = OwnedObjectPath::try_from(format!(
        "/org/ldrop/ble/monitor{}_{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ))?;
    let (tx, rx) = mpsc::unbounded_channel();
    let iface = MonitorIface {
        patterns: patterns.iter().map(|p| (p.start, p.ad_type, p.content.clone())).collect(),
        range,
        bluez,
        tx,
    };
    let server = conn.object_server();
    server.at(&root, ObjectManager).await?;
    server.at(monitor_path(&root), iface).await?;

    let manager = MonitorManagerProxy::builder(&conn).path(adapter.clone()).build().await?;
    if let Err(e) = manager.register_monitor(&root).await {
        let _ = server.remove::<MonitorIface, _>(monitor_path(&root)).await;
        let _ = server.remove::<ObjectManager, _>(&root).await;
        return Err(e);
    }
    tracing::debug!("BLE: monitoring advertisements on {adapter}");
    Ok(Some(Monitor { conn, adapter, root, rx, stopped: false }))
}

async fn monitoring_adapter(conn: &Connection) -> zbus::Result<Option<OwnedObjectPath>> {
    let objects =
        ObjectManagerProxy::builder(conn).destination(BLUEZ).path("/").build().await?.get_managed_objects().await?;
    let mut candidates: Vec<OwnedObjectPath> = objects
        .into_iter()
        .filter(|(_, ifaces)| {
            let iface = |name: &str| ifaces.iter().find(|(n, _)| n.as_str() == name).map(|(_, p)| p);
            let powered = iface("org.bluez.Adapter1")
                .and_then(|p| p.get("Powered"))
                .and_then(|v| bool::try_from(v).ok())
                .unwrap_or(false);
            let or_patterns = iface("org.bluez.AdvertisementMonitorManager1")
                .and_then(|p| p.get("SupportedMonitorTypes"))
                .and_then(|v| <Vec<String>>::try_from(v.try_clone().ok()?).ok())
                .is_some_and(|types| types.iter().any(|t| t == "or_patterns"));
            powered && or_patterns
        })
        .map(|(path, _)| path)
        .collect();
    candidates.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(candidates.into_iter().next())
}
