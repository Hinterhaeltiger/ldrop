// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop send`: pick a device, send, and follow the transfer to the end.

use std::io::{IsTerminal, Write};
use std::time::Duration;

use ldrop_client::{Client, DiscoveryEvent, DiscoverySession, TransferInfo};
use ldrop_core::{Device, TransferState, format_size};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::fmt;
use crate::{CmdResult, Exit, Fail, SendArgs};

pub async fn run(args: SendArgs) -> CmdResult {
    for f in &args.files {
        if !f.is_file() {
            return Err(Fail::new(Exit::Error, format!("{}: not a readable file", f.display())));
        }
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if args.to.is_none() && !interactive {
        return Err(Fail::new(Exit::Usage, "--to is required when not running in a terminal"));
    }

    let client = Client::connect().await?;
    let mut session = client.discover().await?;
    let device = match &args.to {
        Some(to) => wait_for(&mut session, to, args.timeout).await,
        None => pick(&mut session).await,
    };
    let device = match device {
        Ok(d) => d,
        Err(e) => {
            let _ = session.stop().await;
            return Err(e);
        }
    };

    let sent = if let Some(text) = &args.text {
        client.send_text(&device.id, text, false).await
    } else if let Some(url) = &args.url {
        client.send_text(&device.id, url, true).await
    } else {
        client.send_files(&device.id, &args.files).await
    };
    let _ = session.stop().await;
    let path = sent?;

    let mut progress = Progress::new(device.name.clone());
    let transfer = path.as_ref();
    let followed = tokio::select! {
        result = client.follow(&transfer, |t| progress.show(t)) => result?,
        _ = client.daemon_exited() => return Err(Fail::new(Exit::Error, "ldropd stopped during the transfer")),
        _ = tokio::signal::ctrl_c() => {
            progress.end_line();
            client.cancel(&path.as_ref()).await?;
            eprintln!("Cancelled");
            return Ok(Exit::Cancelled);
        }
    };
    Ok(match followed.state {
        TransferState::Completed => Exit::Ok,
        TransferState::Rejected => Exit::Declined,
        TransferState::Cancelled => Exit::Cancelled,
        _ => Exit::Error,
    })
}

fn matches(d: &Device, to: &str) -> bool {
    d.id == to || d.name.eq_ignore_ascii_case(to)
}

async fn wait_for(session: &mut DiscoverySession, to: &str, timeout: Duration) -> Result<Device, Fail> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                return Err(Fail::new(Exit::Error, format!(
                    "no device `{to}` found within {}s (see `ldrop devices`)", timeout.as_secs()
                )));
            }
            _ = tokio::signal::ctrl_c() => return Err(Fail::new(Exit::Cancelled, "cancelled")),
            event = session.next() => match event {
                None => return Err(Fail::new(Exit::Error, "connection to ldropd closed")),
                Some(DiscoveryEvent::Found(d)) if matches(&d, to) => return Ok(d),
                Some(_) => {}
            }
        }
    }
}

/// Lists devices as they appear; the user types a number.
async fn pick(session: &mut DiscoverySession) -> Result<Device, Fail> {
    eprintln!("Looking for nearby devices… type a number and press Enter (Ctrl-C to quit).");
    eprintln!("iPhone, iPad or Mac? Set AirDrop to “Everyone for 10 minutes”.");
    let mut devices: Vec<Device> = Vec::new();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Err(Fail::new(Exit::Cancelled, "cancelled")),
            event = session.next() => match event {
                None => return Err(Fail::new(Exit::Error, "connection to ldropd closed")),
                Some(DiscoveryEvent::Found(d)) => {
                    if let Some(known) = devices.iter_mut().find(|x| x.id == d.id) {
                        *known = d;
                    } else {
                        eprintln!("  [{}] {}", devices.len() + 1, fmt::device_line(&d));
                        devices.push(d);
                    }
                }
                Some(DiscoveryEvent::Lost(id)) => {
                    if let Some(d) = devices.iter().position(|x| x.id == id) {
                        eprintln!("  [{}] {} is no longer nearby", d + 1, devices[d].name);
                    }
                }
            },
            line = lines.next_line() => {
                let Ok(Some(line)) = line else {
                    return Err(Fail::new(Exit::Cancelled, "no device chosen"));
                };
                match line.trim().parse::<usize>() {
                    Ok(n) if (1..=devices.len()).contains(&n) => return Ok(devices[n - 1].clone()),
                    _ if line.trim().is_empty() => {}
                    _ => eprintln!("Type a number from the list."),
                }
            }
        }
    }
}

/// Prints state changes; on a terminal, a live progress line.
struct Progress {
    peer: String,
    last: Option<TransferState>,
    tty: bool,
    open_line: bool,
}

impl Progress {
    fn new(peer: String) -> Self {
        Progress { peer, last: None, tty: std::io::stderr().is_terminal(), open_line: false }
    }

    fn end_line(&mut self) {
        if self.open_line {
            eprintln!();
            self.open_line = false;
        }
    }

    fn show(&mut self, t: &TransferInfo) {
        let changed = self.last != Some(t.state);
        self.last = Some(t.state);
        if t.state == TransferState::Transferring {
            if self.tty {
                eprint!(
                    "\r\x1b[2KSending… {}% ({} of {})",
                    fmt::percent(t),
                    format_size(t.bytes_done),
                    format_size(t.bytes_total)
                );
                let _ = std::io::stderr().flush();
                self.open_line = true;
            } else if changed {
                eprintln!("Sending…");
            }
            return;
        }
        if !changed {
            return;
        }
        self.end_line();
        let peer = &self.peer;
        match t.state {
            TransferState::Connecting => eprintln!("Connecting to {peer}…"),
            TransferState::AwaitingAcceptance => {
                if !t.pin.is_empty() {
                    eprintln!("PIN {} (check that {peer} shows the same PIN)", t.pin);
                }
                eprintln!("Waiting for {peer} to accept…");
            }
            TransferState::Transferring => {}
            TransferState::Completed => eprintln!("Sent {} to {peer}", fmt::items(&t.items, t.bytes_total)),
            TransferState::Rejected => eprintln!("{peer} declined"),
            TransferState::Cancelled => eprintln!("Cancelled"),
            TransferState::Failed => {
                eprintln!("Failed: {}", if t.error.is_empty() { "unknown error" } else { &t.error })
            }
        }
    }
}
