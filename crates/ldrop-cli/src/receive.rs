// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop receive`: a foreground agent. Shows each request with its PIN, asks y/N, and shows
//! the progress of incoming transfers. There is no auto-accept, by design.

use std::collections::HashMap;
use std::io::{IsTerminal, Write};

use ldrop_client::{Client, Decision, PendingRequest, TransferEvent, TransferInfo};
use ldrop_core::time::format_countdown;
use ldrop_core::{Direction, TransferState, Visibility};
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

use crate::fmt;
use crate::{CmdResult, Exit, Fail};

pub async fn run() -> CmdResult {
    let client = Client::connect().await?;
    let status = client.status().await?;
    let (registration, mut requests) = client.register_agent_channel().await?;
    let mut watcher = client.watch_transfers().await?;
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut shown: HashMap<u32, TransferState> = watcher.transfers().map(|t| (t.id, t.state)).collect();

    eprintln!("Receiving as “{}”. Ctrl-C to stop.", status.device_name);
    if status.visibility == Visibility::Hidden {
        eprintln!("This computer is hidden; run `ldrop visible` so nearby devices can find it.");
    }

    let daemon_exited = client.daemon_exited();
    tokio::pin!(daemon_exited);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut daemon_exited => return Err(Fail::new(Exit::Error, "ldropd stopped")),
            pending = requests.recv() => {
                let Some(pending) = pending else { break };
                if !ask(pending, &mut stdin).await {
                    break;
                }
            }
            event = watcher.next() => match event {
                None => break,
                Some(TransferEvent::Added(t)) | Some(TransferEvent::Changed(t)) => show(&t, &mut shown),
                Some(TransferEvent::Removed(_)) => {}
            }
        }
    }
    let _ = registration.unregister().await;
    Ok(Exit::Ok)
}

/// Asks about one request. Returns false when the user pressed Ctrl-C.
async fn ask(mut pending: PendingRequest, stdin: &mut Lines<BufReader<Stdin>>) -> bool {
    let r = pending.request.clone();
    eprintln!();
    eprintln!("{} wants to share {}", fmt::peer(&r.peer), fmt::items(&r.items, r.bytes_total));
    for item in r.items.iter().take(10) {
        match &item.text {
            Some(text) if !text.is_empty() => eprintln!("  {text}"),
            _ => eprintln!("  {}", item.name),
        }
    }
    if r.items.len() > 10 {
        eprintln!("  … and {} more", r.items.len() - 10);
    }
    if !r.pin.is_empty() {
        eprintln!("PIN {} (check that the sender shows the same PIN)", r.pin);
    }
    eprint!("Accept? [y/N] ({} left) ", format_countdown(r.time_left()));
    let _ = std::io::stderr().flush();

    tokio::select! {
        line = stdin.next_line() => {
            let yes = matches!(line, Ok(Some(ref l)) if matches!(l.trim(), "y" | "Y" | "yes" | "Yes"));
            pending.respond(if yes { Decision::Accept } else { Decision::Decline });
            if !yes {
                eprintln!("Declined");
            }
            true
        }
        _ = pending.expired() => {
            eprintln!();
            eprintln!("Request closed (timed out, or answered elsewhere)");
            true
        }
        _ = tokio::signal::ctrl_c() => {
            pending.respond(Decision::Decline);
            eprintln!();
            false
        }
    }
}

/// Prints how incoming transfers progress after the user answered.
fn show(t: &TransferInfo, shown: &mut HashMap<u32, TransferState>) {
    if t.direction != Direction::Incoming {
        return;
    }
    let before = shown.insert(t.id, t.state);
    let changed = before != Some(t.state);
    let tty = std::io::stderr().is_terminal();
    match t.state {
        TransferState::Transferring if tty => {
            eprint!("\r\x1b[2KReceiving… {}%", fmt::percent(t));
            let _ = std::io::stderr().flush();
        }
        TransferState::Transferring if changed => eprintln!("Receiving…"),
        TransferState::Completed if changed => {
            if tty && before == Some(TransferState::Transferring) {
                eprintln!();
            }
            eprintln!("Received {} from {}", fmt::items(&t.items, t.bytes_total), t.peer.name);
            for item in &t.items {
                if let Some(path) = &item.path {
                    println!("{path}");
                } else if let Some(text) = &item.text {
                    println!("{text}");
                }
            }
        }
        TransferState::Failed | TransferState::Cancelled if changed && before.is_some() => {
            if tty && before == Some(TransferState::Transferring) {
                eprintln!();
            }
            let reason = if t.error.is_empty() { String::new() } else { format!(": {}", t.error) };
            eprintln!("Transfer from {} {}{reason}", t.peer.name, fmt::state(t.state));
        }
        _ => {}
    }
}
