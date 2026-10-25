// SPDX-License-Identifier: GPL-3.0-or-later
//! The tray process: follows ldropd (reconnecting when it restarts), answers incoming requests
//! through Accept/Decline notifications as the Agent (SPEC §10), and shows transfer progress.

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::stream::{BoxStream, select_all};
use futures_util::{FutureExt, StreamExt};
use ldrop_client::{Client, Decision, PendingRequest, Status, TransferEvent, TransferInfo};
use ldrop_core::time::remaining;
use ldrop_core::{Direction, TransferState, Visibility, prompt_text};
use tokio::sync::mpsc;
use zbus::Connection;

use crate::notify::{DISMISSED, Notification, Notifier, Reply, Urgency};
use crate::progress::{Progress, percent};
use crate::tray::{Action, Tray, View};

const RECONNECT_DELAY: Duration = Duration::from_secs(3);
/// Keeps the "N min left" countdown current.
const REFRESH: Duration = Duration::from_secs(20);
const NOTIFIER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
struct State {
    /// `None` while ldropd is unreachable.
    status: Option<Status>,
    transfers: BTreeMap<u32, TransferInfo>,
}

/// What the tray shows for `state`.
fn view(state: &State) -> View {
    let Some(status) = &state.status else {
        return View {
            icon: "view-hidden",
            status: "ldropd is not running".into(),
            visibility: None,
            attention: false,
        };
    };
    let attention = state
        .transfers
        .values()
        .any(|t| t.direction == Direction::Incoming && t.state == TransferState::AwaitingAcceptance);
    let visibility = Some(status.visibility);
    if let Some(t) = state.transfers.values().find(|t| t.state == TransferState::Transferring) {
        let percent = percent(t);
        let (icon, text) = match t.direction {
            Direction::Incoming => ("folder-download", format!("Receiving from {} · {percent}%", t.peer.name)),
            Direction::Outgoing => ("document-send", format!("Sending to {} · {percent}%", t.peer.name)),
        };
        return View { icon, status: text, visibility, attention };
    }
    match status.visibility {
        Visibility::Everyone => {
            let mut text = format!("Visible as {}", status.device_name);
            if let Some(left) = remaining(status.visibility_expires_at) {
                text.push_str(&format!(" · {} min left", left.as_secs().div_ceil(60)));
            }
            View { icon: "view-visible", status: text, visibility, attention }
        }
        Visibility::Hidden => {
            View { icon: "view-hidden", status: "Hidden from nearby devices".into(), visibility, attention }
        }
    }
}

enum End {
    Quit,
    Lost(String),
}

/// `tray_icon`: whether to show the tray icon (the Plasma applet may stand in for it).
pub async fn run(conn: Connection, tray_icon: bool) -> anyhow::Result<()> {
    let (actions_tx, mut actions) = mpsc::unbounded_channel();
    let tray = Tray::start(&conn, view(&State::default()), actions_tx, tray_icon).await?;
    // D-Bus activation of a missing server would otherwise hold everything up for 25 s.
    let notifier = match tokio::time::timeout(NOTIFIER_TIMEOUT, Notifier::new(&conn)).await {
        Ok(Ok(notifier)) => Some(notifier),
        Ok(Err(e)) => {
            tracing::warn!("no notification server ({e}); ldropd asks about incoming transfers by itself");
            None
        }
        Err(_) => {
            tracing::warn!("the notification server does not answer; ldropd asks about incoming transfers by itself");
            None
        }
    };
    loop {
        let why = match session(&conn, &tray, notifier.as_ref(), &mut actions).await {
            End::Quit => return Ok(()),
            End::Lost(why) => why,
        };
        tracing::info!("{why}");
        tray.update(view(&State::default())).await;
        // Quit still works while waiting.
        let sleep = tokio::time::sleep(RECONNECT_DELAY);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                () = &mut sleep => break,
                Some(action) = actions.recv() => if action == Action::Quit { return Ok(()) },
            }
        }
    }
}

/// Runs one ldropd session; returns why it ended.
async fn session(
    conn: &Connection,
    tray: &Tray,
    notifier: Option<&Notifier>,
    actions: &mut mpsc::UnboundedReceiver<Action>,
) -> End {
    let lost = |e: ldrop_client::Error| End::Lost(format!("cannot reach ldropd: {e}"));
    let client = match Client::with_connection(conn.clone()).await {
        Ok(client) => client,
        Err(e) => return lost(e),
    };
    // Also starts ldropd through D-Bus activation.
    let status = match client.status().await {
        Ok(status) => status,
        Err(e) => return lost(e),
    };
    let mut watch = match client.watch_transfers().await {
        Ok(watch) => watch,
        Err(e) => return lost(e),
    };
    // Prompts need action buttons; without them ldropd keeps asking by itself.
    let agent = match notifier.filter(|n| n.has_actions()) {
        Some(_) => match client.register_agent_channel().await {
            Ok(agent) => Some(agent),
            Err(e) => return lost(e),
        },
        None => None,
    };
    let (registration, mut requests) = match agent {
        Some((registration, requests)) => (Some(registration), Some(requests)),
        None => (None, None),
    };

    let mut state = State { transfers: watch.transfers().map(|t| (t.id, t.clone())).collect(), status: Some(status) };
    let mut progress = notifier.map(|n| Progress::new(n.clone(), client.clone(), state.transfers.keys().copied()));
    if let (Some(progress), Some(status)) = (progress.as_mut(), &state.status) {
        progress.set_download_dir(&status.download_dir);
    }
    tray.update(view(&state)).await;
    tracing::info!("connected to ldropd");

    let manager = client.manager();
    let mut changes: Vec<BoxStream<'_, ()>> = vec![
        manager.receive_device_name_changed().await.map(|_| ()).boxed(),
        manager.receive_visibility_changed().await.map(|_| ()).boxed(),
        manager.receive_visibility_expires_at_changed().await.map(|_| ()).boxed(),
        manager.receive_download_dir_changed().await.map(|_| ()).boxed(),
    ];
    let mut changes = select_all(changes.drain(..));
    let exited = client.daemon_exited().fuse();
    tokio::pin!(exited);
    let mut refresh = tokio::time::interval(REFRESH);

    let end = loop {
        tokio::select! {
            _ = &mut exited => break End::Lost("ldropd stopped".into()),
            change = watch.next() => match change {
                Some(TransferEvent::Added(t) | TransferEvent::Changed(t)) => {
                    if let Some(progress) = progress.as_mut() {
                        progress.on_change(&t).await;
                    }
                    state.transfers.insert(t.id, t);
                }
                Some(TransferEvent::Removed(id)) => {
                    state.transfers.remove(&id);
                }
                None => break End::Lost("lost the connection to ldropd".into()),
            },
            Some(()) = changes.next() => {
                if let Ok(status) = client.status().await {
                    if let Some(progress) = progress.as_mut() {
                        progress.set_download_dir(&status.download_dir);
                    }
                    state.status = Some(status);
                }
            }
            Some(pending) = recv(&mut requests) => {
                if let Some(notifier) = notifier {
                    tokio::spawn(prompt(notifier.clone(), pending));
                }
            }
            Some(action) = actions.recv() => {
                if let Some(end) = act(&client, &state, action).await {
                    break end;
                }
            }
            _ = refresh.tick() => {}
        }
        tray.update(view(&state)).await;
    };
    if let Some(registration) = registration {
        let _ = registration.unregister().await;
    }
    end
}

async fn recv(requests: &mut Option<mpsc::Receiver<PendingRequest>>) -> Option<PendingRequest> {
    match requests {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn act(client: &Client, state: &State, action: Action) -> Option<End> {
    let set = |mode| async move {
        // A zero timeout means ldropd's configured one (10 minutes by default).
        if let Err(e) = client.set_visibility(mode, Duration::ZERO).await {
            tracing::warn!("cannot change visibility: {e}");
        }
    };
    match action {
        Action::SetVisibility(mode) => set(mode).await,
        Action::ToggleVisibility => {
            let hidden = state.status.as_ref().is_none_or(|s| s.visibility == Visibility::Hidden);
            set(if hidden { Visibility::Everyone } else { Visibility::Hidden }).await
        }
        Action::OpenDownloads => {
            if let Some(status) = &state.status {
                crate::xdg_open(&status.download_dir).await;
            }
        }
        Action::Quit => return Some(End::Quit),
    }
    None
}

/// Asks about one incoming request with an Accept/Decline notification. Dismissing it declines;
/// if the server drops it otherwise, the request waits for its deadline (or `ldrop accept`).
async fn prompt(notifier: Notifier, mut pending: PendingRequest) {
    let note = {
        let r = &pending.request;
        let (summary, body) = prompt_text(&r.peer.name, &r.items, r.bytes_total, &r.pin);
        Notification {
            actions: vec![("accept", "Accept"), ("reject", "Decline")],
            urgency: Urgency::Critical,
            timeout_ms: i32::try_from(r.time_left().as_millis()).unwrap_or(i32::MAX),
            ..Notification::new(summary, body)
        }
    };
    let (tx, mut replies) = mpsc::unbounded_channel();
    let id = match notifier.show(&note, 0, tx).await {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!("cannot show the request from {}: {e}", pending.request.peer.name);
            return pending.expired().await;
        }
    };
    let decision = loop {
        tokio::select! {
            reply = replies.recv() => match reply {
                Some(Reply::Action(a)) if a == "accept" => break Some(Decision::Accept),
                Some(Reply::Action(a)) if a == "reject" => break Some(Decision::Decline),
                Some(Reply::Closed(DISMISSED)) => break Some(Decision::Decline),
                Some(Reply::Action(_)) => {}
                Some(Reply::Closed(_)) | None => break None,
            },
            () = pending.expired() => break None,
        }
    };
    notifier.close(id).await;
    match decision {
        Some(decision) => pending.respond(decision),
        None => pending.expired().await,
    }
}

#[cfg(test)]
mod tests {
    use ldrop_core::time::now_us;
    use ldrop_core::{DeviceKind, Peer, Platform, Protocol};
    use zbus::OwnedObjectPath;

    use super::*;

    fn status(visibility: Visibility, expires_at: u64) -> Status {
        Status {
            device_name: "thinkpad".into(),
            visibility,
            visibility_expires_at: expires_at,
            download_dir: "/home/u/Downloads".into(),
        }
    }

    fn transfer(id: u32, direction: Direction, state: TransferState) -> TransferInfo {
        TransferInfo {
            path: OwnedObjectPath::try_from(format!("/org/ldrop/Ldrop1/transfer/{id}")).unwrap(),
            id,
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
            items: Vec::new(),
            bytes_total: 4_000,
            bytes_done: 1_000,
            error: String::new(),
            deadline: 0,
            created_at: 0,
            preview: Vec::new(),
            preview_mime: String::new(),
        }
    }

    #[test]
    fn offline() {
        let v = view(&State::default());
        assert_eq!(v.visibility, None);
        assert_eq!(v.status, "ldropd is not running");
    }

    #[test]
    fn visibility_and_countdown() {
        let hidden = State { status: Some(status(Visibility::Hidden, 0)), ..Default::default() };
        assert_eq!(view(&hidden).icon, "view-hidden");
        assert_eq!(view(&hidden).visibility, Some(Visibility::Hidden));
        let expires = now_us() + 7 * 60 * 1_000_000 - 1_000_000;
        let everyone = State { status: Some(status(Visibility::Everyone, expires)), ..Default::default() };
        let v = view(&everyone);
        assert_eq!((v.icon, v.status.as_str()), ("view-visible", "Visible as thinkpad · 7 min left"));
    }

    #[test]
    fn transfers_and_requests() {
        let mut state = State { status: Some(status(Visibility::Everyone, 0)), ..Default::default() };
        state.transfers.insert(1, transfer(1, Direction::Incoming, TransferState::AwaitingAcceptance));
        assert!(view(&state).attention);
        state.transfers.insert(1, transfer(1, Direction::Incoming, TransferState::Transferring));
        let v = view(&state);
        assert!(!v.attention);
        assert_eq!((v.icon, v.status.as_str()), ("folder-download", "Receiving from Pixel 9 · 25%"));
    }
}
