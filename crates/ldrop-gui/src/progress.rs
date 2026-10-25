// SPDX-License-Identifier: GPL-3.0-or-later
//! Running transfers as notifications (SPEC §11.4): a progress notification once a transfer
//! starts moving data, then one that says how it ended.
//!
//! On Plasma the progress goes to the job view server, which draws a progress bar with a
//! Cancel button and, for files, a finished notification that opens the download folder.
//! Elsewhere a regular notification is updated in place, with the `value` hint for servers
//! that draw a bar.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use ldrop_client::{Client, TransferInfo};
use ldrop_core::uri::path_to_file_uri;
use ldrop_core::{Direction, Item, MIME_TEXT, MIME_URL, MIME_WIFI, TransferState, describe_items, format_size};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::{Connection, OwnedObjectPath, Variant, proxy};

use crate::notify::{JobViewProxy, Notification, Notifier, Reply, Urgency};

/// Updates more often than this only change the speed estimate.
const UPDATE_EVERY: Duration = Duration::from_millis(500);
/// `KJob::KilledJobError`: Plasma treats the job as cancelled and shows nothing.
const KILLED: u32 = 1;
/// `KJob::UserDefinedError`: Plasma shows the message.
const FAILED: u32 = 100;
const MAX_TEXT_IN_BODY: usize = 200;

#[proxy(
    interface = "org.freedesktop.FileManager1",
    default_service = "org.freedesktop.FileManager1",
    default_path = "/org/freedesktop/FileManager1"
)]
trait FileManager {
    fn show_items(&self, uris: &[&str], startup_id: &str) -> zbus::Result<()>;
}

/// Aborts the task when dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum Display {
    Job(JobViewProxy<'static>),
    Note { id: u32, replies: mpsc::UnboundedSender<Reply> },
}

struct Shown {
    display: Display,
    updated: Instant,
    /// Bytes and time of the last update, for the speed.
    sample: (u64, Instant),
    _cancel: Task,
}

/// Progress and completion notifications for every transfer.
pub struct Progress {
    notifier: Notifier,
    client: Client,
    download_dir: String,
    shown: HashMap<u32, Shown>,
    /// Transfers whose end was already reported.
    finished: HashSet<u32>,
}

impl Progress {
    /// `known` are the transfers that existed at startup; their ends are not reported again.
    pub fn new(notifier: Notifier, client: Client, known: impl IntoIterator<Item = u32>) -> Self {
        Progress {
            notifier,
            client,
            download_dir: String::new(),
            shown: HashMap::new(),
            finished: known.into_iter().collect(),
        }
    }

    pub fn set_download_dir(&mut self, dir: &str) {
        dir.clone_into(&mut self.download_dir);
    }

    /// Follows a change of transfer `t`.
    pub async fn on_change(&mut self, t: &TransferInfo) {
        if t.state == TransferState::Transferring {
            match self.shown.get_mut(&t.id) {
                Some(shown) => update(&self.notifier, shown, t).await,
                None => {
                    if let Some(shown) = self.show(t).await {
                        self.shown.insert(t.id, shown);
                    }
                }
            }
        } else if t.state.is_final() && self.finished.insert(t.id) {
            let shown = self.shown.remove(&t.id);
            self.finish(t, shown).await;
        }
    }

    async fn show(&self, t: &TransferInfo) -> Option<Shown> {
        let cancel = |display_task: JoinHandle<()>| Task(display_task);
        let now = Instant::now();
        let mut hints = HashMap::from([
            ("title", Variant::from(title(t))),
            ("descriptionLabel1", Variant::from(if t.items.len() == 1 { "Item" } else { "Items" })),
            ("descriptionValue1", Variant::from(what(&t.items))),
            ("totalBytes", Variant::from(t.bytes_total)),
            ("processedBytes", Variant::from(t.bytes_done)),
            ("percent", Variant::from(percent(t))),
            ("immediate", Variant::from(true)),
        ]);
        if t.direction == Direction::Incoming && !self.download_dir.is_empty() {
            hints.insert("destUrl", Variant::from(path_to_file_uri(self.download_dir.as_ref())));
        }
        if let Some(job) = self.notifier.job_view(hints).await {
            let (client, path, job2) = (self.client.clone(), t.path.clone(), job.clone());
            let task = tokio::spawn(async move {
                if let Ok(mut requests) = job2.receive_cancel_requested().await
                    && requests.next().await.is_some()
                {
                    let _ = client.cancel(&path.as_ref()).await;
                }
            });
            return Some(Shown {
                display: Display::Job(job),
                updated: now,
                sample: (t.bytes_done, now),
                _cancel: cancel(task),
            });
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let id = match self.notifier.show(&progress_note(t, 0), 0, tx.clone()).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("cannot show the progress of transfer {}: {e}", t.id);
                return None;
            }
        };
        let task = tokio::spawn(cancel_on_action(rx, self.client.clone(), t.path.clone()));
        Some(Shown {
            display: Display::Note { id, replies: tx },
            updated: now,
            sample: (t.bytes_done, now),
            _cancel: cancel(task),
        })
    }

    async fn finish(&self, t: &TransferInfo, shown: Option<Shown>) {
        let mut replaces = 0;
        let mut on_plasma = false;
        if let Some(shown) = shown {
            match shown.display {
                Display::Job(job) => {
                    let (code, message) = match t.state {
                        TransferState::Completed => (0, String::new()),
                        TransferState::Cancelled => (KILLED, String::new()),
                        _ => (FAILED, failure(t)),
                    };
                    let _ = job.terminate(code, &message, HashMap::new()).await;
                    on_plasma = true;
                }
                Display::Note { id, .. } => replaces = id,
            }
        }
        let Some(mut note) = ending(t) else {
            if replaces != 0 {
                self.notifier.close(replaces).await;
            }
            return;
        };
        // Plasma's finished job already says it, unless there is text to show.
        let has_text = t.items.iter().any(|i| i.text.is_some() && i.mime != MIME_WIFI);
        if on_plasma && (t.state != TransferState::Completed || !has_text) {
            return;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        if note.actions.is_empty() {
            note.timeout_ms = -1;
        }
        match self.notifier.show(&note, replaces, tx).await {
            Ok(_) => {
                tokio::spawn(open_on_action(rx, self.client.connection().clone(), t.items.clone()));
            }
            Err(e) => tracing::warn!("cannot show how transfer {} ended: {e}", t.id),
        }
    }
}

async fn update(notifier: &Notifier, shown: &mut Shown, t: &TransferInfo) {
    let now = Instant::now();
    if now.duration_since(shown.updated) < UPDATE_EVERY {
        return;
    }
    let (bytes, at) = shown.sample;
    let elapsed = now.duration_since(at).as_secs_f64();
    let speed = if elapsed > 0.0 { (t.bytes_done.saturating_sub(bytes) as f64 / elapsed) as u64 } else { 0 };
    shown.updated = now;
    shown.sample = (t.bytes_done, now);
    match &shown.display {
        Display::Job(job) => {
            let update = HashMap::from([
                ("processedBytes", Variant::from(t.bytes_done)),
                ("totalBytes", Variant::from(t.bytes_total)),
                ("percent", Variant::from(percent(t))),
                ("speed", Variant::from(speed)),
            ]);
            let _ = job.update(update).await;
        }
        Display::Note { id, replies } => {
            let _ = notifier.show(&progress_note(t, speed), *id, replies.clone()).await;
        }
    }
}

pub fn percent(t: &TransferInfo) -> u32 {
    (t.bytes_done.min(t.bytes_total) * 100).checked_div(t.bytes_total).unwrap_or(0) as u32
}

fn title(t: &TransferInfo) -> String {
    match t.direction {
        Direction::Incoming => format!("Receiving from {}", t.peer.name),
        Direction::Outgoing => format!("Sending to {}", t.peer.name),
    }
}

/// "photo.jpg" or "3 files".
fn what(items: &[Item]) -> String {
    match items {
        [one] if one.text.is_none() => one.name.clone(),
        _ => describe_items(items),
    }
}

fn progress_note(t: &TransferInfo, speed: u64) -> Notification {
    let mut body = format!("{}% · {} of {}", percent(t), format_size(t.bytes_done), format_size(t.bytes_total));
    if speed > 0 {
        body.push_str(&format!(" · {}/s", format_size(speed)));
    }
    Notification {
        icon: match t.direction {
            Direction::Incoming => "folder-download",
            Direction::Outgoing => "document-send",
        },
        actions: vec![("cancel", "Cancel")],
        progress: Some(percent(t) as u8),
        timeout_ms: 0,
        ..Notification::new(format!("{} · {}", title(t), what(&t.items)), body)
    }
}

fn failure(t: &TransferInfo) -> String {
    match t.state {
        TransferState::Rejected if t.error.is_empty() => format!("{} declined", t.peer.name),
        _ if t.error.is_empty() => "The transfer failed".to_owned(),
        _ => t.error.clone(),
    }
}

/// The notification that says how `t` ended, `None` when there is nothing to say (we declined
/// or cancelled it ourselves).
fn ending(t: &TransferInfo) -> Option<Notification> {
    let peer = &t.peer.name;
    let incoming = t.direction == Direction::Incoming;
    match t.state {
        TransferState::Completed if incoming => Some(received(t)),
        TransferState::Completed => Some(Notification {
            category: "transfer.complete",
            ..Notification::new(format!("Sent {} to {peer}", what(&t.items)), "")
        }),
        TransferState::Rejected if incoming => None,
        TransferState::Cancelled if t.error.is_empty() || t.error.contains("this device") => None,
        TransferState::Rejected | TransferState::Cancelled | TransferState::Failed => {
            let summary =
                if incoming { format!("Transfer from {peer} ended") } else { format!("Transfer to {peer} ended") };
            Some(Notification {
                icon: "dialog-error",
                category: "transfer.error",
                urgency: Urgency::Normal,
                ..Notification::new(summary, failure(t))
            })
        }
        _ => None,
    }
}

fn received(t: &TransferInfo) -> Notification {
    let peer = &t.peer.name;
    let mut note = Notification { category: "transfer.complete", ..Notification::new("", "") };
    match t.items.as_slice() {
        [one] if one.mime == MIME_WIFI => {
            // Never put the password in a notification.
            note.summary = format!("{peer} shared the Wi-Fi network {}", one.name);
        }
        [one] if one.text.is_some() => {
            let text = one.text.clone().unwrap_or_default();
            note.summary = format!("Received {} from {peer}", describe_items(&t.items));
            note.body = truncate(&text, MAX_TEXT_IN_BODY);
            if one.mime == MIME_URL {
                note.actions = vec![("default", "Open link"), ("open", "Open link")];
            }
        }
        items => {
            note.summary = format!("Received {} from {peer}", what(items));
            let folder = items
                .iter()
                .find_map(|i| i.path.as_deref())
                .and_then(|p| std::path::Path::new(p).parent())
                .map(|p| p.display().to_string());
            if let Some(folder) = folder {
                note.body = format!("Saved in {folder}");
                note.actions = vec![("default", "Open"), ("open", "Open"), ("folder", "Show in folder")];
            }
        }
    }
    note
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_owned(),
    }
}

async fn cancel_on_action(mut replies: mpsc::UnboundedReceiver<Reply>, client: Client, path: OwnedObjectPath) {
    while let Some(reply) = replies.recv().await {
        if reply == Reply::Action("cancel".into()) {
            let _ = client.cancel(&path.as_ref()).await;
        }
    }
}

async fn open_on_action(mut replies: mpsc::UnboundedReceiver<Reply>, conn: Connection, items: Vec<Item>) {
    while let Some(reply) = replies.recv().await {
        let Reply::Action(action) = reply else { return };
        match action.as_str() {
            "default" | "open" => open_items(&items).await,
            "folder" => show_in_folder(&conn, &items).await,
            _ => {}
        }
    }
}

/// Opens one file, link or text with `xdg-open`; several files open their folder.
async fn open_items(items: &[Item]) {
    let target = match items {
        [one] if one.mime == MIME_URL => one.text.clone(),
        [one] if one.mime == MIME_TEXT => None,
        [one] => one.path.clone(),
        many => many
            .iter()
            .find_map(|i| i.path.as_deref())
            .and_then(|p| std::path::Path::new(p).parent())
            .map(|p| p.display().to_string()),
    };
    if let Some(target) = target {
        crate::xdg_open(&target).await;
    }
}

async fn show_in_folder(conn: &Connection, items: &[Item]) {
    let uris: Vec<String> =
        items.iter().filter_map(|i| i.path.as_deref()).map(|p| path_to_file_uri(p.as_ref())).collect();
    if uris.is_empty() {
        return;
    }
    let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
    let shown = async { FileManagerProxy::new(conn).await?.show_items(&uris, "").await };
    if let Err(e) = shown.await {
        tracing::debug!("no file manager service ({e}); opening the folder instead");
        let folder = items.iter().find_map(|i| i.path.as_deref()).and_then(|p| std::path::Path::new(p).parent());
        if let Some(folder) = folder {
            crate::xdg_open(&folder.display().to_string()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use ldrop_core::{DeviceKind, Peer, Platform, Protocol};

    use super::*;

    fn transfer(direction: Direction, state: TransferState, items: Vec<Item>) -> TransferInfo {
        TransferInfo {
            path: OwnedObjectPath::try_from("/org/ldrop/Ldrop1/transfer/1").unwrap(),
            id: 1,
            direction,
            state,
            peer: Peer {
                name: "Pixel 9".into(),
                platform: Platform::Android,
                kind: DeviceKind::Phone,
                protocol: Protocol::QuickShare,
                vendor: None,
            },
            pin: String::new(),
            items,
            bytes_total: 2_000_000,
            bytes_done: 500_000,
            error: String::new(),
            deadline: 0,
            created_at: 0,
            preview: Vec::new(),
            preview_mime: String::new(),
        }
    }

    fn file(name: &str) -> Item {
        Item {
            name: name.into(),
            mime: "image/jpeg".into(),
            path: Some(format!("/home/u/Downloads/{name}")),
            ..Default::default()
        }
    }

    #[test]
    fn progress_text() {
        let t = transfer(Direction::Incoming, TransferState::Transferring, vec![file("a.jpg")]);
        let note = progress_note(&t, 1_500_000);
        assert_eq!(note.summary, "Receiving from Pixel 9 · a.jpg");
        assert_eq!(note.body, "25% · 500 kB of 2.0 MB · 1.5 MB/s");
        assert_eq!(note.progress, Some(25));
        assert_eq!(note.actions, [("cancel", "Cancel")]);
    }

    #[test]
    fn received_files_offer_open_and_folder() {
        let t = transfer(Direction::Incoming, TransferState::Completed, vec![file("a.jpg"), file("b.jpg")]);
        let note = ending(&t).unwrap();
        assert_eq!(note.summary, "Received 2 files from Pixel 9");
        assert_eq!(note.body, "Saved in /home/u/Downloads");
        assert!(note.actions.iter().any(|(k, _)| *k == "folder"));
    }

    #[test]
    fn wifi_passwords_stay_out_of_notifications() {
        let wifi =
            Item { name: "Home".into(), mime: MIME_WIFI.into(), text: Some("hunter22".into()), ..Default::default() };
        let note = ending(&transfer(Direction::Incoming, TransferState::Completed, vec![wifi])).unwrap();
        assert!(!note.summary.contains("hunter22") && !note.body.contains("hunter22"));
    }

    #[test]
    fn our_own_declines_and_cancels_stay_quiet() {
        let mut t = transfer(Direction::Incoming, TransferState::Rejected, vec![file("a.jpg")]);
        assert!(ending(&t).is_none());
        t.state = TransferState::Cancelled;
        t.error = "cancelled on this device".into();
        assert!(ending(&t).is_none());
        t.error = "cancelled by the sender".into();
        assert_eq!(ending(&t).unwrap().body, "cancelled by the sender");
        let mut out = transfer(Direction::Outgoing, TransferState::Rejected, vec![file("a.jpg")]);
        assert_eq!(ending(&out).unwrap().body, "Pixel 9 declined");
        out.state = TransferState::Completed;
        assert_eq!(ending(&out).unwrap().summary, "Sent a.jpg to Pixel 9");
    }

    #[test]
    fn long_texts_are_cut() {
        assert_eq!(truncate("äbc", 2), "äb…");
        assert_eq!(truncate("abc", 3), "abc");
    }
}
