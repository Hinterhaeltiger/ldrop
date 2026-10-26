// SPDX-License-Identifier: GPL-3.0-or-later
//! System probe behind `Manager.Capabilities` and `ldrop doctor` (SPEC §3.5). Read-only.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ldrop_core::capabilities::{
    BluetoothReport, DoctorReport, NetworkReport, OwlyReport, Policy, WifiReport, apply_owly, evaluate,
};
use nix::ifaddrs::getifaddrs;
use nix::net::if_::InterfaceFlags;
use nix::sys::socket::{AddressFamily, SockaddrLike};

use crate::manager;
use crate::state::Shared;

/// Background re-probe interval; reads re-probe on their own when the cache is stale.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

pub async fn probe(policy: Policy, hardware: bool) -> DoctorReport {
    let (wifi, bluetooth) = if hardware {
        tokio::join!(ldrop_wifi::probe(), ldrop_ble::probe())
    } else {
        (WifiReport::default(), BluetoothReport::default())
    };
    // owlyd only matters with a Wi-Fi card; asking starts it (D-Bus activation).
    let owly = if hardware && wifi.present { ldrop_wifi::owly::report().await } else { OwlyReport::default() };
    let network = lan_report();
    let mut backends = evaluate(&network, &wifi, &bluetooth, policy);
    apply_owly(&mut backends, &owly, policy, &bluetooth);
    let mut notes = vec![
        "Receiving needs inbound mDNS (5353/udp) and the Quick Share TCP port on the LAN; \
         AirDrop needs 8771/tcp on awdl0."
            .to_owned(),
    ];
    if wifi.present && !owly.present {
        notes.push(
            "AirDrop needs Owly's radio service: install the `owly` package (Arch: `yay -S owly`; \
             .deb and .rpm on github.com/t4t5/owly/releases). AirDrop verdicts above use ldrop's \
             built-in copy of Owly's hardware matrix until then."
                .to_owned(),
        );
    }
    if owly.present && ufw_enabled() {
        notes.push(
            "ufw is on: allow AirDrop with `sudo ufw allow in on awdl0 from fe80::/10 to any port 8771 proto tcp`."
                .to_owned(),
        );
    }
    DoctorReport { backends, network, wifi, bluetooth, owly, notes }
}

/// Whether ufw's config enables it. Unreadable counts as off.
fn ufw_enabled() -> bool {
    std::fs::read_to_string("/etc/ufw/ufw.conf")
        .is_ok_and(|c| c.lines().any(|l| l.trim().eq_ignore_ascii_case("ENABLED=yes")))
}

/// Re-probes, stores the report, and emits `Capabilities` changes when the verdicts moved.
pub async fn refresh(s: &Arc<Shared>) {
    if update(s).await {
        manager::emit_capabilities(s).await;
    }
}

/// Re-probes and stores the report without emitting; returns whether it changed.
pub async fn update(s: &Arc<Shared>) -> bool {
    let policy = Policy { allow_exclusive_radio: s.state().config.airdrop.allow_exclusive_radio };
    let report = probe(policy, s.probe_hardware).await;
    let mut st = s.state();
    let changed = st.report != report;
    if changed {
        tracing::info!("capabilities: {}", summary(&report));
    }
    st.report = report;
    st.report_at = Some(Instant::now());
    changed
}

fn summary(report: &DoctorReport) -> String {
    report.backends.iter().map(|b| format!("{}={}", b.id, b.support)).collect::<Vec<_>>().join(" ")
}

/// Interfaces that are up, have carrier, do multicast, and carry an IP address.
fn lan_report() -> NetworkReport {
    let wanted = InterfaceFlags::IFF_UP | InterfaceFlags::IFF_RUNNING | InterfaceFlags::IFF_MULTICAST;
    let mut lan_interfaces: Vec<String> = Vec::new();
    if let Ok(addrs) = getifaddrs() {
        for ifa in addrs {
            if !ifa.flags.contains(wanted) || ifa.flags.contains(InterfaceFlags::IFF_LOOPBACK) {
                continue;
            }
            let has_ip = ifa
                .address
                .as_ref()
                .and_then(|a| a.family())
                .is_some_and(|f| matches!(f, AddressFamily::Inet | AddressFamily::Inet6));
            if has_ip && !lan_interfaces.contains(&ifa.interface_name) {
                lan_interfaces.push(ifa.interface_name);
            }
        }
    }
    NetworkReport { lan_interfaces }
}
