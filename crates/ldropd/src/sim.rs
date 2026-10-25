// SPDX-License-Identifier: GPL-3.0-or-later
//! Simulated peers for frontend development and tests (`ldropd --dev-sim`). Nothing leaves
//! the machine and no files are written.
//!
//! - Two devices appear in discovery: "Sim Pixel" (Quick Share) and "Sim iPhone" (AirDrop).
//! - Sending to them walks the full outgoing state machine; an item whose name contains
//!   "decline" makes the peer decline.
//! - `org.ldrop.Ldrop1.Debug.InjectIncoming` starts an incoming request that goes through the
//!   real consent path (agent or notification).

use std::sync::Arc;
use std::time::Duration;

use ldrop_core::time::now_us;
use ldrop_core::{Device, DeviceKind, Direction, Item, Medium, Peer, Platform, Protocol, TransferState};
use rand::Rng;
use tokio_util::sync::CancellationToken;
use zbus::{OwnedObjectPath, interface};

use crate::consent::{self, Decision};
use crate::discovery;
use crate::error::{Error, Result};
use crate::state::Shared;
use crate::transfer::{self, TransferRecord};

const SIM_PREFIX: &str = "sim-";
const FOUND_AFTER: Duration = Duration::from_millis(400);
const PEER_THINKS_FOR: Duration = Duration::from_millis(1500);
const TRANSFER_TAKES: Duration = Duration::from_secs(2);
const PROGRESS_STEPS: u64 = 20;

pub fn is_sim_device(device: &Device) -> bool {
    device.id.starts_with(SIM_PREFIX)
}

fn devices() -> [Device; 2] {
    [
        Device {
            id: "sim-pixel".into(),
            name: "Sim Pixel".into(),
            kind: DeviceKind::Phone,
            platform: Platform::Android,
            protocol: Protocol::QuickShare,
            mediums: vec![Medium::Lan],
            vendor: None,
        },
        Device {
            id: "sim-iphone".into(),
            name: "Sim iPhone".into(),
            kind: DeviceKind::Phone,
            platform: Platform::Apple,
            protocol: Protocol::AirDrop,
            mediums: vec![Medium::Awdl],
            vendor: None,
        },
    ]
}

/// Simulated devices show up shortly after the first discovery session starts.
pub async fn on_discovery_started(s: &Arc<Shared>) {
    tokio::time::sleep(FOUND_AFTER).await;
    for device in devices() {
        discovery::device_found(s, device).await;
    }
}

fn pin() -> String {
    format!("{:04}", rand::rng().random_range(0..10_000))
}

pub async fn start_outgoing(s: &Arc<Shared>, device: &Device, items: Vec<Item>) -> Result<u32> {
    let peer = Peer {
        name: device.name.clone(),
        platform: device.platform,
        kind: device.kind,
        protocol: device.protocol,
        vendor: None,
    };
    let declines = items.iter().any(|i| i.name.to_lowercase().contains("decline"));
    let record = TransferRecord::new(Direction::Outgoing, TransferState::Connecting, peer, items);
    let cancel = record.cancel.clone();
    let id = transfer::create(s, record).await?;
    let s = s.clone();
    let protocol = device.protocol;
    tokio::spawn(async move {
        let run = async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            transfer::update(&s, id, |p| {
                p.state = TransferState::AwaitingAcceptance;
                if protocol == Protocol::QuickShare {
                    p.pin = pin();
                }
            })
            .await;
            tokio::time::sleep(PEER_THINKS_FOR).await;
            if declines {
                transfer::update(&s, id, |p| p.state = TransferState::Rejected).await;
                return;
            }
            simulate_progress(&s, id).await;
        };
        tokio::select! {
            _ = run => {}
            _ = cancel.cancelled() => cancelled(&s, id).await,
        }
    });
    Ok(id)
}

async fn run_incoming(s: Arc<Shared>, id: u32, cancel: CancellationToken) {
    let decision = consent::ask(&s, id).await;
    tracing::info!("sim incoming {id}: {decision:?}");
    match decision {
        Decision::Accept => {
            tokio::select! {
                _ = simulate_progress(&s, id) => {}
                _ = cancel.cancelled() => cancelled(&s, id).await,
            }
        }
        Decision::Reject => transfer::update(&s, id, |p| p.state = TransferState::Rejected).await,
        Decision::Timeout => {
            transfer::update(&s, id, |p| {
                p.state = TransferState::Rejected;
                p.error = "no answer before the deadline".into();
            })
            .await
        }
        Decision::Cancelled => cancelled(&s, id).await,
    }
}

async fn cancelled(s: &Arc<Shared>, id: u32) {
    transfer::update(s, id, |p| {
        p.state = TransferState::Cancelled;
        p.error = "cancelled on this device".into();
    })
    .await;
}

async fn simulate_progress(s: &Arc<Shared>, id: u32) {
    transfer::update(s, id, |p| {
        p.state = TransferState::Transferring;
        p.deadline = 0;
    })
    .await;
    let total = s.state().transfers.get(&id).map_or(0, |r| r.props.bytes_total);
    for step in 1..=PROGRESS_STEPS {
        tokio::time::sleep(TRANSFER_TAKES / PROGRESS_STEPS as u32).await;
        transfer::update(s, id, |p| p.bytes_done = total * step / PROGRESS_STEPS).await;
    }
    transfer::update(s, id, |p| p.state = TransferState::Completed).await;
}

/// `org.ldrop.Ldrop1.Debug`, exported only with `--dev-sim`.
pub struct Debug {
    pub s: Arc<Shared>,
}

#[interface(name = "org.ldrop.Ldrop1.Debug")]
impl Debug {
    /// Starts a simulated incoming request. `platform` is `android` (with PIN) or `apple`.
    async fn inject_incoming(
        &self,
        peer_name: String,
        platform: &str,
        names: Vec<String>,
        size_each: u64,
    ) -> Result<OwnedObjectPath> {
        let platform: Platform =
            platform.parse().map_err(|e: ldrop_core::ParseEnumError| Error::Failed(e.to_string()))?;
        let protocol = if platform == Platform::Apple { Protocol::AirDrop } else { Protocol::QuickShare };
        let names = if names.is_empty() { vec!["photo.jpg".to_owned()] } else { names };
        let items = names
            .into_iter()
            .map(|name| Item {
                mime: mime_guess::from_path(&name).first_or_octet_stream().essence_str().to_owned(),
                name,
                size: size_each,
                path: None,
                text: None,
            })
            .collect();
        let peer = Peer { name: peer_name, platform, kind: DeviceKind::Phone, protocol, vendor: None };
        let mut record = TransferRecord::new(Direction::Incoming, TransferState::AwaitingAcceptance, peer, items);
        let timeout = match protocol {
            Protocol::QuickShare => {
                record.props.pin = pin();
                u64::from(self.s.state().config.quickshare.prompt_timeout_secs)
            }
            // `/Ask` is held open for at most 15 s (SPEC §1.3).
            Protocol::AirDrop => 15,
        };
        record.props.deadline = now_us() + timeout * 1_000_000;
        let cancel = record.cancel.clone();
        let id = transfer::create(&self.s, record).await?;
        tokio::spawn(run_incoming(self.s.clone(), id, cancel));
        Ok(transfer::path(id))
    }
}
