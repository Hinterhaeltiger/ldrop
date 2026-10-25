// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop-gui`: the desktop side of ldrop (SPEC §11.4). For now the tray icon with the
//! visibility switch, Accept/Decline notifications for incoming transfers, and progress
//! notifications; the Kirigami window comes later.

mod app;
mod notify;
mod progress;
mod tray;

use std::io::IsTerminal;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;
use zbus::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};

/// One instance per session owns this name (SPEC §11.4).
const GUI_NAME: &str = "org.ldrop.Gui1";

/// Tray icon and notifications for ldrop.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Run in the background with the tray icon. This is the only mode until the main window
    /// exists, so it is also the default.
    #[arg(long)]
    tray: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("LDROP_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let conn = Connection::session().await.context("cannot connect to the session bus")?;
    match conn.request_name_with_flags(GUI_NAME, RequestNameFlags::DO_NOT_QUEUE).await {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => {
            eprintln!("ldrop-gui is already running");
            return Ok(());
        }
        Err(e) => return Err(e).context(format!("cannot claim {GUI_NAME}")),
    }
    app::run(conn).await
}

/// Opens a file, folder or URL with the desktop's default application.
pub(crate) async fn xdg_open(target: &str) {
    match tokio::process::Command::new("xdg-open").arg(target).spawn() {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(e) => tracing::warn!("cannot run xdg-open: {e}"),
    }
}
