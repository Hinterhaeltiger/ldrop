// SPDX-License-Identifier: GPL-3.0-or-later
//! Scanning for devices, Bluetooth classic and LE interleaved.
//!
//! BlueZ keeps device objects around (paired ones for good), so only fresh evidence counts as
//! a sighting: a device appearing, or its RSSI, name or service data changing while we scan.

use std::collections::HashMap;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::fdo::ObjectManagerProxy;
use zbus::{MatchRule, MessageStream, OwnedObjectPath, OwnedVariant, Variant};

use crate::{Adapter, BLUEZ, Result, device_address};

const DEVICE: &str = "org.bluez.Device1";

/// What discovery reports.
#[derive(Debug, Clone)]
pub enum Seen {
    /// A device was seen just now. Reported again on every sighting.
    Device {
        address: String,
        name: Option<String>,
        /// Service data by 128-bit UUID string.
        service_data: HashMap<String, Vec<u8>>,
    },
    /// BlueZ forgot the device.
    Lost { address: String },
}

#[derive(Default)]
struct Known {
    name: Option<String>,
    service_data: HashMap<String, Vec<u8>>,
}

impl Known {
    /// Merges properties; true when one of them shows the device was seen just now.
    fn update(&mut self, props: &HashMap<String, OwnedVariant>) -> bool {
        let mut seen = props.contains_key("RSSI");
        if let Some(name) = props.get("Name").and_then(|v| <&str>::try_from(v).ok()) {
            self.name = Some(name.to_owned());
            seen = true;
        }
        if let Some(data) =
            props.get("ServiceData").and_then(|v| HashMap::<String, OwnedVariant>::try_from(v.clone()).ok())
        {
            for (uuid, value) in data {
                if let Ok(bytes) = Vec::<u8>::try_from(value) {
                    self.service_data.insert(uuid.to_ascii_lowercase(), bytes);
                }
            }
            seen = true;
        }
        seen
    }
}

/// A running scan; stopped when dropped.
pub struct Discovery {
    rx: mpsc::Receiver<Seen>,
    task: JoinHandle<()>,
    adapter: Adapter,
}

impl Discovery {
    pub async fn next(&mut self) -> Option<Seen> {
        self.rx.recv().await
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        self.task.abort();
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let adapter = self.adapter.clone();
        rt.spawn(async move {
            if let Ok(proxy) = adapter.proxy().await {
                let _ = proxy.stop_discovery().await;
            }
        });
    }
}

impl Adapter {
    pub async fn discover(&self) -> Result<Discovery> {
        let conn = &self.conn;
        let namespace = self.path.as_str().to_owned();
        let changed = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender(BLUEZ)
            .interface("org.freedesktop.DBus.Properties")
            .member("PropertiesChanged")
            .path_namespace(namespace.as_str())
            .arg(0, DEVICE)
            .build()?;
        let mut changed = MessageStream::for_match_rule(changed, conn, None).await?;
        let objects = ObjectManagerProxy::builder(conn).destination(BLUEZ).path("/").build().await?;
        let mut added = objects.receive_interfaces_added().await?;
        let mut removed = objects.receive_interfaces_removed().await?;

        let mut known: HashMap<OwnedObjectPath, Known> = HashMap::new();
        for (path, ifaces) in objects.get_managed_objects().await? {
            if let Some((_, props)) = ifaces.iter().find(|(n, _)| n.as_str() == DEVICE)
                && path.as_str().starts_with(&namespace)
            {
                known.entry(path).or_default().update(props);
            }
        }

        let proxy = self.proxy().await?;
        let filter = HashMap::from([("Transport", Variant::from("auto")), ("DuplicateData", Variant::from(true))]);
        proxy.set_discovery_filter(filter).await?;
        proxy.start_discovery().await?;
        tracing::debug!("Bluetooth: discovery started on {}", self.path);

        let (tx, rx) = mpsc::channel(64);
        let task = tokio::spawn(async move {
            let report = |path: &OwnedObjectPath, k: &Known| {
                device_address(path).map(|address| Seen::Device {
                    address,
                    name: k.name.clone(),
                    service_data: k.service_data.clone(),
                })
            };
            loop {
                let event = tokio::select! {
                    Some(Ok(msg)) = changed.next() => {
                        let Some(path) = msg.header().path().map(|p| OwnedObjectPath::from(p.to_owned())) else { continue };
                        let Ok((_, props, _)) =
                            msg.body().deserialize::<(String, HashMap<String, OwnedVariant>, Vec<String>)>()
                        else {
                            continue;
                        };
                        let k = known.entry(path.clone()).or_default();
                        if !k.update(&props) { continue }
                        report(&path, k)
                    }
                    Some(signal) = added.next() => {
                        let Ok(args) = signal.args() else { continue };
                        let path = OwnedObjectPath::from(args.object_path().to_owned());
                        let Some(props) = args.interfaces_and_properties().get(DEVICE) else { continue };
                        if !path.as_str().starts_with(&namespace) { continue }
                        let props: HashMap<String, OwnedVariant> = props
                            .iter()
                            .filter_map(|(k, v)| Some((k.to_string(), v.try_to_owned().ok()?)))
                            .collect();
                        let k = known.entry(path.clone()).or_default();
                        k.update(&props);
                        report(&path, k)
                    }
                    Some(signal) = removed.next() => {
                        let Ok(args) = signal.args() else { continue };
                        if !args.interfaces().iter().any(|i| i.as_str() == DEVICE) { continue }
                        let path = OwnedObjectPath::from(args.object_path().to_owned());
                        known.remove(&path);
                        device_address(&path).map(|address| Seen::Lost { address })
                    }
                    else => break,
                };
                if let Some(event) = event
                    && tx.send(event).await.is_err()
                {
                    break;
                }
            }
        });
        Ok(Discovery { rx, task, adapter: self.clone() })
    }
}
