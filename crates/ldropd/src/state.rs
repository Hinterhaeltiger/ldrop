// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-wide state shared by all D-Bus objects and background tasks.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use ldrop_core::capabilities::DoctorReport;
use ldrop_core::config::{self, Config};
use ldrop_core::history::History;
use ldrop_core::hostname::default_device_name;
use ldrop_core::paths::{Paths, resolve_download_dir};
use ldrop_core::{Device, Visibility};
use tokio::sync::watch;
use zbus::{Connection, OwnedObjectPath};

use crate::transfer::TransferRecord;

/// How the daemon is started.
#[derive(Debug, Clone)]
pub struct Options {
    pub paths: Paths,
    /// Simulated peers and the `org.ldrop.Ldrop1.Debug` interface, for frontend development
    /// and tests without phones.
    pub sim: bool,
    /// Probe real hardware for `Capabilities`. Off in tests.
    pub probe_hardware: bool,
}

/// What backends react to (M2+): current visibility and when it expires (unix µs, 0 = never).
pub type VisibilityState = (Visibility, u64);

pub struct Shared {
    pub conn: Connection,
    pub paths: Paths,
    pub sim: bool,
    pub probe_hardware: bool,
    pub visibility_tx: watch::Sender<VisibilityState>,
    state: Mutex<State>,
}

pub struct State {
    pub config: Config,
    pub report: DoctorReport,
    pub report_at: Option<Instant>,
    pub visibility: Visibility,
    pub visibility_expires_at: u64,
    /// Bumped on every visibility change so stale expiry timers do nothing.
    pub visibility_generation: u64,
    /// Registered agents, last registered wins.
    pub agents: Vec<AgentReg>,
    pub sessions: HashMap<u32, SessionReg>,
    pub next_session: u32,
    /// Devices currently known from all discovery backends, in first-seen order.
    pub devices: Vec<Device>,
    pub transfers: BTreeMap<u32, TransferRecord>,
    pub next_transfer: u32,
    pub history: History,
    /// Transfer ids backing the history entries, oldest first (parallel to `history`).
    pub history_ids: VecDeque<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentReg {
    pub owner: String,
    pub path: OwnedObjectPath,
}

#[derive(Debug, Clone)]
pub struct SessionReg {
    pub owner: String,
}

impl Shared {
    pub fn new(conn: Connection, opts: Options) -> Arc<Self> {
        let config = match config::load(&opts.paths.config_file) {
            Ok(loaded) => {
                for w in &loaded.warnings {
                    tracing::warn!("{w}");
                }
                loaded.config
            }
            Err(e) => {
                tracing::error!("{e}; using defaults until the file is fixed");
                Config::default()
            }
        };
        let history = History::load(&opts.paths.history_file());
        let (visibility_tx, _) = watch::channel((Visibility::Hidden, 0));
        Arc::new(Shared {
            conn,
            sim: opts.sim,
            probe_hardware: opts.probe_hardware,
            visibility_tx,
            state: Mutex::new(State {
                config,
                report: DoctorReport::default(),
                report_at: None,
                visibility: Visibility::Hidden,
                visibility_expires_at: 0,
                visibility_generation: 0,
                agents: Vec::new(),
                sessions: HashMap::new(),
                next_session: 1,
                devices: Vec::new(),
                transfers: BTreeMap::new(),
                next_transfer: 1,
                history,
                history_ids: VecDeque::new(),
            }),
            paths: opts.paths,
        })
    }

    /// Locks the state. Never hold the guard across an `.await`.
    pub fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn device_name(&self) -> String {
        let name = self.state().config.device_name.clone();
        if name.is_empty() { default_device_name() } else { name }
    }

    pub fn download_dir(&self) -> String {
        resolve_download_dir(&self.state().config.download_dir).display().to_string()
    }
}
