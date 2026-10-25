// SPDX-License-Identifier: GPL-3.0-or-later
//! Temporary Wi-Fi networks through NetworkManager (SPEC §5.4): joining the hotspot or
//! Wi-Fi Direct group a phone hosts, and hosting a hotspot for one.
//!
//! Every profile is created with `persist = volatile` (never written to disk, deleted when it
//! goes down) and `bind-activation = dbus-client` (taken down when our bus connection closes,
//! so a crash cannot leave the computer on the phone's network). When a [`Session`] ends, the
//! connection the device had before comes back.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use zbus::proxy::CacheProperties;
use zbus::{Connection, ObjectPath, OwnedObjectPath, OwnedVariant, Variant, proxy};

const DEVICE_TYPE_WIFI: u32 = 2;
/// `NM_DEVICE_STATE_ACTIVATED`
const DEVICE_ACTIVATED: u32 = 100;
/// `NM_WIFI_DEVICE_CAP_AP`
const WIFI_CAP_AP: u32 = 0x40;
/// `NM_ACTIVE_CONNECTION_STATE_ACTIVATED`, `…_DEACTIVATED`
const ACTIVE_ACTIVATED: u32 = 2;
const ACTIVE_DEACTIVATED: u32 = 4;
/// Joining includes a scan and DHCP; hosting includes starting dnsmasq.
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("NetworkManager: {0}")]
    Bus(#[from] zbus::Error),
    #[error("{0}")]
    Failed(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[proxy(
    interface = "org.freedesktop.NetworkManager",
    default_service = "org.freedesktop.NetworkManager",
    default_path = "/org/freedesktop/NetworkManager"
)]
trait NetworkManager {
    fn get_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;
    fn add_and_activate_connection2(
        &self,
        connection: HashMap<&str, HashMap<&str, Variant<'_>>>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
        options: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<(OwnedObjectPath, OwnedObjectPath, HashMap<String, OwnedVariant>)>;
    fn activate_connection(
        &self,
        connection: &ObjectPath<'_>,
        device: &ObjectPath<'_>,
        specific_object: &ObjectPath<'_>,
    ) -> zbus::Result<OwnedObjectPath>;
    fn deactivate_connection(&self, active_connection: &ObjectPath<'_>) -> zbus::Result<()>;
}

#[proxy(interface = "org.freedesktop.NetworkManager.Device", default_service = "org.freedesktop.NetworkManager")]
trait Device {
    #[zbus(property)]
    fn device_type(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn interface(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn managed(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn active_connection(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Device.Wireless",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Wireless {
    #[zbus(property)]
    fn wireless_capabilities(&self) -> zbus::Result<u32>;
}

#[proxy(
    interface = "org.freedesktop.NetworkManager.Connection.Active",
    default_service = "org.freedesktop.NetworkManager"
)]
trait Active {
    #[zbus(property)]
    fn state(&self) -> zbus::Result<u32>;
    #[zbus(property)]
    fn connection(&self) -> zbus::Result<OwnedObjectPath>;
    #[zbus(property)]
    fn ip4_config(&self) -> zbus::Result<OwnedObjectPath>;
}

#[proxy(interface = "org.freedesktop.NetworkManager.IP4Config", default_service = "org.freedesktop.NetworkManager")]
trait Ip4Config {
    #[zbus(property)]
    fn address_data(&self) -> zbus::Result<Vec<HashMap<String, OwnedVariant>>>;
    #[zbus(property)]
    fn gateway(&self) -> zbus::Result<String>;
}

macro_rules! uncached {
    ($proxy:ident, $conn:expr, $path:expr) => {
        $proxy::builder($conn).path($path).cache_properties(CacheProperties::No).build()
    };
}

/// A Wi-Fi device NetworkManager manages.
#[derive(Debug, Clone)]
pub struct WifiDevice {
    conn: Connection,
    path: OwnedObjectPath,
    pub interface: String,
    /// Can host an access point.
    pub can_host: bool,
    /// Currently connected to a network, which a session would interrupt.
    pub connected: bool,
}

/// The first managed Wi-Fi device, `None` without NetworkManager or Wi-Fi.
pub async fn wifi_device() -> Result<Option<WifiDevice>> {
    let conn = Connection::system().await?;
    let nm = NetworkManagerProxy::new(&conn).await?;
    let devices = match nm.get_devices().await {
        Ok(devices) => devices,
        // NetworkManager is not running.
        Err(zbus::Error::MethodError(..)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    for path in devices {
        let device = uncached!(DeviceProxy, &conn, path.clone()).await?;
        if device.device_type().await? != DEVICE_TYPE_WIFI || !device.managed().await? {
            continue;
        }
        let caps = uncached!(WirelessProxy, &conn, path.clone()).await?.wireless_capabilities().await?;
        return Ok(Some(WifiDevice {
            interface: device.interface().await?,
            can_host: caps & WIFI_CAP_AP != 0,
            connected: device.state().await? == DEVICE_ACTIVATED,
            conn,
            path,
        }));
    }
    Ok(None)
}

/// A temporary network the device is on. Taken down when dropped, and the previous
/// connection is brought back.
pub struct Session {
    conn: Connection,
    device: OwnedObjectPath,
    active: OwnedObjectPath,
    previous: Option<OwnedObjectPath>,
    /// Our address on the network.
    pub address: Ipv4Addr,
    /// The network's gateway: the host, when joined.
    pub gateway: Option<Ipv4Addr>,
}

impl WifiDevice {
    /// Joins a WPA2 network (open when `password` is empty).
    pub async fn join(&self, ssid: &str, password: &str) -> Result<Session> {
        let mut wifi = HashMap::from([
            ("ssid", Variant::from(ssid.as_bytes().to_vec())),
            ("mode", Variant::from("infrastructure")),
            // Probe for it directly: phone hotspots may not be in the last scan yet.
            ("hidden", Variant::from(true)),
        ]);
        let mut settings = HashMap::from([
            ("connection", connection_section(ssid)),
            (
                "ipv4",
                HashMap::from([
                    ("method", Variant::from("auto")),
                    ("never-default", Variant::from(true)),
                    ("may-fail", Variant::from(false)),
                ]),
            ),
            ("ipv6", HashMap::from([("method", Variant::from("ignore"))])),
        ]);
        if !password.is_empty() {
            wifi.insert("security", Variant::from("802-11-wireless-security"));
            settings.insert(
                "802-11-wireless-security",
                HashMap::from([("key-mgmt", Variant::from("wpa-psk")), ("psk", Variant::from(password))]),
            );
        }
        settings.insert("802-11-wireless", wifi);
        self.activate(settings).await
    }

    /// Hosts a WPA2 hotspot on 2.4 GHz, with NetworkManager's DHCP server (`ipv4.method shared`,
    /// needs dnsmasq).
    pub async fn host(&self, ssid: &str, password: &str) -> Result<Session> {
        let settings = HashMap::from([
            ("connection", connection_section(ssid)),
            (
                "802-11-wireless",
                HashMap::from([
                    ("ssid", Variant::from(ssid.as_bytes().to_vec())),
                    ("mode", Variant::from("ap")),
                    ("band", Variant::from("bg")),
                    ("security", Variant::from("802-11-wireless-security")),
                ]),
            ),
            (
                "802-11-wireless-security",
                HashMap::from([
                    ("key-mgmt", Variant::from("wpa-psk")),
                    ("psk", Variant::from(password)),
                    ("proto", Variant::from(vec!["rsn"])),
                    ("pairwise", Variant::from(vec!["ccmp"])),
                    ("group", Variant::from(vec!["ccmp"])),
                ]),
            ),
            ("ipv4", HashMap::from([("method", Variant::from("shared"))])),
            ("ipv6", HashMap::from([("method", Variant::from("ignore"))])),
        ]);
        self.activate(settings).await
    }

    async fn activate(&self, settings: HashMap<&str, HashMap<&str, Variant<'_>>>) -> Result<Session> {
        let device = uncached!(DeviceProxy, &self.conn, self.path.clone()).await?;
        let previous = match device.active_connection().await? {
            active if active.as_str() == "/" => None,
            active => uncached!(ActiveProxy, &self.conn, active).await?.connection().await.ok(),
        };
        let options =
            HashMap::from([("persist", Variant::from("volatile")), ("bind-activation", Variant::from("dbus-client"))]);
        let nm = NetworkManagerProxy::new(&self.conn).await?;
        let root = ObjectPath::try_from("/").expect("valid object path");
        let (_, active, _) = nm.add_and_activate_connection2(settings, &self.path, &root, options).await?;
        // From here on, dropping the session undoes everything.
        let mut session = Session {
            conn: self.conn.clone(),
            device: self.path.clone(),
            active: active.clone(),
            previous,
            address: Ipv4Addr::UNSPECIFIED,
            gateway: None,
        };
        let proxy = uncached!(ActiveProxy, &self.conn, active).await?;
        let deadline = Instant::now() + ACTIVATION_TIMEOUT;
        loop {
            match proxy.state().await {
                Ok(ACTIVE_ACTIVATED) => break,
                Ok(ACTIVE_DEACTIVATED) | Err(_) => {
                    return Err(Error::Failed("NetworkManager could not connect".into()));
                }
                Ok(_) if Instant::now() > deadline => return Err(Error::Failed("timed out connecting".into())),
                Ok(_) => tokio::time::sleep(POLL).await,
            }
        }
        let ip4 = uncached!(Ip4ConfigProxy, &self.conn, proxy.ip4_config().await?).await?;
        session.address = ip4
            .address_data()
            .await?
            .iter()
            .find_map(|a| a.get("address").and_then(|v| <&str>::try_from(v).ok()).and_then(|a| a.parse().ok()))
            .ok_or_else(|| Error::Failed("no IPv4 address on the new network".into()))?;
        session.gateway = ip4.gateway().await.ok().and_then(|g| g.parse().ok());
        Ok(session)
    }
}

fn connection_section(ssid: &str) -> HashMap<&'static str, Variant<'static>> {
    let mut section = HashMap::from([
        ("id", Variant::from(format!("ldrop {ssid}"))),
        ("type", Variant::from("802-11-wireless")),
        ("autoconnect", Variant::from(false)),
    ]);
    // Owned by this user: needs only the `settings.modify.own` permission.
    if let Ok(user) = std::env::var("USER") {
        section.insert("permissions", Variant::from(vec![format!("user:{user}")]));
    }
    section
}

impl Drop for Session {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (conn, device, active, previous) =
            (self.conn.clone(), self.device.clone(), self.active.clone(), self.previous.take());
        rt.spawn(async move {
            let Ok(nm) = NetworkManagerProxy::new(&conn).await else { return };
            if let Err(e) = nm.deactivate_connection(&active).await {
                tracing::debug!("NetworkManager: cannot deactivate {active}: {e}");
            }
            if let Some(previous) = previous {
                let root = ObjectPath::try_from("/").expect("valid object path");
                if let Err(e) = nm.activate_connection(&previous, &device, &root).await {
                    tracing::warn!("NetworkManager: cannot bring the previous connection back: {e}");
                }
            }
        });
    }
}
