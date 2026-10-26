// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop doctor` (SPEC §3.5): what the hardware supports, and why.

use ldrop_client::Client;
use ldrop_core::capabilities::{DoctorReport, Support};

use crate::fmt::print_json;
use crate::{CmdResult, Exit, Fail};

pub async fn run(json: bool, card_test: bool) -> CmdResult {
    if card_test {
        return Err(Fail::new(
            Exit::Unavailable,
            "--card-test needs ldrop-netd, which arrives with the Apple link (milestone M6)",
        ));
    }
    let report = Client::connect().await?.capabilities().await?;
    if json {
        print_json(&serde_json::to_value(&report).unwrap_or_default());
    } else {
        print!("{}", render(&report));
    }
    Ok(Exit::Ok)
}

fn mark(s: Support) -> &'static str {
    match s {
        Support::Supported => "ok",
        Support::Degraded => "degraded",
        Support::Unsupported => "no",
    }
}

pub fn render(r: &DoctorReport) -> String {
    let mut out = String::from("Links\n");
    for b in &r.backends {
        out += &format!("  {:<15} {:<9} {}\n", b.id, mark(b.support), b.reason);
    }

    out += "\nNetwork\n";
    if r.network.lan_interfaces.is_empty() {
        out += "  no usable LAN interface\n";
    } else {
        out += &format!("  LAN: {}\n", r.network.lan_interfaces.join(", "));
    }

    out += "\nWi-Fi\n";
    if let Some(e) = &r.wifi.error {
        out += &format!("  probe failed: {e}\n");
    }
    if !r.wifi.present {
        out += "  no Wi-Fi hardware found\n";
    }
    for phy in &r.wifi.phys {
        let mut ident = Vec::new();
        ident.extend(phy.driver.clone());
        ident.extend(phy.hw_id.clone());
        out += &format!("  {} ({})\n", phy.name, if ident.is_empty() { "unknown".into() } else { ident.join(", ") });
        out += &format!("    interface types: {}\n", phy.iftypes.join(", "));
        for c in &phy.combinations {
            out += &format!("    combination: {}\n", c.describe());
        }
        for i in &phy.interfaces {
            let chan = i.channel.map(|c| format!(", channel {c}")).unwrap_or_default();
            out += &format!("    {}: {}{chan}\n", i.name, i.iftype);
        }
        let channels: Vec<String> =
            phy.channels.iter().map(|c| format!("{}{}", c.channel, if c.usable { "" } else { " (blocked)" })).collect();
        if !channels.is_empty() {
            out += &format!("    AirDrop channels: {}\n", channels.join(", "));
        }
    }

    if r.wifi.present {
        out += "\nOwly (owlyd)\n";
        let o = &r.owly;
        if let Some(e) = &o.error {
            out += &format!("  {e}\n");
        } else if !o.present {
            out += "  not installed\n";
        } else {
            let card = [&o.station, &o.driver, &o.device].into_iter().flatten().cloned().collect::<Vec<_>>();
            let card = if card.is_empty() { "no card".to_owned() } else { card.join(", ") };
            let mode = o.mode.as_deref().unwrap_or("no mode");
            let status = o.status.as_deref().map(|s| format!(", {s}")).unwrap_or_default();
            let channel = o.channel.map(|c| format!(" on channel {c}")).unwrap_or_default();
            out += &format!("  {card}: {mode}{channel}{status}\n");
            for (_, say) in &o.missing {
                out += &format!("  missing: {say}\n");
            }
            for issue in &o.issues {
                out += &format!("  issue: {issue}\n");
            }
        }
    }

    out += "\nBluetooth\n";
    let b = &r.bluetooth;
    if let Some(e) = &b.error {
        out += &format!("  probe failed: {e}\n");
    }
    if !b.bluez_running {
        out += "  BlueZ is not running\n";
    } else {
        out += &format!("  BlueZ {}\n", b.bluez_version.as_deref().unwrap_or("(unknown version)"));
        if b.adapters.is_empty() {
            out += "  no adapter\n";
        }
        for a in &b.adapters {
            let adv = match (a.adv_instances_supported, a.adv_instances_active) {
                (Some(n), active) => format!("{n} advertising slots ({} in use)", active.unwrap_or(0)),
                (None, _) => "cannot advertise".into(),
            };
            let powered = if a.powered { "on" } else { "off" };
            out += &format!("  {} {}: {powered}, {adv}\n", a.name, a.address);
        }
    }

    if !r.notes.is_empty() {
        out += "\nNotes\n";
        for n in &r.notes {
            out += &format!("  - {n}\n");
        }
    }
    out
}
