// SPDX-License-Identifier: GPL-3.0-or-later
//! Typed proxies for `org.ldrop.Ldrop1` (SPEC §10).

use std::collections::HashMap;

use ldrop_core::dbus::CapabilitiesDict;
use zbus::{DBusError, ObjectPath, OwnedObjectPath, Variant, proxy};

/// `org.ldrop.Error.*`, plus transport errors.
#[derive(Debug, DBusError)]
#[zbus(prefix = "org.ldrop.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    NotSupported(String),
    NotFound(String),
    Busy(String),
    Unavailable(String),
    PermissionDenied(String),
    Failed(String),
}

impl Error {
    /// The human-readable part, without the D-Bus error name.
    pub fn message(&self) -> String {
        match self {
            Error::ZBus(zbus::Error::MethodError(name, Some(msg), _)) => format!("{msg} ({name})"),
            Error::ZBus(zbus::Error::FDO(e)) => e.to_string(),
            Error::ZBus(e) => e.to_string(),
            Error::NotSupported(m)
            | Error::NotFound(m)
            | Error::Busy(m)
            | Error::Unavailable(m)
            | Error::PermissionDenied(m)
            | Error::Failed(m) => m.clone(),
        }
    }

    /// The daemon isn't running and couldn't be activated.
    pub fn is_daemon_missing(&self) -> bool {
        matches!(
            self,
            Error::ZBus(zbus::Error::MethodError(name, _, _))
                if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown"
                    || name.as_str() == "org.freedesktop.DBus.Error.NameHasNoOwner"
        )
    }
}

impl From<zbus::fdo::Error> for Error {
    fn from(e: zbus::fdo::Error) -> Self {
        Error::ZBus(zbus::Error::FDO(Box::new(e)))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[proxy(
    interface = "org.ldrop.Ldrop1.Manager",
    default_service = "org.ldrop.Ldrop1",
    default_path = "/org/ldrop/Ldrop1"
)]
pub trait Manager {
    #[zbus(property)]
    fn device_name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_device_name(&self, name: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn visibility(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn visibility_expires_at(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn download_dir(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn set_download_dir(&self, dir: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn capabilities(&self) -> zbus::Result<CapabilitiesDict>;

    fn set_visibility(&self, mode: &str, timeout_secs: u32) -> Result<()>;
    fn start_discovery(&self) -> Result<OwnedObjectPath>;
    fn send(&self, device_id: &str, uris: &[&str], opts: HashMap<&str, Variant<'_>>) -> Result<OwnedObjectPath>;
    fn send_text(&self, device_id: &str, text: &str, is_url: bool) -> Result<OwnedObjectPath>;
    fn create_qr_session(&self) -> Result<(OwnedObjectPath, String)>;
    fn accept(&self, transfer: &ObjectPath<'_>) -> Result<()>;
    fn reject(&self, transfer: &ObjectPath<'_>) -> Result<()>;
    fn cancel(&self, transfer: &ObjectPath<'_>) -> Result<()>;
    fn register_agent(&self, agent: &ObjectPath<'_>) -> Result<()>;
    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> Result<()>;
    fn list_transfers(&self) -> Result<Vec<OwnedObjectPath>>;
    fn clear_history(&self) -> Result<()>;

    #[zbus(signal)]
    fn incoming_request(&self, transfer: ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.ldrop.Ldrop1.Discovery", default_service = "org.ldrop.Ldrop1", assume_defaults = false)]
pub trait Discovery {
    fn stop(&self) -> Result<()>;
}

/// Development-only interface of `ldropd --dev-sim`.
#[proxy(interface = "org.ldrop.Ldrop1.Debug", default_service = "org.ldrop.Ldrop1", default_path = "/org/ldrop/Ldrop1")]
pub trait Debug {
    fn inject_incoming(
        &self,
        peer_name: &str,
        platform: &str,
        names: &[&str],
        size_each: u64,
    ) -> Result<OwnedObjectPath>;
}
