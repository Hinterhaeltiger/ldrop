// SPDX-License-Identifier: GPL-3.0-or-later
//! Transfer objects (`/org/ldrop/Ldrop1/transfer/N`, SPEC §9, §10) and the finished-transfer
//! history behind `ListTransfers`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ldrop_core::dbus::{ItemDict, PeerDict, TRANSFER_PATH_PREFIX};
use ldrop_core::history::HistoryEntry;
use ldrop_core::time::now_us;
use ldrop_core::{Direction, Item, Peer, TransferState};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use zbus::{OwnedObjectPath, interface};

use crate::state::Shared;

/// `PropertiesChanged` for progress is throttled to this interval (≤ 10 Hz, SPEC §10).
const PROGRESS_EMIT_INTERVAL: Duration = Duration::from_millis(100);

/// What the `Transfer` interface exposes.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferProps {
    pub direction: Direction,
    pub state: TransferState,
    pub peer: Peer,
    pub pin: String,
    pub items: Vec<Item>,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub error: String,
    /// Consent deadline, unix µs; 0 when no prompt is pending.
    pub deadline: u64,
    pub created_at: u64,
    pub preview: Vec<u8>,
    pub preview_mime: String,
}

pub struct TransferRecord {
    pub props: TransferProps,
    /// Resolved by `Manager.Accept`/`Reject` while a prompt is pending.
    pub decision: Option<oneshot::Sender<bool>>,
    pub cancel: CancellationToken,
    last_progress_emit: Option<Instant>,
}

impl TransferRecord {
    pub fn new(direction: Direction, state: TransferState, peer: Peer, items: Vec<Item>) -> Self {
        let bytes_total = items.iter().map(|i| i.size).sum();
        TransferRecord {
            props: TransferProps {
                direction,
                state,
                peer,
                pin: String::new(),
                items,
                bytes_total,
                bytes_done: 0,
                error: String::new(),
                deadline: 0,
                created_at: now_us(),
                preview: Vec::new(),
                preview_mime: String::new(),
            },
            decision: None,
            cancel: CancellationToken::new(),
            last_progress_emit: None,
        }
    }

    fn from_history(entry: &HistoryEntry) -> Self {
        let mut rec = TransferRecord::new(entry.direction, entry.state, entry.peer.clone(), entry.items.clone());
        rec.props.bytes_total = entry.bytes_total;
        rec.props.bytes_done = entry.bytes_done;
        rec.props.error = entry.error.clone();
        rec.props.created_at = entry.created_at;
        rec
    }
}

pub fn path(id: u32) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{TRANSFER_PATH_PREFIX}/{id}")).expect("valid object path")
}

/// Parses `/org/ldrop/Ldrop1/transfer/N`.
pub fn id_from_path(path: &str) -> Option<u32> {
    path.strip_prefix(TRANSFER_PATH_PREFIX)?.strip_prefix('/')?.parse().ok()
}

pub struct TransferIface {
    s: Arc<Shared>,
    id: u32,
}

impl TransferIface {
    fn get<T>(&self, f: impl FnOnce(&TransferProps) -> T) -> T
    where
        T: Default,
    {
        self.s.state().transfers.get(&self.id).map(|r| f(&r.props)).unwrap_or_default()
    }
}

#[interface(name = "org.ldrop.Ldrop1.Transfer")]
impl TransferIface {
    #[zbus(property)]
    fn direction(&self) -> String {
        self.get(|p| p.direction.to_string())
    }

    #[zbus(property)]
    fn state(&self) -> String {
        self.get(|p| p.state.to_string())
    }

    #[zbus(property)]
    fn peer(&self) -> PeerDict {
        self.get(|p| PeerDict::from(&p.peer))
    }

    #[zbus(property)]
    fn pin(&self) -> String {
        self.get(|p| p.pin.clone())
    }

    #[zbus(property)]
    fn items(&self) -> Vec<ItemDict> {
        self.get(|p| p.items.iter().map(ItemDict::from).collect())
    }

    #[zbus(property)]
    fn bytes_total(&self) -> u64 {
        self.get(|p| p.bytes_total)
    }

    #[zbus(property)]
    fn bytes_done(&self) -> u64 {
        self.get(|p| p.bytes_done)
    }

    #[zbus(property)]
    fn error(&self) -> String {
        self.get(|p| p.error.clone())
    }

    #[zbus(property)]
    fn deadline(&self) -> u64 {
        self.get(|p| p.deadline)
    }

    #[zbus(property)]
    fn created_at(&self) -> u64 {
        self.get(|p| p.created_at)
    }

    #[zbus(property)]
    fn preview(&self) -> Vec<u8> {
        self.get(|p| p.preview.clone())
    }

    #[zbus(property)]
    fn preview_mime(&self) -> String {
        self.get(|p| p.preview_mime.clone())
    }
}

/// Registers a new transfer and exports its object.
pub async fn create(s: &Arc<Shared>, record: TransferRecord) -> zbus::Result<u32> {
    let id = {
        let mut st = s.state();
        let id = st.next_transfer;
        st.next_transfer += 1;
        st.transfers.insert(id, record);
        id
    };
    s.conn.object_server().at(path(id), TransferIface { s: s.clone(), id }).await?;
    Ok(id)
}

/// Applies `change` to a transfer, emits `PropertiesChanged` for what changed (progress
/// throttled), and files the transfer into history once it reaches a final state.
pub async fn update(s: &Arc<Shared>, id: u32, change: impl FnOnce(&mut TransferProps)) {
    let (old, new, emit_progress) = {
        let mut st = s.state();
        let Some(rec) = st.transfers.get_mut(&id) else { return };
        if rec.props.state.is_final() {
            return;
        }
        let old = rec.props.clone();
        change(&mut rec.props);
        let new = rec.props.clone();
        let now = Instant::now();
        let emit_progress = new.state.is_final()
            || rec.last_progress_emit.is_none_or(|t| now.duration_since(t) >= PROGRESS_EMIT_INTERVAL);
        if emit_progress && old.bytes_done != new.bytes_done {
            rec.last_progress_emit = Some(now);
        }
        if new.state != TransferState::AwaitingAcceptance {
            rec.decision = None;
        }
        (old, new, emit_progress)
    };
    if let Err(e) = emit_changes(s, id, &old, &new, emit_progress).await {
        tracing::warn!("transfer {id}: cannot emit property changes: {e}");
    }
    if new.state.is_final() {
        finish(s, id, &new).await;
    }
}

async fn emit_changes(
    s: &Shared,
    id: u32,
    old: &TransferProps,
    new: &TransferProps,
    emit_progress: bool,
) -> zbus::Result<()> {
    let iface_ref = s.conn.object_server().interface::<_, TransferIface>(path(id)).await?;
    let iface = iface_ref.get().await;
    let em = iface_ref.signal_emitter();
    if old.state != new.state {
        iface.state_changed(em).await?;
    }
    if old.pin != new.pin {
        iface.pin_changed(em).await?;
    }
    if old.items != new.items {
        iface.items_changed(em).await?;
    }
    if old.bytes_total != new.bytes_total {
        iface.bytes_total_changed(em).await?;
    }
    if old.bytes_done != new.bytes_done && emit_progress {
        iface.bytes_done_changed(em).await?;
    }
    if old.error != new.error {
        iface.error_changed(em).await?;
    }
    if old.deadline != new.deadline {
        iface.deadline_changed(em).await?;
    }
    if old.peer != new.peer {
        iface.peer_changed(em).await?;
    }
    if old.preview != new.preview || old.preview_mime != new.preview_mime {
        iface.preview_changed(em).await?;
        iface.preview_mime_changed(em).await?;
    }
    Ok(())
}

/// Appends a finished transfer to history and drops the object history no longer holds.
async fn finish(s: &Arc<Shared>, id: u32, props: &TransferProps) {
    let entry = HistoryEntry {
        direction: props.direction,
        state: props.state,
        peer: props.peer.clone(),
        items: props.items.clone(),
        bytes_total: props.bytes_total,
        bytes_done: props.bytes_done,
        error: props.error.clone(),
        created_at: props.created_at,
        finished_at: now_us(),
    };
    let evicted = {
        let mut st = s.state();
        let evicted = match st.history.push(entry) {
            Ok(evicted) => evicted.is_some(),
            Err(e) => {
                tracing::warn!("cannot write history: {e}");
                false
            }
        };
        st.history_ids.push_back(id);
        let evicted_id = if evicted { st.history_ids.pop_front() } else { None };
        if let Some(old) = evicted_id {
            st.transfers.remove(&old);
        }
        evicted_id
    };
    if let Some(old) = evicted {
        let _ = s.conn.object_server().remove::<TransferIface, _>(path(old)).await;
    }
}

/// Exports the history loaded at startup as finished transfer objects.
pub async fn export_history(s: &Arc<Shared>) -> zbus::Result<()> {
    let records: Vec<TransferRecord> = s.state().history.entries().map(TransferRecord::from_history).collect();
    for record in records {
        let id = create(s, record).await?;
        s.state().history_ids.push_back(id);
    }
    Ok(())
}

pub async fn clear_history(s: &Arc<Shared>) -> std::io::Result<()> {
    let ids: Vec<u32> = {
        let mut st = s.state();
        st.history.clear()?;
        let ids: Vec<u32> = st.history_ids.drain(..).collect();
        for id in &ids {
            st.transfers.remove(id);
        }
        ids
    };
    for id in ids {
        let _ = s.conn.object_server().remove::<TransferIface, _>(path(id)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip() {
        assert_eq!(path(7).as_str(), "/org/ldrop/Ldrop1/transfer/7");
        assert_eq!(id_from_path("/org/ldrop/Ldrop1/transfer/7"), Some(7));
        assert_eq!(id_from_path("/org/ldrop/Ldrop1/transfer7"), None);
        assert_eq!(id_from_path("/org/ldrop/Ldrop1/discovery/7"), None);
    }
}
