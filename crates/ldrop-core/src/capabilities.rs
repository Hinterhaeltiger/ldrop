// SPDX-License-Identifier: GPL-3.0-or-later
//! Capability report and backend rules for `ldrop doctor` (SPEC §3.2, §3.3, §3.5).
//!
//! Probing lives in `ldrop-wifi` and `ldrop-ble`; this module turns their facts into a
//! `supported | degraded | unsupported` verdict per link backend. It is pure so the rules can
//! be tested against described hardware.

use serde::{Deserialize, Serialize};

use crate::model::string_enum;

string_enum!(
    /// Link backends (SPEC §3.2).
    BackendId ("backend") {
        Lan => "lan",
        Bt => "bt",
        Wfd => "wfd",
        Hotspot => "hotspot",
        AwdlShared => "awdl-shared",
        AwdlExclusive => "awdl-exclusive",
        AwdlActivemon => "awdl-activemon",
        Nan => "nan",
    }
);

string_enum!(
    Support ("support level") {
        Supported => "supported",
        Degraded => "degraded",
        Unsupported => "unsupported",
    }
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub id: BackendId,
    pub support: Support,
    pub reason: String,
}

/// Everything `doctor` found. Serialized as-is for `--json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub backends: Vec<BackendStatus>,
    pub network: NetworkReport,
    pub wifi: WifiReport,
    pub bluetooth: BluetoothReport,
    pub notes: Vec<String>,
}

impl DoctorReport {
    pub fn backend(&self, id: BackendId) -> Option<&BackendStatus> {
        self.backends.iter().find(|b| b.id == id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NetworkReport {
    /// Interfaces that are up, have carrier and multicast, and carry an address.
    pub lan_interfaces: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WifiReport {
    pub present: bool,
    pub phys: Vec<WifiPhy>,
    /// Set when probing failed (e.g. nl80211 unavailable) although hardware exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WifiPhy {
    pub name: String,
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// `vendor:device` (PCI or USB), lowercase hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hw_id: Option<String>,
    /// nl80211 iftype names: station, ap, monitor, p2p-client, p2p-go, p2p-device, nan, nan-data…
    pub iftypes: Vec<String>,
    pub combinations: Vec<IfaceCombination>,
    pub interfaces: Vec<WifiInterface>,
    /// Regulatory state of the channels AWDL shared mode needs (6, 44, 149).
    pub channels: Vec<ChannelStatus>,
}

impl WifiPhy {
    pub fn supports(&self, iftype: &str) -> bool {
        self.iftypes.iter().any(|t| t == iftype)
    }

    /// Whether some interface combination allows one interface of each type at once.
    pub fn concurrent(&self, a: &str, b: &str) -> bool {
        self.combinations.iter().any(|c| c.allows_both(a, b))
    }

    /// Channel of the first connected station interface.
    pub fn station_channel(&self) -> Option<u32> {
        self.interfaces.iter().find(|i| i.iftype == "station").and_then(|i| i.channel)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IfaceCombination {
    pub limits: Vec<IfaceLimit>,
    pub max_interfaces: u32,
    pub num_channels: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IfaceLimit {
    pub max: u32,
    pub iftypes: Vec<String>,
}

impl IfaceCombination {
    fn allows_both(&self, a: &str, b: &str) -> bool {
        if self.max_interfaces < 2 {
            return false;
        }
        let holds = |t: &str| self.limits.iter().position(|l| l.max >= 1 && l.iftypes.iter().any(|x| x == t));
        match (holds(a), holds(b)) {
            (Some(i), Some(j)) if i != j => true,
            (Some(i), Some(_)) => self.limits[i].max >= 2,
            _ => false,
        }
    }

    /// `#{station} ≤ 1, #{ap, p2p-go} ≤ 1; total ≤ 3; channels ≤ 2` (as `iw list` prints it).
    pub fn describe(&self) -> String {
        let limits: Vec<String> =
            self.limits.iter().map(|l| format!("#{{{}}} <= {}", l.iftypes.join(", "), l.max)).collect();
        format!("{}; total <= {}; channels <= {}", limits.join(", "), self.max_interfaces, self.num_channels)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WifiInterface {
    pub name: String,
    pub iftype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_mhz: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelStatus {
    pub channel: u32,
    pub freq_mhz: u32,
    /// Neither disabled nor no-IR in the current regulatory domain.
    pub usable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BluetoothReport {
    pub bluez_running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bluez_version: Option<String>,
    /// `org.bluez.ProfileManager1`, needed for the RFCOMM medium.
    pub profile_manager: bool,
    pub adapters: Vec<BtAdapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BtAdapter {
    pub name: String,
    pub address: String,
    pub powered: bool,
    /// From `org.bluez.LEAdvertisingManager1`; `None` when the adapter can't advertise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adv_instances_supported: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adv_instances_active: Option<u8>,
}

impl BluetoothReport {
    /// The adapter ldrop uses: the first powered one, else the first one.
    pub fn primary(&self) -> Option<&BtAdapter> {
        self.adapters.iter().find(|a| a.powered).or(self.adapters.first())
    }

    fn can_advertise(&self) -> bool {
        self.primary().is_some_and(|a| a.powered && a.adv_instances_supported.unwrap_or(0) > 0)
    }
}

/// AirDrop link support of a Wi-Fi card according to Owly's hardware matrix (SPEC §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppleLinkProfile {
    /// Shared mode works (Wi-Fi stays up).
    Shared,
    /// Shared mode works but Owly keeps it opt-in.
    SharedOptIn,
    /// Exclusive mode only: Wi-Fi drops while active.
    Exclusive,
    /// USB card for active-monitor mode.
    ActiveMonitor,
    /// Listed as unreliable.
    Unreliable,
    /// Listed as failing.
    Fails,
    /// Not in the matrix.
    Unknown,
}

/// Built-in snapshot of Owly's Oct 2026 matrix [R27]. M6 replaces it with Owly's own
/// `drivers.toml`, which is authoritative.
pub fn apple_link_profile(driver: Option<&str>, hw_id: Option<&str>) -> AppleLinkProfile {
    use AppleLinkProfile::*;
    let device = hw_id.and_then(|id| id.split(':').nth(1)).unwrap_or("");
    match driver.unwrap_or("") {
        "mt7925e" if device == "7927" => Fails,
        "mt7925e" => Shared,
        "mt7921e" => Unreliable,
        "rtw89_8852ae" | "rtw89_8852be" | "rtw89_8852ce" => SharedOptIn,
        "rtw88_8822be" => Exclusive,
        "rtw88_8821ce" => Fails,
        "rtl8xxxu" => Unreliable,
        "mt76x2u" => ActiveMonitor,
        "rtw89_8852bu" => Exclusive,
        "ath11k_pci" | "ath12k_pci" => Fails,
        // Intel 7265 (095a/095b), 8260 (24f3/24f4), 8265 (24fd) fail; 9260 and newer work.
        "iwlwifi" if matches!(device, "095a" | "095b" | "24f3" | "24f4" | "24fd") => Fails,
        "iwlwifi" => Exclusive,
        _ => Unknown,
    }
}

/// Inputs to [`evaluate`] that come from config rather than hardware.
#[derive(Debug, Clone, Copy, Default)]
pub struct Policy {
    pub allow_exclusive_radio: bool,
}

/// Channels where AWDL shared mode can coexist with the access point (SPEC §3.2).
pub const AWDL_SHARED_CHANNELS: [u32; 3] = [6, 44, 149];

pub fn evaluate(
    network: &NetworkReport,
    wifi: &WifiReport,
    bt: &BluetoothReport,
    policy: Policy,
) -> Vec<BackendStatus> {
    let status = |id, support, reason: String| BackendStatus { id, support, reason };
    let mut out = Vec::with_capacity(BackendId::ALL.len());

    out.push(match network.lan_interfaces.as_slice() {
        [] => status(BackendId::Lan, Support::Unsupported, "no network interface with a link and an address".into()),
        ifaces => status(BackendId::Lan, Support::Supported, format!("via {}", ifaces.join(", "))),
    });

    out.push(evaluate_bt(bt));

    let Some(phy) = wifi.phys.first() else {
        let reason = match &wifi.error {
            Some(e) => format!("Wi-Fi probe failed: {e}"),
            None => "no Wi-Fi hardware".to_owned(),
        };
        for id in &BackendId::ALL[2..] {
            out.push(status(*id, Support::Unsupported, reason.clone()));
        }
        return out;
    };
    let card = describe_card(phy);

    out.push(if phy.supports("p2p-go") && phy.supports("p2p-client") {
        status(BackendId::Wfd, Support::Supported, format!("{card} supports P2P-GO and P2P-client"))
    } else {
        status(BackendId::Wfd, Support::Unsupported, format!("{card} has no Wi-Fi Direct (P2P) support"))
    });

    out.push(if !phy.supports("ap") {
        status(BackendId::Hotspot, Support::Unsupported, format!("{card} has no AP mode"))
    } else if phy.concurrent("station", "ap") {
        status(BackendId::Hotspot, Support::Supported, format!("{card} can run an AP while connected"))
    } else {
        status(
            BackendId::Hotspot,
            Support::Degraded,
            format!("{card} has AP mode but no station+AP combination: Wi-Fi drops while the hotspot runs"),
        )
    });

    let profile = apple_link_profile(phy.driver.as_deref(), phy.hw_id.as_deref());
    let no_ble = (!bt.can_advertise()).then_some("; no BLE advertising, so Apple devices can't be woken for discovery");
    let with_ble = |s: String| match no_ble {
        Some(extra) => s + extra,
        None => s,
    };
    let ble_support = |s: Support| if no_ble.is_some() && s == Support::Supported { Support::Degraded } else { s };

    out.push(match profile {
        AppleLinkProfile::Shared | AppleLinkProfile::SharedOptIn => {
            let mut support = Support::Supported;
            let mut reason = format!("{card} supports AWDL shared mode");
            if profile == AppleLinkProfile::SharedOptIn {
                support = Support::Degraded;
                reason.push_str(" (opt-in in Owly's driver profile)");
            }
            if let Some(ch) = phy.station_channel().filter(|ch| !AWDL_SHARED_CHANNELS.contains(ch)) {
                support = Support::Degraded;
                reason =
                    format!("{reason}, but Wi-Fi is on channel {ch}; shared mode needs the router on 6, 44 or 149");
            }
            status(BackendId::AwdlShared, ble_support(support), with_ble(reason))
        }
        _ => status(
            BackendId::AwdlShared,
            Support::Unsupported,
            format!("{card}: shared mode not supported ({})", profile_word(profile)),
        ),
    });

    out.push(match profile {
        AppleLinkProfile::Exclusive if phy.supports("monitor") => {
            let mut reason = format!("{card} supports AWDL only in exclusive mode: Wi-Fi disconnects while Apple devices can see this computer");
            if !policy.allow_exclusive_radio {
                reason.push_str("; enable with `ldrop config set airdrop.allow_exclusive_radio true`");
            }
            status(BackendId::AwdlExclusive, Support::Degraded, with_ble(reason))
        }
        AppleLinkProfile::Exclusive => status(BackendId::AwdlExclusive, Support::Unsupported, format!("{card} lacks monitor mode")),
        AppleLinkProfile::Shared | AppleLinkProfile::SharedOptIn => {
            status(BackendId::AwdlExclusive, Support::Unsupported, format!("{card} uses shared mode instead"))
        }
        _ => status(BackendId::AwdlExclusive, Support::Unsupported, format!("{card}: exclusive mode not supported ({})", profile_word(profile))),
    });

    out.push(match profile {
        AppleLinkProfile::ActiveMonitor => status(
            BackendId::AwdlActivemon,
            ble_support(Support::Supported),
            with_ble(format!("{card} supports active-monitor mode")),
        ),
        _ => status(
            BackendId::AwdlActivemon,
            Support::Unsupported,
            "needs an active-monitor USB card (e.g. MT7612U)".into(),
        ),
    });

    out.push(if phy.supports("nan") && phy.supports("nan-data") {
        status(
            BackendId::Nan,
            Support::Degraded,
            format!("{card} supports NAN and NAN data (experimental backend, SPEC §7)"),
        )
    } else if phy.supports("nan") {
        status(BackendId::Nan, Support::Unsupported, format!("{card} supports NAN discovery but not the NAN data path"))
    } else {
        status(BackendId::Nan, Support::Unsupported, format!("{card} has no Wi-Fi Aware (NAN) support"))
    });

    out
}

fn evaluate_bt(bt: &BluetoothReport) -> BackendStatus {
    let status = |support, reason: String| BackendStatus { id: BackendId::Bt, support, reason };
    if !bt.bluez_running {
        return status(
            Support::Unsupported,
            bt.error.clone().unwrap_or_else(|| "BlueZ (bluetoothd) is not running".into()),
        );
    }
    let Some(adapter) = bt.primary() else {
        return status(Support::Unsupported, "no Bluetooth adapter".into());
    };
    if !bt.profile_manager {
        return status(Support::Unsupported, "BlueZ has no ProfileManager1, so RFCOMM is unavailable".into());
    }
    if !adapter.powered {
        return status(Support::Degraded, format!("{} is powered off; run `bluetoothctl power on`", adapter.name));
    }
    match adapter.adv_instances_supported {
        None | Some(0) => status(
            Support::Degraded,
            format!("{} ({}) has no LE advertising: RFCOMM works, BLE wake-up doesn't", adapter.name, adapter.address),
        ),
        Some(1) => status(
            Support::Supported,
            format!(
                "{} ({}); only 1 advertising instance, so Quick Share and AirDrop adverts take turns",
                adapter.name, adapter.address
            ),
        ),
        Some(n) => {
            status(Support::Supported, format!("{} ({}), {n} advertising instances", adapter.name, adapter.address))
        }
    }
}

fn describe_card(phy: &WifiPhy) -> String {
    match (&phy.driver, &phy.hw_id) {
        (Some(d), Some(id)) => format!("{} ({d}, {id})", phy.name),
        (Some(d), None) => format!("{} ({d})", phy.name),
        _ => phy.name.clone(),
    }
}

fn profile_word(p: AppleLinkProfile) -> &'static str {
    match p {
        AppleLinkProfile::Unreliable => "listed as unreliable in Owly's matrix",
        AppleLinkProfile::Fails => "listed as failing in Owly's matrix",
        AppleLinkProfile::Unknown => "card not in Owly's matrix yet",
        AppleLinkProfile::ActiveMonitor => "use active-monitor mode",
        _ => "see Owly's matrix",
    }
}

/// Converts a frequency to its 2.4/5/6 GHz channel number.
pub fn freq_to_channel(freq_mhz: u32) -> Option<u32> {
    match freq_mhz {
        2484 => Some(14),
        2412..=2472 => Some((freq_mhz - 2407) / 5),
        5955..=7115 => Some((freq_mhz - 5950) / 5),
        5000..=5900 => Some((freq_mhz - 5000) / 5),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bt_ok() -> BluetoothReport {
        BluetoothReport {
            bluez_running: true,
            bluez_version: Some("5.87".into()),
            profile_manager: true,
            adapters: vec![BtAdapter {
                name: "hci0".into(),
                address: "00:11:22:33:44:55".into(),
                powered: true,
                adv_instances_supported: Some(4),
                adv_instances_active: Some(0),
            }],
            error: None,
        }
    }

    fn lan() -> NetworkReport {
        NetworkReport { lan_interfaces: vec!["enp6s0".into()] }
    }

    fn intel_ax211() -> WifiPhy {
        WifiPhy {
            name: "phy0".into(),
            index: 0,
            driver: Some("iwlwifi".into()),
            hw_id: Some("8086:51f0".into()),
            iftypes: ["station", "ap", "monitor", "p2p-client", "p2p-go", "p2p-device"].map(String::from).to_vec(),
            combinations: vec![IfaceCombination {
                limits: vec![
                    IfaceLimit { max: 1, iftypes: vec!["station".into()] },
                    IfaceLimit { max: 1, iftypes: vec!["ap".into(), "p2p-client".into(), "p2p-go".into()] },
                    IfaceLimit { max: 1, iftypes: vec!["p2p-device".into()] },
                ],
                max_interfaces: 3,
                num_channels: 2,
            }],
            interfaces: vec![WifiInterface {
                name: "wlan0".into(),
                iftype: "station".into(),
                freq_mhz: Some(2437),
                channel: Some(6),
            }],
            channels: vec![],
        }
    }

    fn get(v: &[BackendStatus], id: BackendId) -> &BackendStatus {
        v.iter().find(|b| b.id == id).unwrap()
    }

    #[test]
    fn dev_desktop_without_wifi() {
        let r = evaluate(&lan(), &WifiReport::default(), &bt_ok(), Policy::default());
        assert_eq!(r.len(), BackendId::ALL.len());
        assert_eq!(get(&r, BackendId::Lan).support, Support::Supported);
        assert_eq!(get(&r, BackendId::Bt).support, Support::Supported);
        for id in &BackendId::ALL[2..] {
            let b = get(&r, *id);
            assert_eq!(b.support, Support::Unsupported);
            assert_eq!(b.reason, "no Wi-Fi hardware");
        }
    }

    #[test]
    fn intel_is_exclusive_only() {
        let wifi = WifiReport { present: true, phys: vec![intel_ax211()], error: None };
        let r = evaluate(&lan(), &wifi, &bt_ok(), Policy::default());
        assert_eq!(get(&r, BackendId::Wfd).support, Support::Supported);
        assert_eq!(get(&r, BackendId::Hotspot).support, Support::Supported);
        assert_eq!(get(&r, BackendId::AwdlShared).support, Support::Unsupported);
        let excl = get(&r, BackendId::AwdlExclusive);
        assert_eq!(excl.support, Support::Degraded);
        assert!(excl.reason.contains("allow_exclusive_radio"));
        let r = evaluate(&lan(), &wifi, &bt_ok(), Policy { allow_exclusive_radio: true });
        assert!(!get(&r, BackendId::AwdlExclusive).reason.contains("allow_exclusive_radio"));
        assert_eq!(get(&r, BackendId::Nan).support, Support::Unsupported);
    }

    #[test]
    fn old_intel_fails() {
        let mut phy = intel_ax211();
        phy.hw_id = Some("8086:24fd".into());
        let wifi = WifiReport { present: true, phys: vec![phy], error: None };
        let r = evaluate(&lan(), &wifi, &bt_ok(), Policy::default());
        assert_eq!(get(&r, BackendId::AwdlExclusive).support, Support::Unsupported);
    }

    #[test]
    fn mt7925_shared_mode_depends_on_channel() {
        let mut phy = intel_ax211();
        phy.driver = Some("mt7925e".into());
        phy.hw_id = Some("14c3:7925".into());
        let mut wifi = WifiReport { present: true, phys: vec![phy], error: None };
        let r = evaluate(&lan(), &wifi, &bt_ok(), Policy::default());
        assert_eq!(get(&r, BackendId::AwdlShared).support, Support::Supported);

        wifi.phys[0].interfaces[0].channel = Some(1);
        let r = evaluate(&lan(), &wifi, &bt_ok(), Policy::default());
        let shared = get(&r, BackendId::AwdlShared);
        assert_eq!(shared.support, Support::Degraded);
        assert!(shared.reason.contains("channel 1"), "{}", shared.reason);
    }

    #[test]
    fn no_ble_degrades_apple_backends() {
        let mut phy = intel_ax211();
        phy.driver = Some("mt7925e".into());
        let wifi = WifiReport { present: true, phys: vec![phy], error: None };
        let mut bt = bt_ok();
        bt.adapters[0].adv_instances_supported = None;
        let r = evaluate(&lan(), &wifi, &bt, Policy::default());
        assert_eq!(get(&r, BackendId::Bt).support, Support::Degraded);
        assert_eq!(get(&r, BackendId::AwdlShared).support, Support::Degraded);
    }

    #[test]
    fn bluetooth_states() {
        let mut bt = bt_ok();
        bt.adapters[0].powered = false;
        assert_eq!(evaluate_bt(&bt).support, Support::Degraded);
        bt.adapters.clear();
        assert_eq!(evaluate_bt(&bt).support, Support::Unsupported);
        assert_eq!(evaluate_bt(&BluetoothReport::default()).support, Support::Unsupported);
    }

    #[test]
    fn hotspot_without_concurrency_is_degraded() {
        let mut phy = intel_ax211();
        phy.combinations[0].limits.remove(1);
        phy.combinations[0].limits[0].iftypes.push("ap".into());
        let wifi = WifiReport { present: true, phys: vec![phy], error: None };
        let r = evaluate(&lan(), &wifi, &bt_ok(), Policy::default());
        assert_eq!(get(&r, BackendId::Hotspot).support, Support::Degraded);
    }

    #[test]
    fn channels_from_frequencies() {
        assert_eq!(freq_to_channel(2437), Some(6));
        assert_eq!(freq_to_channel(2484), Some(14));
        assert_eq!(freq_to_channel(5220), Some(44));
        assert_eq!(freq_to_channel(5745), Some(149));
        assert_eq!(freq_to_channel(5955), Some(1));
        assert_eq!(freq_to_channel(900), None);
    }
}
