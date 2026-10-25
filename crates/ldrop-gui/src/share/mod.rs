// SPDX-License-Identifier: GPL-3.0-or-later
//! The share sheet (SPEC §11.4, surface 1): `ldrop-gui --share FILE…` opens a window listing
//! nearby devices; picking one sends the files and shows the PIN, the progress and the outcome
//! right there. Qt runs on the main thread; discovery and transfers run on a tokio runtime in
//! a second thread and push their state to the window through [`CxxQtThread::queue`].

// The cxx-qt bridge's generated FFI code is unsafe; the module itself writes none.
#[allow(unsafe_code)]
mod bridge;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use cxx_qt::CxxQtThread;
use cxx_qt_lib::QString;
use ldrop_client::{Client, DiscoveryEvent, TransferInfo};
use ldrop_core::{Device, DeviceKind, Platform, Protocol, TransferState, format_size};
use serde::Serialize;
use tokio::sync::mpsc;
use zbus::OwnedObjectPath;

use bridge::ShareBackend;

/// The files to send, set once before the window opens.
static FILES: OnceLock<Vec<PathBuf>> = OnceLock::new();

/// Opens the share sheet for `files` and returns when its window closes.
pub fn run(files: Vec<PathBuf>) -> i32 {
    let _ = FILES.set(files);
    bridge::run_share_app()
}

fn files() -> &'static [PathBuf] {
    FILES.get().map(Vec::as_slice).unwrap_or_default()
}

/// The sheet's title: "photo.jpg" or "photo.jpg and 2 more", with the total size.
fn summary() -> String {
    let files = files();
    let Some(first) = files.first() else { return "Nothing to send".into() };
    let name = first.file_name().map_or_else(|| first.display().to_string(), |n| n.to_string_lossy().into_owned());
    let size: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    match files.len() {
        1 => format!("{name} · {}", format_size(size)),
        n => format!("{name} and {} more · {}", n - 1, format_size(size)),
    }
}

pub enum Command {
    Send(String),
    Cancel(String),
}

/// One row of the device list, as the QML reads it.
#[derive(Debug, Serialize, PartialEq)]
struct Row {
    id: String,
    name: String,
    icon: &'static str,
    /// "Android · Quick Share", or the state of a send to this device.
    detail: String,
    /// "idle", "busy" (sending, can cancel), "done" or "failed".
    phase: &'static str,
    /// 0–1 while the files go out, -1 otherwise.
    progress: f64,
}

/// A send started from this sheet.
struct Sending {
    transfer: Option<OwnedObjectPath>,
    info: Option<TransferInfo>,
    /// Set when the send could not start.
    error: Option<String>,
}

#[derive(Default)]
struct Sheet {
    /// In the order they were found.
    devices: Vec<Device>,
    sending: HashMap<String, Sending>,
}

impl Sheet {
    fn rows(&self) -> Vec<Row> {
        self.devices.iter().map(|d| row(d, self.sending.get(&d.id))).collect()
    }
}

fn row(device: &Device, sending: Option<&Sending>) -> Row {
    let icon = match device.kind {
        DeviceKind::Phone => "smartphone",
        DeviceKind::Tablet => "tablet",
        DeviceKind::Laptop => "computer-laptop",
        DeviceKind::Desktop => "computer",
        DeviceKind::Car | DeviceKind::Xr | DeviceKind::Unknown => "network-wireless",
    };
    let platform = match device.platform {
        Platform::Android => "Android",
        Platform::Apple => "Apple",
        Platform::Linux => "Linux",
    };
    let protocol = match device.protocol {
        Protocol::QuickShare => "Quick Share",
        Protocol::AirDrop => "AirDrop",
    };
    let mut row = Row {
        id: device.id.clone(),
        name: device.name.clone(),
        icon,
        detail: format!("{platform} · {protocol}"),
        phase: "idle",
        progress: -1.0,
    };
    let Some(sending) = sending else { return row };
    if let Some(error) = &sending.error {
        row.phase = "failed";
        row.detail = error.clone();
        return row;
    }
    let Some(t) = &sending.info else {
        row.phase = "busy";
        row.detail = "Connecting…".into();
        return row;
    };
    (row.phase, row.detail) = match t.state {
        TransferState::Connecting => ("busy", "Connecting…".into()),
        TransferState::AwaitingAcceptance if t.pin.is_empty() => ("busy", "Waiting for them to accept".into()),
        TransferState::AwaitingAcceptance => ("busy", format!("PIN {} · waiting for them to accept", t.pin)),
        TransferState::Transferring => {
            row.progress = if t.bytes_total > 0 { t.bytes_done as f64 / t.bytes_total as f64 } else { 0.0 };
            ("busy", format!("Sending · {} of {}", format_size(t.bytes_done), format_size(t.bytes_total)))
        }
        TransferState::Completed => ("done", "Sent".into()),
        TransferState::Rejected => ("failed", "Declined".into()),
        TransferState::Cancelled => ("failed", "Cancelled".into()),
        TransferState::Failed if t.error.is_empty() => ("failed", "Failed".into()),
        TransferState::Failed => ("failed", format!("Failed: {}", t.error)),
    };
    row
}

/// Updates from the tasks of [`serve`].
enum Update {
    Discovery(DiscoveryEvent),
    DiscoveryEnded(String),
    Started(String, Result<OwnedObjectPath, String>),
    Progress(String, TransferInfo),
}

/// The worker thread: talks to ldropd and keeps the window's state.
fn worker(qt: CxxQtThread<ShareBackend>, commands: mpsc::UnboundedReceiver<Command>) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => return show_status(&qt, &format!("Cannot start: {e}")),
    };
    runtime.block_on(serve(qt, commands));
}

async fn serve(qt: CxxQtThread<ShareBackend>, mut commands: mpsc::UnboundedReceiver<Command>) {
    let client = match Client::connect().await {
        Ok(client) => client,
        Err(e) => return show_status(&qt, &format!("ldropd is not available: {}", e.message())),
    };
    let (tx, mut updates) = mpsc::unbounded_channel();
    let mut session = match client.discover().await {
        Ok(session) => session,
        Err(e) => return show_status(&qt, &format!("Cannot look for devices: {}", e.message())),
    };
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(event) = session.next().await {
                if tx.send(Update::Discovery(event)).is_err() {
                    break;
                }
            }
            let _ = tx.send(Update::DiscoveryEnded("Lost the connection to ldropd".into()));
            let _ = session.stop().await;
        });
    }
    tracing::debug!("share sheet: looking for devices");
    let mut sheet = Sheet::default();
    let mut shown = Vec::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // The window is gone.
                None => break,
                Some(Command::Send(id)) => {
                    let active = sheet.sending.get(&id).is_some_and(|s| s.error.is_none() && !s.info.as_ref().is_some_and(|t| t.state.is_final()));
                    if active {
                        continue;
                    }
                    sheet.sending.insert(id.clone(), Sending { transfer: None, info: None, error: None });
                    tokio::spawn(send(client.clone(), id, tx.clone()));
                }
                Some(Command::Cancel(id)) => {
                    if let Some(path) = sheet.sending.get(&id).and_then(|s| s.transfer.clone()) {
                        let client = client.clone();
                        tokio::spawn(async move { let _ = client.cancel(&path).await; });
                    }
                }
            },
            Some(update) = updates.recv() => match update {
                Update::Discovery(DiscoveryEvent::Found(device)) => {
                    match sheet.devices.iter_mut().find(|d| d.id == device.id) {
                        Some(known) => *known = device,
                        None => sheet.devices.push(device),
                    }
                }
                // Keep devices we are sending to, so their progress stays visible.
                Update::Discovery(DiscoveryEvent::Lost(id)) => {
                    if !sheet.sending.contains_key(&id) {
                        sheet.devices.retain(|d| d.id != id);
                    }
                }
                Update::DiscoveryEnded(why) => show_status(&qt, &why),
                Update::Started(id, result) => {
                    if let Some(s) = sheet.sending.get_mut(&id) {
                        match result {
                            Ok(path) => s.transfer = Some(path),
                            Err(e) => s.error = Some(e),
                        }
                    }
                }
                Update::Progress(id, info) => {
                    if let Some(s) = sheet.sending.get_mut(&id) {
                        s.info = Some(info);
                    }
                }
            },
        }
        let rows = sheet.rows();
        if rows != shown {
            let json = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".into());
            if let Err(e) = qt.queue(move |backend| backend.set_devices(QString::from(json.as_str()))) {
                tracing::warn!("share sheet: {e:?}");
            }
            shown = rows;
        }
    }
}

/// Sends the files to `device` and reports its progress until it ends.
async fn send(client: Client, device: String, tx: mpsc::UnboundedSender<Update>) {
    let path = match client.send_files(&device, files()).await {
        Ok(path) => path,
        Err(e) => return drop(tx.send(Update::Started(device, Err(e.message())))),
    };
    let _ = tx.send(Update::Started(device.clone(), Ok(path.clone())));
    let result = client
        .follow(&path, |info| {
            let _ = tx.send(Update::Progress(device.clone(), info.clone()));
        })
        .await;
    if let Err(e) = result {
        let _ = tx.send(Update::Started(device, Err(e.message())));
    }
}

fn show_status(qt: &CxxQtThread<ShareBackend>, text: &str) {
    let text = QString::from(text);
    let _ = qt.queue(move |backend| backend.set_status(text));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ldrop_core::{Direction, Peer};

    fn device() -> Device {
        Device {
            id: "qs-1".into(),
            name: "Pixel 9".into(),
            kind: DeviceKind::Phone,
            platform: Platform::Android,
            protocol: Protocol::QuickShare,
            mediums: Vec::new(),
            vendor: None,
        }
    }

    fn info(state: TransferState) -> TransferInfo {
        TransferInfo {
            path: OwnedObjectPath::try_from("/org/ldrop/Ldrop1/transfer/1").unwrap(),
            id: 1,
            direction: Direction::Outgoing,
            state,
            peer: Peer {
                name: "Pixel 9".into(),
                platform: Platform::Android,
                kind: DeviceKind::Phone,
                protocol: Protocol::QuickShare,
                vendor: None,
            },
            pin: "1234".into(),
            items: Vec::new(),
            bytes_total: 4_000_000,
            bytes_done: 1_000_000,
            error: String::new(),
            deadline: 0,
            created_at: 0,
            preview: Vec::new(),
            preview_mime: String::new(),
        }
    }

    fn sending(state: TransferState) -> Sending {
        Sending { transfer: None, info: Some(info(state)), error: None }
    }

    #[test]
    fn rows_follow_the_send() {
        let idle = row(&device(), None);
        assert_eq!((idle.phase, idle.detail.as_str(), idle.icon), ("idle", "Android · Quick Share", "smartphone"));

        let waiting = row(&device(), Some(&sending(TransferState::AwaitingAcceptance)));
        assert_eq!((waiting.phase, waiting.detail.as_str()), ("busy", "PIN 1234 · waiting for them to accept"));

        let going = row(&device(), Some(&sending(TransferState::Transferring)));
        assert_eq!(going.phase, "busy");
        assert!((going.progress - 0.25).abs() < 1e-9);

        assert_eq!(row(&device(), Some(&sending(TransferState::Completed))).phase, "done");
        assert_eq!(row(&device(), Some(&sending(TransferState::Rejected))).detail, "Declined");

        let failed = Sending { transfer: None, info: None, error: Some("device not found".into()) };
        assert_eq!(row(&device(), Some(&failed)).phase, "failed");
    }
}
