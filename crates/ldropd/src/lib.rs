// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldropd`, the unprivileged user daemon (SPEC §4, §10). Owns `org.ldrop.Ldrop1` on the
//! session bus. Protocol backends plug in from M2 on.

mod consent;
mod discovery;
mod doctor;
mod error;
mod manager;
mod sim;
mod state;
mod transfer;

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use futures_util::StreamExt;
use ldrop_core::config;
use ldrop_core::dbus::{BUS_NAME, ROOT_PATH};
use zbus::Connection;
use zbus::fdo::{DBusProxy, ObjectManager};

pub use state::{Options, Shared};

/// How often the config file is checked for outside edits.
const CONFIG_POLL: Duration = Duration::from_secs(2);

/// Exports all objects on `conn` and starts the background tasks. Does not claim the bus
/// name, so tests can run the daemon on any connection.
pub async fn start(conn: &Connection, opts: Options) -> anyhow::Result<Arc<Shared>> {
    let s = Shared::new(conn.clone(), opts);
    let server = conn.object_server();
    server.at(ROOT_PATH, ObjectManager).await?;
    server.at(ROOT_PATH, manager::Manager { s: s.clone() }).await?;
    if s.sim {
        server.at(ROOT_PATH, sim::Debug { s: s.clone() }).await?;
        tracing::warn!("simulation mode: fake devices and org.ldrop.Ldrop1.Debug are enabled");
    }
    transfer::export_history(&s).await?;
    doctor::refresh(&s).await;

    tokio::spawn(watch_clients(s.clone()));
    tokio::spawn(watch_config(s.clone()));
    tokio::spawn(refresh_capabilities(s.clone()));
    Ok(s)
}

/// Runs the daemon on the session bus until SIGINT/SIGTERM.
pub async fn run(opts: Options) -> anyhow::Result<()> {
    let conn = Connection::session().await.context("cannot connect to the session bus")?;
    let _shared = start(&conn, opts).await?;
    conn.request_name(BUS_NAME).await.with_context(|| format!("cannot own {BUS_NAME}; is ldropd already running?"))?;
    tracing::info!("ldropd ready on {BUS_NAME}");

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    Ok(())
}

/// Drops agents and discovery sessions of clients that leave the bus (SPEC §10).
async fn watch_clients(s: Arc<Shared>) {
    let result: zbus::Result<()> = async {
        let proxy = DBusProxy::new(&s.conn).await?;
        let mut changes = proxy.receive_name_owner_changed().await?;
        while let Some(signal) = changes.next().await {
            let Ok(args) = signal.args() else { continue };
            if args.new_owner().is_some() {
                continue;
            }
            let name = args.name().to_string();
            if !name.starts_with(':') {
                continue;
            }
            let dropped_agent = {
                let mut st = s.state();
                let before = st.agents.len();
                st.agents.retain(|a| a.owner != name);
                st.agents.len() != before
            };
            if dropped_agent {
                tracing::info!("agent {name} left the bus");
            }
            discovery::stop_owned_by(&s, &name).await;
        }
        Ok(())
    }
    .await;
    if let Err(e) = result {
        tracing::error!("cannot watch bus clients: {e}");
    }
}

/// Picks up edits to `config.toml` made outside the daemon. An invalid file keeps the last
/// good config (SPEC §12).
async fn watch_config(s: Arc<Shared>) {
    let path = s.paths.config_file.clone();
    let mtime = || std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let mut last: Option<SystemTime> = mtime();
    loop {
        tokio::time::sleep(CONFIG_POLL).await;
        let now = mtime();
        if now == last {
            continue;
        }
        last = now;
        match config::load(&path) {
            Ok(loaded) => {
                for w in &loaded.warnings {
                    tracing::warn!("{w}");
                }
                let (name, dir) = {
                    let mut st = s.state();
                    let old = std::mem::replace(&mut st.config, loaded.config);
                    (old.device_name != st.config.device_name, old.download_dir != st.config.download_dir)
                };
                tracing::info!("reloaded {}", path.display());
                manager::emit_config_props(&s, name, dir).await;
            }
            Err(e) => tracing::error!("{e}; keeping the previous config"),
        }
    }
}

async fn refresh_capabilities(s: Arc<Shared>) {
    loop {
        tokio::time::sleep(doctor::REFRESH_INTERVAL).await;
        doctor::refresh(&s).await;
    }
}
