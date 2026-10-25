// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop-gui`: the desktop side of ldrop (SPEC §11.4): the tray icon with the visibility
//! switch, Accept/Decline notifications for incoming transfers, progress notifications, and
//! the share sheet that file managers open. The main window comes later.

mod app;
mod desktop;
mod notify;
mod progress;
mod share;
mod tray;

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use ldrop_core::uri::file_uri_to_path;
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
    /// Show the tray icon even when the ldrop Plasma applet or GNOME Shell extension is
    /// active, which otherwise takes its place.
    #[arg(long)]
    tray_icon: bool,
    /// Open the share sheet to send these files (paths or file:// URIs) to a nearby device.
    #[arg(long, num_args = 1.., value_name = "FILE")]
    share: Vec<String>,
    /// Same as --share, so "Open with ldrop" works from any file manager.
    #[arg(value_name = "FILE", conflicts_with = "share")]
    files: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("LDROP_LOG").unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let share = if args.share.is_empty() { &args.files } else { &args.share };
    if !share.is_empty() {
        let files = share_paths(share)?;
        std::process::exit(share::run(files));
    }
    tokio::runtime::Runtime::new().context("cannot start the async runtime")?.block_on(tray(args))
}

/// Local files from file manager arguments; everything sent must exist and be a regular file.
fn share_paths(args: &[String]) -> anyhow::Result<Vec<PathBuf>> {
    args.iter()
        .map(|arg| {
            let path = match arg.strip_prefix("file:") {
                Some(_) => file_uri_to_path(arg).with_context(|| format!("not a local file: {arg}"))?,
                None => PathBuf::from(arg),
            };
            let path = std::path::absolute(&path).with_context(|| format!("bad path: {arg}"))?;
            anyhow::ensure!(path.is_file(), "{} is not a file", path.display());
            Ok(path)
        })
        .collect()
}

async fn tray(args: Args) -> anyhow::Result<()> {
    let conn = Connection::session().await.context("cannot connect to the session bus")?;
    match conn.request_name_with_flags(GUI_NAME, RequestNameFlags::DO_NOT_QUEUE).await {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => {}
        Ok(_) | Err(zbus::Error::NameTaken) => {
            eprintln!("ldrop-gui is already running");
            return Ok(());
        }
        Err(e) => return Err(e).context(format!("cannot claim {GUI_NAME}")),
    }
    let set_up = desktop::first_run(&conn).await;
    for step in &set_up {
        tracing::info!("set up for this user: {step}");
    }
    let widget = if args.tray_icon {
        None
    } else if set_up.contains(&desktop::GNOME_EXTENSION_STEP) {
        // GNOME reports it active only a moment later.
        Some(desktop::GNOME_WIDGET)
    } else {
        desktop::native_widget(&conn).await
    };
    if let Some(widget) = widget {
        tracing::info!("{widget} shows visibility in the panel; no tray icon");
    }
    app::run(conn, widget.is_none()).await
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
