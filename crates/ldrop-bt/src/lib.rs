// SPDX-License-Identifier: GPL-3.0-or-later
//! Bluetooth classic for ldrop (SPEC §5.2, §5.4), through BlueZ over zbus:
//!
//! - [`Rfcomm`]: an RFCOMM service registered with `ProfileManager1`, which takes incoming
//!   connections and opens outgoing ones;
//! - [`Visible`]: the adapter renamed and discoverable, restored afterwards (also after a crash);
//! - [`Discovery`]: devices seen while scanning, classic and LE.
//!
//! Nothing here knows Quick Share: callers pass the service UUID and names, and parse the
//! names and service data that discovery reports.

mod discovery;
mod profile;
mod visible;

pub use discovery::{Discovery, Seen};
pub use profile::{Incoming, Rfcomm, RfcommStream};
pub use visible::{Visible, restore_after_crash};

use std::collections::HashMap;

use zbus::fdo::ObjectManagerProxy;
use zbus::proxy::CacheProperties;
use zbus::{Connection, ObjectPath, OwnedObjectPath, Variant, proxy};

pub(crate) const BLUEZ: &str = "org.bluez";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Bus(#[from] zbus::Error),
    #[error(transparent)]
    Fdo(#[from] zbus::fdo::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Failed(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
pub(crate) trait Adapter1 {
    fn start_discovery(&self) -> zbus::Result<()>;
    fn stop_discovery(&self) -> zbus::Result<()>;
    fn set_discovery_filter(&self, filter: HashMap<&str, Variant<'_>>) -> zbus::Result<()>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_alias(&self, alias: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn discoverable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_discoverable(&self, discoverable: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discoverable_timeout(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn set_discoverable_timeout(&self, seconds: u32) -> zbus::Result<()>;
}

#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub(crate) trait Device1 {
    fn connect_profile(&self, uuid: &str) -> zbus::Result<()>;
    fn disconnect_profile(&self, uuid: &str) -> zbus::Result<()>;
}

/// A BlueZ adapter (`/org/bluez/hciN`) and the system bus connection used to drive it.
#[derive(Clone)]
pub struct Adapter {
    conn: Connection,
    path: OwnedObjectPath,
}

impl Adapter {
    /// The first powered adapter, `None` when there is none. Adapters are never powered on
    /// (SPEC §3.5).
    pub async fn first_powered() -> Result<Option<Adapter>> {
        let conn = Connection::system().await?;
        let objects = ObjectManagerProxy::builder(&conn)
            .destination(BLUEZ)
            .path("/")
            .build()
            .await?
            .get_managed_objects()
            .await?;
        let mut powered: Vec<OwnedObjectPath> = objects
            .into_iter()
            .filter(|(_, ifaces)| {
                ifaces
                    .iter()
                    .find(|(name, _)| name.as_str() == "org.bluez.Adapter1")
                    .and_then(|(_, props)| props.get("Powered"))
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(false)
            })
            .map(|(path, _)| path)
            .collect();
        powered.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(powered.into_iter().next().map(|path| Adapter { conn, path }))
    }

    /// The adapter's object path, e.g. `/org/bluez/hci0`.
    pub fn path(&self) -> &ObjectPath<'static> {
        &self.path
    }

    pub async fn powered(&self) -> Result<bool> {
        Ok(self.proxy().await?.powered().await?)
    }

    pub(crate) async fn proxy(&self) -> zbus::Result<Adapter1Proxy<'static>> {
        Adapter1Proxy::builder(&self.conn).path(self.path.clone()).cache_properties(CacheProperties::No).build().await
    }

    pub(crate) async fn device(&self, address: &str) -> Result<(OwnedObjectPath, Device1Proxy<'static>)> {
        let path = device_path(&self.path, address)
            .ok_or_else(|| Error::Failed(format!("`{address}` is not a Bluetooth address")))?;
        let proxy =
            Device1Proxy::builder(&self.conn).path(path.clone()).cache_properties(CacheProperties::No).build().await?;
        Ok((path, proxy))
    }
}

/// `/org/bluez/hci0` + `AA:BB:CC:DD:EE:FF` → `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF`.
fn device_path(adapter: &ObjectPath<'_>, address: &str) -> Option<OwnedObjectPath> {
    let parts: Vec<&str> = address.split(':').collect();
    let valid = parts.len() == 6 && parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()));
    if !valid {
        return None;
    }
    OwnedObjectPath::try_from(format!("{adapter}/dev_{}", parts.join("_").to_ascii_uppercase())).ok()
}

/// The address in a device path: `…/dev_AA_BB_CC_DD_EE_FF` → `AA:BB:CC:DD:EE:FF`.
pub(crate) fn device_address(path: &ObjectPath<'_>) -> Option<String> {
    let last = path.as_str().rsplit('/').next()?.strip_prefix("dev_")?;
    let address = last.replace('_', ":");
    (address.len() == 17).then_some(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_paths() {
        let adapter = ObjectPath::try_from("/org/bluez/hci0").unwrap();
        let path = device_path(&adapter, "aa:bb:cc:00:11:22").unwrap();
        assert_eq!(path.as_str(), "/org/bluez/hci0/dev_AA_BB_CC_00_11_22");
        assert_eq!(device_address(&path).as_deref(), Some("AA:BB:CC:00:11:22"));
        assert_eq!(device_path(&adapter, "aa:bb:cc:00:11"), None);
        assert_eq!(device_path(&adapter, "aa:bb:cc:00:11:2/"), None);
        assert_eq!(device_address(&adapter), None);
    }
}
