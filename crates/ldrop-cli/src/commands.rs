// SPDX-License-Identifier: GPL-3.0-or-later
//! The short commands: status, visibility, devices, transfers, decisions, config.

use std::time::Duration;

use ldrop_client::{Client, DiscoveryEvent, TransferEvent, transfer_path};
use ldrop_core::paths::Paths;
use ldrop_core::time::{format_countdown, remaining};
use ldrop_core::{Visibility, config};
use serde_json::json;

use crate::fmt::{self, print_json, print_json_line};
use crate::{CmdResult, Exit, Fail};

pub async fn status(json: bool) -> CmdResult {
    let client = Client::connect().await?;
    let status = client.status().await?;
    let active: Vec<_> = client.transfers().await?.into_iter().filter(|t| t.is_active()).collect();
    let left = remaining(status.visibility_expires_at).map(format_countdown);

    if json {
        let class = if !active.is_empty() { "transferring" } else { status.visibility.as_str() };
        let text = match (&left, active.len()) {
            (_, n) if n > 0 => format!("{n} transfer{}", if n == 1 { "" } else { "s" }),
            (Some(left), _) => format!("Visible {left}"),
            (None, _) => "Hidden".into(),
        };
        let tooltip = format!(
            "{}: {}{}",
            status.device_name,
            status.visibility,
            left.as_ref().map(|l| format!(" ({l} left)")).unwrap_or_default()
        );
        print_json_line(&json!({
            "text": text,
            "class": class,
            "alt": class,
            "tooltip": tooltip,
            "device_name": status.device_name,
            "visibility": status.visibility,
            "visibility_expires_at": status.visibility_expires_at,
            "download_dir": status.download_dir,
            "active_transfers": active.len(),
        }));
        return Ok(Exit::Ok);
    }

    println!("Device name:  {}", status.device_name);
    match left {
        Some(left) => println!("Visibility:   {} ({left} left)", status.visibility),
        None => println!("Visibility:   {}", status.visibility),
    }
    println!("Downloads:    {}", status.download_dir);
    println!("Transfers:    {} active", active.len());
    for t in &active {
        println!("  {}", fmt::transfer_line(t));
    }
    Ok(Exit::Ok)
}

pub async fn visible(duration: Option<Duration>) -> CmdResult {
    let client = Client::connect().await?;
    client.set_visibility(Visibility::Everyone, duration.unwrap_or(Duration::ZERO)).await?;
    let status = client.status().await?;
    let left = remaining(status.visibility_expires_at).unwrap_or_default();
    println!("Visible to everyone nearby for {}", format_countdown(left));
    Ok(Exit::Ok)
}

pub async fn hidden() -> CmdResult {
    Client::connect().await?.set_visibility(Visibility::Hidden, Duration::ZERO).await?;
    println!("Hidden");
    Ok(Exit::Ok)
}

pub async fn devices(watch: bool, timeout: Duration, json: bool) -> CmdResult {
    let client = Client::connect().await?;
    let mut session = client.discover().await?;
    let mut found = Vec::new();
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    if !json && fmt::stdout_is_tty() {
        eprintln!("Looking for nearby devices…");
    }
    let daemon_exited = client.daemon_exited();
    tokio::pin!(daemon_exited);
    loop {
        tokio::select! {
            _ = &mut deadline, if !watch => break,
            _ = tokio::signal::ctrl_c() => break,
            _ = &mut daemon_exited => return Err(Fail::new(Exit::Error, "ldropd stopped")),
            event = session.next() => match event {
                None => return Err(Fail::new(Exit::Error, "connection to ldropd closed")),
                Some(DiscoveryEvent::Found(d)) => {
                    if json && watch {
                        print_json_line(&json!({ "event": "found", "device": fmt::device_json(&d) }));
                    } else if !json {
                        println!("{:<20} {}", d.id, fmt::device_line(&d));
                    }
                    found.retain(|x: &ldrop_core::Device| x.id != d.id);
                    found.push(d);
                }
                Some(DiscoveryEvent::Lost(id)) => {
                    if json && watch {
                        print_json_line(&json!({ "event": "lost", "id": id }));
                    } else if !json {
                        println!("{id:<20} (gone)");
                    }
                    found.retain(|x| x.id != id);
                }
            }
        }
    }
    let _ = session.stop().await;
    if json && !watch {
        print_json(&json!(found.iter().map(fmt::device_json).collect::<Vec<_>>()));
    } else if !json && found.is_empty() && !watch {
        eprintln!(
            "No devices found. Ask the other person to open their share sheet, or for an iPhone to set AirDrop to Everyone for 10 minutes."
        );
    }
    Ok(Exit::Ok)
}

pub async fn transfers(watch: bool, json: bool) -> CmdResult {
    let client = Client::connect().await?;
    if !watch {
        let list = client.transfers().await?;
        if json {
            print_json(&json!(list.iter().map(fmt::transfer_json).collect::<Vec<_>>()));
        } else if list.is_empty() {
            println!("No transfers");
        } else {
            for t in &list {
                println!("{}", fmt::transfer_line(t));
            }
        }
        return Ok(Exit::Ok);
    }

    let mut watcher = client.watch_transfers().await?;
    for t in watcher.transfers() {
        if json {
            print_json_line(&json!({ "event": "existing", "transfer": fmt::transfer_json(t) }));
        } else {
            println!("{}", fmt::transfer_line(t));
        }
    }
    let daemon_exited = client.daemon_exited();
    tokio::pin!(daemon_exited);
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(Exit::Ok),
            _ = &mut daemon_exited => return Err(Fail::new(Exit::Error, "ldropd stopped")),
            event = watcher.next() => match event {
                None => return Err(Fail::new(Exit::Error, "connection to ldropd closed")),
                Some(TransferEvent::Added(t)) | Some(TransferEvent::Changed(t)) if json => {
                    print_json_line(&json!({ "event": "changed", "transfer": fmt::transfer_json(&t) }));
                }
                Some(TransferEvent::Added(t)) | Some(TransferEvent::Changed(t)) => println!("{}", fmt::transfer_line(&t)),
                Some(TransferEvent::Removed(id)) if json => print_json_line(&json!({ "event": "removed", "id": id })),
                Some(TransferEvent::Removed(id)) => println!("{id:>4}  removed"),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Verb {
    Accept,
    Reject,
    Cancel,
}

pub async fn decide(transfer: &str, verb: Verb) -> CmdResult {
    let path = transfer_path(transfer)
        .ok_or_else(|| Fail::new(Exit::Usage, format!("`{transfer}` is not a transfer id (see `ldrop transfers`)")))?;
    let client = Client::connect().await?;
    match verb {
        Verb::Accept => client.accept(&path.as_ref()).await?,
        Verb::Reject => client.reject(&path.as_ref()).await?,
        Verb::Cancel => client.cancel(&path.as_ref()).await?,
    }
    Ok(Exit::Ok)
}

pub async fn clear_history() -> CmdResult {
    Client::connect().await?.clear_history().await?;
    Ok(Exit::Ok)
}

pub fn config_get(key: Option<&str>) -> CmdResult {
    let paths = Paths::from_env();
    let loaded = config::load(&paths.config_file)?;
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }
    match key {
        None => {
            let text = toml::to_string_pretty(&loaded.config).map_err(|e| Fail::new(Exit::Error, e.to_string()))?;
            print!("{text}");
        }
        Some(key) => {
            let value =
                loaded.config.get(key).ok_or_else(|| Fail::new(Exit::Usage, format!("unknown config key `{key}`")))?;
            match value {
                toml::Value::String(s) => println!("{s}"),
                toml::Value::Table(t) => print!("{}", toml::to_string_pretty(&t).unwrap_or_default()),
                other => println!("{other}"),
            }
        }
    }
    Ok(Exit::Ok)
}

pub fn config_set(key: &str, value: &str) -> CmdResult {
    let paths = Paths::from_env();
    match config::set(&paths.config_file, key, value) {
        Ok(_) => {
            if config::RESTART_KEYS.contains(&key) {
                eprintln!("ldrop: {key} takes effect after restarting ldropd: systemctl --user restart ldropd");
            }
            Ok(Exit::Ok)
        }
        Err(e @ config::ConfigError::UnknownKey(_)) => Err(Fail::new(Exit::Usage, e.to_string())),
        Err(e) => Err(e.into()),
    }
}

pub fn config_path() -> CmdResult {
    println!("{}", Paths::from_env().config_file.display());
    Ok(Exit::Ok)
}

/// Replaces this process with `ldrop-tui`, preferring the one installed next to `ldrop`.
pub fn tui(args: &[String]) -> CmdResult {
    use std::os::unix::process::CommandExt;
    let sibling = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("ldrop-tui")));
    let program = sibling.filter(|p| p.exists()).unwrap_or_else(|| "ldrop-tui".into());
    let err = std::process::Command::new(&program).args(args).exec();
    Err(Fail::new(Exit::Error, format!("cannot start {}: {err}", program.display())))
}
