// SPDX-License-Identifier: GPL-3.0-or-later
//! Human-readable output shared by the commands.

use std::io::IsTerminal;

use ldrop_client::TransferInfo;
use ldrop_core::time::{format_countdown, remaining};
use ldrop_core::{Device, Direction, Peer, Platform, Protocol, TransferState, describe_items, format_size};
use serde_json::{Value, json};

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}

pub fn platform(p: Platform) -> &'static str {
    match p {
        Platform::Android => "Android",
        Platform::Apple => "Apple",
        Platform::Linux => "Linux",
    }
}

pub fn protocol(p: Protocol) -> &'static str {
    match p {
        Protocol::QuickShare => "Quick Share",
        Protocol::AirDrop => "AirDrop",
    }
}

pub fn peer(p: &Peer) -> String {
    let name = if p.name.is_empty() { "Unknown device" } else { &p.name };
    format!("{name} ({} · {})", platform(p.platform), protocol(p.protocol))
}

pub fn device_line(d: &Device) -> String {
    format!("{:<24} {:<8} {:<8} {}", d.name, d.kind, platform(d.platform), protocol(d.protocol))
}

pub fn device_json(d: &Device) -> Value {
    serde_json::to_value(d).unwrap_or(Value::Null)
}

pub fn state(s: TransferState) -> &'static str {
    match s {
        TransferState::Connecting => "connecting",
        TransferState::AwaitingAcceptance => "waiting for answer",
        TransferState::Transferring => "transferring",
        TransferState::Completed => "completed",
        TransferState::Rejected => "declined",
        TransferState::Cancelled => "cancelled",
        TransferState::Failed => "failed",
    }
}

pub fn percent(t: &TransferInfo) -> u64 {
    match t.bytes_total {
        0 => 0,
        total => t.bytes_done.min(total) * 100 / total,
    }
}

/// `3 files (12 MB)` / `a link`.
pub fn items(t_items: &[ldrop_core::Item], bytes_total: u64) -> String {
    let what = describe_items(t_items);
    if t_items.iter().all(|i| i.text.is_some()) { what } else { format!("{what} ({})", format_size(bytes_total)) }
}

pub fn transfer_line(t: &TransferInfo) -> String {
    let arrow = match t.direction {
        Direction::Incoming => "from",
        Direction::Outgoing => "to",
    };
    let mut state = state(t.state).to_owned();
    if t.state == TransferState::Transferring {
        state = format!("{state} {}%", percent(t));
    }
    if t.needs_answer()
        && let Some(left) = remaining(t.deadline)
    {
        state = format!("{state} ({} left)", format_countdown(left));
    }
    if !t.error.is_empty() {
        state = format!("{state}: {}", t.error);
    }
    format!("{:>4}  {:<4} {:<36} {:<22} {}", t.id, arrow, peer(&t.peer), items(&t.items, t.bytes_total), state)
}

pub fn transfer_json(t: &TransferInfo) -> Value {
    json!({
        "id": t.id,
        "path": t.path.as_str(),
        "direction": t.direction,
        "state": t.state,
        "peer": t.peer,
        "pin": t.pin,
        "items": t.items,
        "bytes_total": t.bytes_total,
        "bytes_done": t.bytes_done,
        "error": t.error,
        "deadline": t.deadline,
        "created_at": t.created_at,
    })
}

pub fn print_json(v: &Value) {
    println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
}

pub fn print_json_line(v: &Value) {
    println!("{}", serde_json::to_string(v).unwrap_or_default());
}
