// SPDX-License-Identifier: GPL-3.0-or-later
//! Broadcast advertisements through BlueZ's `LEAdvertisingManager1`: we export an
//! `org.bluez.LEAdvertisement1` object on the system bus, and BlueZ reads its properties when
//! we register it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};

use zbus::fdo::ObjectManagerProxy;
use zbus::{Connection, ObjectPath, OwnedObjectPath, OwnedVariant, Variant, interface, proxy};

use crate::BLUEZ;

#[proxy(interface = "org.bluez.LEAdvertisingManager1", default_service = "org.bluez")]
trait AdvertisingManager {
    fn register_advertisement(
        &self,
        advertisement: &ObjectPath<'_>,
        options: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()>;
    fn unregister_advertisement(&self, advertisement: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// BlueZ's default is about 1.28 s.
const INTERVAL_MS: u32 = 100;

struct AdvertisementIface {
    service_uuid: String,
    data: Vec<u8>,
}

#[interface(name = "org.bluez.LEAdvertisement1")]
impl AdvertisementIface {
    /// BlueZ dropped the advertisement on its own, for example because the adapter went away.
    fn release(&self) {
        tracing::debug!("BLE: BlueZ released the {} advertisement", self.service_uuid);
    }

    #[zbus(property, name = "Type")]
    fn kind(&self) -> &str {
        "broadcast"
    }

    /// Advertise as often as BLE allows a sender to, like Android's low-latency mode: phones
    /// scan in short windows while their screen is off, and miss a slow advertisement for
    /// tens of seconds. Milliseconds; BlueZ before 5.62 ignores both.
    #[zbus(property)]
    fn min_interval(&self) -> u32 {
        INTERVAL_MS
    }

    #[zbus(property)]
    fn max_interval(&self) -> u32 {
        INTERVAL_MS
    }

    #[zbus(property)]
    fn service_data(&self) -> HashMap<String, OwnedVariant> {
        let data = Variant::from(&self.data).try_into_owned().expect("byte arrays hold no file descriptors");
        HashMap::from([(self.service_uuid.clone(), data)])
    }
}

/// A registered advertisement. Unregistered when dropped (in the background) or by [`stop`].
///
/// [`stop`]: Advertisement::stop
pub struct Advertisement {
    conn: Connection,
    adapter: OwnedObjectPath,
    path: OwnedObjectPath,
    stopped: bool,
}

impl Advertisement {
    pub async fn stop(mut self) {
        self.stopped = true;
        unregister(&self.conn, &self.adapter, &self.path).await;
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (conn, adapter, path) = (self.conn.clone(), self.adapter.clone(), self.path.clone());
        rt.spawn(async move { unregister(&conn, &adapter, &path).await });
    }
}

async fn unregister(conn: &Connection, adapter: &OwnedObjectPath, path: &OwnedObjectPath) {
    if let Ok(manager) = AdvertisingManagerProxy::builder(conn).path(adapter.clone()).build().await
        && let Err(e) = manager.unregister_advertisement(path).await
    {
        tracing::debug!("BLE: cannot unregister {path}: {e}");
    }
    let _ = conn.object_server().remove::<AdvertisementIface, _>(path).await;
}

/// Broadcasts `data` as service data for the 16-bit `uuid16` on the first powered adapter that
/// can advertise. `Ok(None)` when there is none; adapters are never powered on (SPEC §3.5).
pub async fn advertise_service_data(uuid16: u16, data: Vec<u8>) -> zbus::Result<Option<Advertisement>> {
    static NEXT_ID: AtomicU32 = AtomicU32::new(0);

    let conn = Connection::system().await?;
    let Some(adapter) = advertising_adapter(&conn).await? else { return Ok(None) };
    let service_uuid = format!("0000{uuid16:04x}-0000-1000-8000-00805f9b34fb");
    let path = OwnedObjectPath::try_from(format!(
        "/org/ldrop/ble/adv{}_{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ))?;
    conn.object_server().at(&path, AdvertisementIface { service_uuid: service_uuid.clone(), data }).await?;

    let manager = AdvertisingManagerProxy::builder(&conn).path(adapter.clone()).build().await?;
    if let Err(e) = manager.register_advertisement(&path, HashMap::new()).await {
        let _ = conn.object_server().remove::<AdvertisementIface, _>(&path).await;
        return Err(e);
    }
    tracing::debug!("BLE: advertising {service_uuid} service data on {adapter}");
    Ok(Some(Advertisement { conn, adapter, path, stopped: false }))
}

async fn advertising_adapter(conn: &Connection) -> zbus::Result<Option<OwnedObjectPath>> {
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
            powered && iface("org.bluez.LEAdvertisingManager1").is_some()
        })
        .map(|(path, _)| path)
        .collect();
    candidates.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    Ok(candidates.into_iter().next())
}
