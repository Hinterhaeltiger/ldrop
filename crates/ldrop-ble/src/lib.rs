// SPDX-License-Identifier: GPL-3.0-or-later
//! Bluetooth side of ldrop: a read-only BlueZ probe for `ldrop doctor` (SPEC §3.1, §3.5),
//! advertisements, used for the Quick Share BLE wake (SPEC §5.2) and the AirDrop sender advert
//! (SPEC §6.1), and advertisement monitors, which wake the AirDrop receiver (SPEC §6.5).
//!
//! Talks to BlueZ with zbus directly instead of bluer, so there is no libdbus dependency.

mod advertise;
mod monitor;

pub use advertise::{Advertisement, advertise_manufacturer_data, advertise_service_data};
pub use monitor::{Monitor, Pattern, Range, Seen, monitor};

use std::collections::HashMap;
use std::time::Duration;

use ldrop_core::capabilities::{BluetoothReport, BtAdapter};
use tokio::sync::OnceCell;
use zbus::fdo::ObjectManagerProxy;
use zbus::{Connection, OwnedVariant};

pub(crate) const BLUEZ: &str = "org.bluez";

/// Probes BlueZ over the system bus. Never fails: problems end up in [`BluetoothReport::error`].
pub async fn probe() -> BluetoothReport {
    let mut report = match probe_bluez().await {
        Ok(report) => report,
        Err(e) => BluetoothReport {
            error: Some(match &e {
                zbus::Error::MethodError(name, _, _)
                    if name.as_str() == "org.freedesktop.DBus.Error.ServiceUnknown" =>
                {
                    "BlueZ (bluetoothd) is not running".to_owned()
                }
                _ => format!("cannot query BlueZ: {e}"),
            }),
            ..Default::default()
        },
    };
    if report.bluez_running {
        report.bluez_version = bluez_version().await;
    }
    report
}

async fn probe_bluez() -> zbus::Result<BluetoothReport> {
    let conn = Connection::system().await?;
    let objects =
        ObjectManagerProxy::builder(&conn).destination(BLUEZ).path("/").build().await?.get_managed_objects().await?;

    let mut report = BluetoothReport { bluez_running: true, ..Default::default() };
    for (path, ifaces) in &objects {
        let iface = |name: &str| ifaces.iter().find(|(n, _)| n.as_str() == name).map(|(_, p)| p);
        if iface("org.bluez.ProfileManager1").is_some() {
            report.profile_manager = true;
        }
        let Some(adapter) = iface("org.bluez.Adapter1") else { continue };
        let adv = iface("org.bluez.LEAdvertisingManager1");
        report.adapters.push(BtAdapter {
            name: path.as_str().rsplit('/').next().unwrap_or_default().to_owned(),
            address: string(adapter, "Address").unwrap_or_default(),
            powered: adapter.get("Powered").and_then(|v| bool::try_from(v).ok()).unwrap_or(false),
            adv_instances_supported: adv.and_then(|p| p.get("SupportedInstances")).and_then(|v| u8::try_from(v).ok()),
            adv_instances_active: adv.and_then(|p| p.get("ActiveInstances")).and_then(|v| u8::try_from(v).ok()),
        });
    }
    report.adapters.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(report)
}

fn string(props: &HashMap<String, OwnedVariant>, key: &str) -> Option<String> {
    props.get(key).and_then(|v| <&str>::try_from(v).ok()).map(str::to_owned)
}

/// `bluetoothctl --version` prints `bluetoothctl: 5.87`; BlueZ has no version property.
/// Asked once per process: probes run every 30 s.
async fn bluez_version() -> Option<String> {
    static VERSION: OnceCell<Option<String>> = OnceCell::const_new();
    VERSION.get_or_init(run_bluetoothctl_version).await.clone()
}

async fn run_bluetoothctl_version() -> Option<String> {
    let output = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("bluetoothctl").arg("--version").kill_on_drop(true).output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    text.rsplit(':').next().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}
