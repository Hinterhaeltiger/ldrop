// SPDX-License-Identifier: GPL-3.0-or-later
//! Wire forms of the `a{sv}` dictionaries in the `org.ldrop.Ldrop1` API (SPEC §10), shared by
//! the daemon and `ldrop-client`. Keys are snake_case. Readers tolerate missing keys.

use zbus::{DeserializeDict, SerializeDict, Type};

use crate::capabilities::{BackendId, BackendStatus, DoctorReport, Support};
use crate::model::{Device, DeviceKind, Item, Peer, Platform, Protocol};

pub const BUS_NAME: &str = "org.ldrop.Ldrop1";
pub const ROOT_PATH: &str = "/org/ldrop/Ldrop1";
pub const MANAGER_IFACE: &str = "org.ldrop.Ldrop1.Manager";
pub const DISCOVERY_IFACE: &str = "org.ldrop.Ldrop1.Discovery";
pub const TRANSFER_IFACE: &str = "org.ldrop.Ldrop1.Transfer";
pub const AGENT_IFACE: &str = "org.ldrop.Ldrop1.Agent";
pub const DISCOVERY_PATH_PREFIX: &str = "/org/ldrop/Ldrop1/discovery";
pub const TRANSFER_PATH_PREFIX: &str = "/org/ldrop/Ldrop1/transfer";

/// `org.ldrop.Error.*` names (SPEC §10).
pub mod errors {
    pub const NOT_SUPPORTED: &str = "org.ldrop.Error.NotSupported";
    pub const NOT_FOUND: &str = "org.ldrop.Error.NotFound";
    pub const BUSY: &str = "org.ldrop.Error.Busy";
    pub const UNAVAILABLE: &str = "org.ldrop.Error.Unavailable";
    pub const PERMISSION_DENIED: &str = "org.ldrop.Error.PermissionDenied";
    pub const FAILED: &str = "org.ldrop.Error.Failed";
}

/// `Transfer.Peer`, and the `peer` entry of `Agent.RequestAccept` info.
#[derive(Debug, Clone, Default, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zbus(signature = "a{sv}")]
pub struct PeerDict {
    pub name: Option<String>,
    pub platform: Option<String>,
    pub kind: Option<String>,
    pub protocol: Option<String>,
    pub vendor: Option<String>,
}

/// One entry of `Transfer.Items`.
#[derive(Debug, Clone, Default, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zbus(signature = "a{sv}")]
pub struct ItemDict {
    pub name: Option<String>,
    pub mime: Option<String>,
    pub size: Option<u64>,
    pub path: Option<String>,
    pub text: Option<String>,
}

/// `Discovery.DeviceFound` properties.
#[derive(Debug, Clone, Default, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zbus(signature = "a{sv}")]
pub struct DeviceDict {
    pub name: Option<String>,
    pub kind: Option<String>,
    pub platform: Option<String>,
    pub protocol: Option<String>,
    pub mediums: Option<Vec<String>>,
    pub vendor: Option<String>,
}

/// `Agent.RequestAccept` info.
#[derive(Debug, Clone, Default, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zbus(signature = "a{sv}")]
pub struct RequestInfoDict {
    pub peer: Option<PeerDict>,
    pub pin: Option<String>,
    pub items: Option<Vec<ItemDict>>,
    pub bytes_total: Option<u64>,
    /// Unix µs.
    pub deadline: Option<u64>,
}

/// One backend entry of `Manager.Capabilities`.
#[derive(Debug, Clone, Default, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zbus(signature = "a{sv}")]
pub struct BackendDict {
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// `Manager.Capabilities`: one entry per backend id, plus `report`, the full doctor report as
/// JSON (hardware details for `ldrop doctor` and the GUI hardware page).
#[derive(Debug, Clone, Default, PartialEq, SerializeDict, DeserializeDict, Type)]
#[zbus(signature = "a{sv}")]
pub struct CapabilitiesDict {
    pub lan: Option<BackendDict>,
    pub bt: Option<BackendDict>,
    pub wfd: Option<BackendDict>,
    pub hotspot: Option<BackendDict>,
    #[zbus(rename = "awdl-shared")]
    pub awdl_shared: Option<BackendDict>,
    #[zbus(rename = "awdl-exclusive")]
    pub awdl_exclusive: Option<BackendDict>,
    #[zbus(rename = "awdl-activemon")]
    pub awdl_activemon: Option<BackendDict>,
    pub nan: Option<BackendDict>,
    pub report: Option<String>,
}

impl From<&Peer> for PeerDict {
    fn from(p: &Peer) -> Self {
        PeerDict {
            name: Some(p.name.clone()),
            platform: Some(p.platform.to_string()),
            kind: Some(p.kind.to_string()),
            protocol: Some(p.protocol.to_string()),
            vendor: p.vendor.clone(),
        }
    }
}

impl From<&PeerDict> for Peer {
    fn from(d: &PeerDict) -> Self {
        Peer {
            name: d.name.clone().unwrap_or_default(),
            platform: parse_or(&d.platform, Platform::Android),
            kind: parse_or(&d.kind, DeviceKind::Unknown),
            protocol: parse_or(&d.protocol, Protocol::QuickShare),
            vendor: d.vendor.clone(),
        }
    }
}

impl From<&Item> for ItemDict {
    fn from(i: &Item) -> Self {
        ItemDict {
            name: Some(i.name.clone()),
            mime: Some(i.mime.clone()),
            size: Some(i.size),
            path: i.path.clone(),
            text: i.text.clone(),
        }
    }
}

impl From<&ItemDict> for Item {
    fn from(d: &ItemDict) -> Self {
        Item {
            name: d.name.clone().unwrap_or_default(),
            mime: d.mime.clone().unwrap_or_else(|| "application/octet-stream".into()),
            size: d.size.unwrap_or(0),
            path: d.path.clone(),
            text: d.text.clone(),
        }
    }
}

impl From<&Device> for DeviceDict {
    fn from(d: &Device) -> Self {
        DeviceDict {
            name: Some(d.name.clone()),
            kind: Some(d.kind.to_string()),
            platform: Some(d.platform.to_string()),
            protocol: Some(d.protocol.to_string()),
            mediums: Some(d.mediums.iter().map(|m| m.to_string()).collect()),
            vendor: d.vendor.clone(),
        }
    }
}

impl DeviceDict {
    pub fn to_device(&self, id: &str) -> Device {
        Device {
            id: id.to_owned(),
            name: self.name.clone().unwrap_or_default(),
            kind: parse_or(&self.kind, DeviceKind::Unknown),
            platform: parse_or(&self.platform, Platform::Android),
            protocol: parse_or(&self.protocol, Protocol::QuickShare),
            mediums: self.mediums.iter().flatten().filter_map(|m| m.parse().ok()).collect(),
            vendor: self.vendor.clone(),
        }
    }
}

impl From<&DoctorReport> for CapabilitiesDict {
    fn from(r: &DoctorReport) -> Self {
        let entry = |id| {
            r.backend(id).map(|b: &BackendStatus| BackendDict {
                status: Some(b.support.to_string()),
                reason: Some(b.reason.clone()),
            })
        };
        CapabilitiesDict {
            lan: entry(BackendId::Lan),
            bt: entry(BackendId::Bt),
            wfd: entry(BackendId::Wfd),
            hotspot: entry(BackendId::Hotspot),
            awdl_shared: entry(BackendId::AwdlShared),
            awdl_exclusive: entry(BackendId::AwdlExclusive),
            awdl_activemon: entry(BackendId::AwdlActivemon),
            nan: entry(BackendId::Nan),
            report: serde_json::to_string(r).ok(),
        }
    }
}

impl CapabilitiesDict {
    /// The full report, from `report` when present, else rebuilt from the backend entries.
    pub fn to_report(&self) -> DoctorReport {
        if let Some(report) = self.report.as_deref().and_then(|j| serde_json::from_str(j).ok()) {
            return report;
        }
        let entries = [
            (BackendId::Lan, &self.lan),
            (BackendId::Bt, &self.bt),
            (BackendId::Wfd, &self.wfd),
            (BackendId::Hotspot, &self.hotspot),
            (BackendId::AwdlShared, &self.awdl_shared),
            (BackendId::AwdlExclusive, &self.awdl_exclusive),
            (BackendId::AwdlActivemon, &self.awdl_activemon),
            (BackendId::Nan, &self.nan),
        ];
        DoctorReport {
            backends: entries
                .into_iter()
                .filter_map(|(id, d)| {
                    let d = d.as_ref()?;
                    Some(BackendStatus {
                        id,
                        support: parse_or(&d.status, Support::Unsupported),
                        reason: d.reason.clone().unwrap_or_default(),
                    })
                })
                .collect(),
            ..Default::default()
        }
    }
}

fn parse_or<T: std::str::FromStr>(s: &Option<String>, fallback: T) -> T {
    s.as_deref().and_then(|s| s.parse().ok()).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{BluetoothReport, NetworkReport, Policy, WifiReport, evaluate};
    use zbus::wire::{LE, serialized::Context, to_bytes};

    fn round_trip<T>(value: &T) -> T
    where
        T: serde::Serialize + for<'d> serde::Deserialize<'d> + Type,
    {
        let ctxt = Context::new(LE, 0);
        let encoded = to_bytes(ctxt, value).unwrap();
        encoded.deserialize::<T>().unwrap().0
    }

    #[test]
    fn nested_request_info_round_trips() {
        let info = RequestInfoDict {
            peer: Some(PeerDict {
                name: Some("Pixel 9".into()),
                platform: Some("android".into()),
                ..Default::default()
            }),
            pin: Some("1234".into()),
            items: Some(vec![ItemDict { name: Some("a.jpg".into()), size: Some(5), ..Default::default() }]),
            bytes_total: Some(5),
            deadline: Some(42),
        };
        assert_eq!(<RequestInfoDict as Type>::SIGNATURE, "a{sv}");
        assert_eq!(round_trip(&info), info);
    }

    #[test]
    fn capabilities_round_trip_with_report() {
        let network = NetworkReport { lan_interfaces: vec!["enp6s0".into()] };
        let report = DoctorReport {
            backends: evaluate(&network, &WifiReport::default(), &BluetoothReport::default(), Policy::default()),
            network,
            ..Default::default()
        };
        let dict = CapabilitiesDict::from(&report);
        let back = round_trip(&dict);
        assert_eq!(back, dict);
        assert_eq!(back.to_report(), report);
        let without_json = CapabilitiesDict { report: None, ..back };
        assert_eq!(without_json.to_report().backends, report.backends);
    }

    #[test]
    fn missing_keys_get_fallbacks() {
        let device = DeviceDict {
            name: Some("X".into()),
            mediums: Some(vec!["lan".into(), "warp".into()]),
            ..Default::default()
        }
        .to_device("7");
        assert_eq!(device.kind, DeviceKind::Unknown);
        assert_eq!(device.mediums, vec![crate::model::Medium::Lan]);
    }
}
