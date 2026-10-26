// SPDX-License-Identifier: GPL-3.0-or-later
//! Wi-Fi capability probe for `ldrop doctor` (SPEC §3.5): sysfs for card identity, nl80211 for
//! interface types, combinations, current channel and regulatory flags. Read-only. Temporary
//! networks for the bandwidth upgrade go through NetworkManager ([`nm`]).

pub mod nm;
pub mod owly;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use futures_util::TryStreamExt;
use ldrop_core::capabilities::{
    AWDL_SHARED_CHANNELS, ChannelStatus, IfaceCombination, IfaceLimit, WifiInterface, WifiPhy, WifiReport,
    freq_to_channel,
};
use wl_nl80211::{
    Nl80211Attr, Nl80211BandInfo, Nl80211FrequencyInfo, Nl80211IfMode, Nl80211IfaceCombAttribute,
    Nl80211IfaceCombLimitAttribute, Nl80211InterfaceType,
};

const SYSFS_IEEE80211: &str = "/sys/class/ieee80211";

/// Probes all wiphys. Never fails: problems end up in [`WifiReport::error`].
pub async fn probe() -> WifiReport {
    let mut phys = sysfs_phys(Path::new(SYSFS_IEEE80211));
    if phys.is_empty() {
        return WifiReport::default();
    }
    let error = match nl80211_fill(&mut phys).await {
        Ok(()) => None,
        Err(e) => {
            tracing::warn!("nl80211 probe failed: {e}");
            Some(e)
        }
    };
    WifiReport { present: true, phys: phys.into_values().collect(), error }
}

/// Phys from sysfs keyed by wiphy index, with name, driver and hardware id filled in.
fn sysfs_phys(root: &Path) -> BTreeMap<u32, WifiPhy> {
    let mut phys = BTreeMap::new();
    let Ok(entries) = fs::read_dir(root) else {
        return phys;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = read_trimmed(&dir.join("index")).and_then(|s| s.parse().ok()) else {
            continue;
        };
        let device = dir.join("device");
        let driver = fs::read_link(device.join("driver"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        phys.insert(index, WifiPhy { name, index, driver, hw_id: hw_id(&device), ..Default::default() });
    }
    phys
}

/// `vendor:device` for PCI (`device/vendor`) or USB (`device/../idVendor`) cards.
fn hw_id(device: &Path) -> Option<String> {
    let hex = |p: &Path| read_trimmed(p).map(|s| s.trim_start_matches("0x").to_lowercase());
    if let (Some(v), Some(d)) = (hex(&device.join("vendor")), hex(&device.join("device"))) {
        return Some(format!("{v}:{d}"));
    }
    let usb = device.join("..");
    match (hex(&usb.join("idVendor")), hex(&usb.join("idProduct"))) {
        (Some(v), Some(d)) => Some(format!("{v}:{d}")),
        _ => None,
    }
}

fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

async fn nl80211_fill(phys: &mut BTreeMap<u32, WifiPhy>) -> Result<(), String> {
    let (connection, handle, _) = wl_nl80211::new_connection().map_err(|e| e.to_string())?;
    let conn_task = tokio::spawn(connection);
    let result = async {
        // Split dumps deliver one wiphy over several messages; merge by index.
        let mut stream = handle.wireless_physic().get().execute().await;
        while let Some(msg) = stream.try_next().await.map_err(|e| e.to_string())? {
            let attrs = &msg.payload.attributes;
            let Some(index) = attrs.iter().find_map(|a| match a {
                Nl80211Attr::Wiphy(i) => Some(*i),
                _ => None,
            }) else {
                continue;
            };
            let phy = phys.entry(index).or_insert_with(|| WifiPhy { index, ..Default::default() });
            merge_wiphy_attrs(phy, attrs);
        }

        let mut stream = handle.interface().get(Vec::new()).execute().await;
        while let Some(msg) = stream.try_next().await.map_err(|e| e.to_string())? {
            let attrs = &msg.payload.attributes;
            let mut wiphy = None;
            let mut iface = WifiInterface::default();
            for attr in attrs {
                match attr {
                    Nl80211Attr::Wiphy(i) => wiphy = Some(*i),
                    Nl80211Attr::IfName(n) => iface.name = n.clone(),
                    Nl80211Attr::IfType(t) => iface.iftype = iface_type_name(t),
                    Nl80211Attr::WiphyFreq(f) => {
                        iface.freq_mhz = Some(*f);
                        iface.channel = freq_to_channel(*f);
                    }
                    _ => {}
                }
            }
            if let Some(phy) = wiphy.and_then(|i| phys.get_mut(&i)) {
                phy.interfaces.push(iface);
            }
        }
        Ok(())
    }
    .await;
    conn_task.abort();
    result
}

fn merge_wiphy_attrs(phy: &mut WifiPhy, attrs: &[Nl80211Attr]) {
    for attr in attrs {
        match attr {
            Nl80211Attr::WiphyName(n) if phy.name.is_empty() => phy.name = n.clone(),
            Nl80211Attr::SupportedIftypes(modes) => {
                phy.iftypes = modes.iter().map(if_mode_name).collect();
            }
            Nl80211Attr::InterfaceCombination(combs) => {
                phy.combinations.extend(combs.iter().map(|c| {
                    let mut out = IfaceCombination::default();
                    for a in &c.attributes {
                        match a {
                            Nl80211IfaceCombAttribute::Limits(limits) => {
                                out.limits = limits
                                    .iter()
                                    .map(|l| {
                                        let mut limit = IfaceLimit::default();
                                        for la in &l.attributes {
                                            match la {
                                                Nl80211IfaceCombLimitAttribute::Max(m) => limit.max = *m,
                                                Nl80211IfaceCombLimitAttribute::Iftypes(t) => {
                                                    limit.iftypes = t.iter().map(iface_type_name).collect();
                                                }
                                                _ => {}
                                            }
                                        }
                                        limit
                                    })
                                    .collect();
                            }
                            Nl80211IfaceCombAttribute::Maxnum(n) => out.max_interfaces = *n,
                            Nl80211IfaceCombAttribute::NumChannels(n) => out.num_channels = *n,
                            _ => {}
                        }
                    }
                    out
                }));
            }
            Nl80211Attr::WiphyBands(bands) => {
                for info in bands.iter().flat_map(|b| &b.info) {
                    let Nl80211BandInfo::Freqs(freqs) = info else { continue };
                    for f in freqs {
                        let mut freq = None;
                        let mut usable = true;
                        for fi in &f.info {
                            match fi {
                                Nl80211FrequencyInfo::Freq(mhz) => freq = Some(*mhz),
                                Nl80211FrequencyInfo::Disabled | Nl80211FrequencyInfo::NoIr => usable = false,
                                _ => {}
                            }
                        }
                        let Some(freq_mhz) = freq else { continue };
                        let Some(channel) = freq_to_channel(freq_mhz) else { continue };
                        // Channels 6/44/149 only exist once across 2.4/5 GHz; skip 6 GHz duplicates.
                        if AWDL_SHARED_CHANNELS.contains(&channel)
                            && freq_mhz < 5900
                            && !phy.channels.iter().any(|c| c.freq_mhz == freq_mhz)
                        {
                            phy.channels.push(ChannelStatus { channel, freq_mhz, usable });
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn if_mode_name(m: &Nl80211IfMode) -> String {
    match m {
        Nl80211IfMode::Adhoc => "adhoc".into(),
        Nl80211IfMode::Station => "station".into(),
        Nl80211IfMode::Ap => "ap".into(),
        Nl80211IfMode::ApVlan => "ap-vlan".into(),
        Nl80211IfMode::Wds => "wds".into(),
        Nl80211IfMode::Monitor => "monitor".into(),
        Nl80211IfMode::MeshPoint => "mesh".into(),
        Nl80211IfMode::P2pClient => "p2p-client".into(),
        Nl80211IfMode::P2pGo => "p2p-go".into(),
        Nl80211IfMode::P2pDevice => "p2p-device".into(),
        Nl80211IfMode::Ocb => "ocb".into(),
        Nl80211IfMode::Nan => "nan".into(),
        Nl80211IfMode::Other(n) => other_iftype_name(u32::from(*n)),
    }
}

fn iface_type_name(t: &Nl80211InterfaceType) -> String {
    match t {
        Nl80211InterfaceType::Unspecified => "unspecified".into(),
        Nl80211InterfaceType::Adhoc => "adhoc".into(),
        Nl80211InterfaceType::Station => "station".into(),
        Nl80211InterfaceType::Ap => "ap".into(),
        Nl80211InterfaceType::ApVlan => "ap-vlan".into(),
        Nl80211InterfaceType::Wds => "wds".into(),
        Nl80211InterfaceType::Monitor => "monitor".into(),
        Nl80211InterfaceType::MeshPoint => "mesh".into(),
        Nl80211InterfaceType::P2pClient => "p2p-client".into(),
        Nl80211InterfaceType::P2pGo => "p2p-go".into(),
        Nl80211InterfaceType::P2pDevice => "p2p-device".into(),
        Nl80211InterfaceType::Ocb => "ocb".into(),
        Nl80211InterfaceType::Nan => "nan".into(),
        Nl80211InterfaceType::Other(n) => other_iftype_name(*n),
    }
}

/// Iftypes newer than wl-nl80211 knows (`enum nl80211_iftype` in linux/nl80211.h).
fn other_iftype_name(n: u32) -> String {
    match n {
        13 => "nan-data".into(),
        14 => "pd".into(),
        n => format!("iftype-{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sysfs_layout_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let phy = root.join("phy0");
        let pci = root.join("pci0000:00/0000:00:14.3");
        fs::create_dir_all(&phy).unwrap();
        fs::create_dir_all(&pci).unwrap();
        fs::create_dir_all(root.join("drivers/iwlwifi")).unwrap();
        fs::write(phy.join("index"), "0\n").unwrap();
        fs::write(pci.join("vendor"), "0x8086\n").unwrap();
        fs::write(pci.join("device"), "0x51F0\n").unwrap();
        std::os::unix::fs::symlink(&pci, phy.join("device")).unwrap();
        std::os::unix::fs::symlink(root.join("drivers/iwlwifi"), pci.join("driver")).unwrap();

        let phys = sysfs_phys(root);
        let p = &phys[&0];
        assert_eq!(p.name, "phy0");
        assert_eq!(p.driver.as_deref(), Some("iwlwifi"));
        assert_eq!(p.hw_id.as_deref(), Some("8086:51f0"));
    }

    #[test]
    fn missing_sysfs_means_no_wifi() {
        assert!(sysfs_phys(Path::new("/nonexistent/ieee80211")).is_empty());
    }

    #[test]
    fn new_iftypes_get_names() {
        assert_eq!(iface_type_name(&Nl80211InterfaceType::Other(13)), "nan-data");
        assert_eq!(if_mode_name(&Nl80211IfMode::Other(99)), "iftype-99");
    }
}
