// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop-gui`: the desktop side of ldrop (SPEC §11.4). For now the tray icon with the
//! visibility switch, Accept/Decline notifications for incoming transfers, and progress
//! notifications; the Kirigami window comes later.

mod app;
mod notify;
mod progress;
mod tray;

use std::io::IsTerminal;
use std::path::PathBuf;

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
    /// Show the tray icon on Plasma even when the ldrop applet is installed, which otherwise
    /// takes its place in the system tray.
    #[arg(long)]
    tray_icon: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
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
    let tray_icon = args.tray_icon || !plasma_applet_installed();
    if !tray_icon {
        tracing::info!("the ldrop Plasma applet shows visibility in the system tray; no tray icon");
    }
    app::run(conn, tray_icon).await
}

/// Plasma applet ID, `packaging/plasma/org.ldrop.visibility`.
const PLASMA_APPLET: &str = "org.ldrop.visibility";

/// Whether this is a Plasma session with the ldrop applet installed.
fn plasma_applet_installed() -> bool {
    let plasma = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d == "KDE"));
    plasma
        && data_dirs()
            .iter()
            .any(|dir| dir.join("plasma/plasmoids").join(PLASMA_APPLET).join("metadata.json").is_file())
}

/// `$XDG_DATA_HOME` and `$XDG_DATA_DIRS`, with their defaults.
fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    home.into_iter().chain(system.split(':').map(PathBuf::from).filter(|p| p.is_absolute())).collect()
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
