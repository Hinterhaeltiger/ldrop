// SPDX-License-Identifier: GPL-3.0-or-later
//! Accept/decline prompts (SPEC §1.3, §10): the registered Agent, else a desktop notification
//! with Accept/Decline actions. `Manager.Accept`/`Reject` answer at any time. Never
//! auto-accepts: no answer by the deadline is a timeout.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use ldrop_core::dbus::{ItemDict, PeerDict, ROOT_PATH, RequestInfoDict};
use ldrop_core::prompt_text;
use ldrop_core::time::remaining;
use tokio::sync::oneshot;
use zbus::object_server::SignalEmitter;
use zbus::{ObjectPath, Variant, proxy};

use crate::manager::Manager;
use crate::state::{AgentReg, Shared};
use crate::transfer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Reject,
    Timeout,
    Cancelled,
}

#[proxy(interface = "org.ldrop.Ldrop1.Agent", assume_defaults = false)]
trait Agent {
    fn request_accept(&self, transfer: &ObjectPath<'_>, info: RequestInfoDict) -> zbus::Result<bool>;
}

#[proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Variant<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// Asks the user about incoming transfer `id`, which must be in `AwaitingAcceptance` with its
/// `deadline` set. Emits `IncomingRequest` first. `withdrawn` resolving (the sender gave up)
/// ends the prompt as `Cancelled`.
pub async fn ask(s: &Arc<Shared>, id: u32, withdrawn: impl Future<Output = ()>) -> Decision {
    let (tx, rx) = oneshot::channel();
    let Some((deadline, cancel, info)) = ({
        let mut st = s.state();
        st.transfers.get_mut(&id).map(|rec| {
            rec.decision = Some(tx);
            let p = &rec.props;
            let info = RequestInfoDict {
                peer: Some(PeerDict::from(&p.peer)),
                pin: (!p.pin.is_empty()).then(|| p.pin.clone()),
                items: Some(p.items.iter().map(ItemDict::from).collect()),
                bytes_total: Some(p.bytes_total),
                deadline: Some(p.deadline),
            };
            (p.deadline, rec.cancel.clone(), info)
        })
    }) else {
        return Decision::Cancelled;
    };
    let path = transfer::path(id);
    if let Ok(em) = SignalEmitter::new(&s.conn, ROOT_PATH)
        && let Err(e) = Manager::incoming_request(&em, path.as_ref()).await
    {
        tracing::warn!("cannot emit IncomingRequest: {e}");
    }

    let time_left = remaining(deadline).unwrap_or(Duration::ZERO);
    let agent = s.state().agents.last().cloned();
    let notification = AtomicU32::new(0);
    let prompt = async {
        if let Some(agent) = &agent {
            match call_agent(s, agent, &path, info.clone()).await {
                Ok(answer) => return Some(answer),
                Err(e) => tracing::warn!("agent {} failed ({e}); falling back to a notification", agent.owner),
            }
        }
        notify(s, &info, time_left, &notification).await
    };

    let decision = tokio::select! {
        biased;
        _ = cancel.cancelled() => Decision::Cancelled,
        () = withdrawn => Decision::Cancelled,
        answer = rx => match answer {
            Ok(true) => Decision::Accept,
            _ => Decision::Reject,
        },
        Some(answer) = prompt => if answer { Decision::Accept } else { Decision::Reject },
        _ = tokio::time::sleep(time_left) => Decision::Timeout,
    };

    let notification = notification.load(Ordering::Relaxed);
    if notification != 0
        && let Ok(proxy) = NotificationsProxy::new(&s.conn).await
    {
        let _ = proxy.close_notification(notification).await;
    }
    decision
}

async fn call_agent(s: &Shared, agent: &AgentReg, path: &ObjectPath<'_>, info: RequestInfoDict) -> zbus::Result<bool> {
    // No timeout here: the caller's deadline ends the wait, and the agent's late reply is ignored.
    let proxy =
        AgentProxy::builder(&s.conn).destination(agent.owner.as_str()).path(agent.path.as_ref()).build().await?;
    proxy.request_accept(path, info).await
}

/// Posts the prompt as a notification and waits for an action. `None` when there is no
/// notification server or the notification was dismissed without an answer.
async fn notify(s: &Shared, info: &RequestInfoDict, time_left: Duration, id_out: &AtomicU32) -> Option<bool> {
    let proxy = NotificationsProxy::new(&s.conn).await.ok()?;
    let mut actions = proxy.receive_action_invoked().await.ok()?;
    let mut closed = proxy.receive_notification_closed().await.ok()?;

    let peer = info.peer.as_ref().and_then(|p| p.name.clone()).unwrap_or_default();
    let items: Vec<_> = info.items.iter().flatten().map(ldrop_core::Item::from).collect();
    let (summary, body) =
        prompt_text(&peer, &items, info.bytes_total.unwrap_or(0), info.pin.as_deref().unwrap_or_default());
    let hints = HashMap::from([
        ("urgency", Variant::from(2u8)),
        ("category", Variant::from("transfer")),
        ("desktop-entry", Variant::from("org.ldrop.Ldrop")),
    ]);
    let id = proxy
        .notify(
            "ldrop",
            0,
            "document-send",
            &summary,
            &body,
            &["accept", "Accept", "reject", "Decline"],
            hints,
            time_left.as_millis().min(i32::MAX as u128) as i32,
        )
        .await
        .map_err(|e| tracing::info!("no notification server ({e}); waiting for Accept/Reject over D-Bus"))
        .ok()?;
    id_out.store(id, Ordering::Relaxed);

    loop {
        tokio::select! {
            Some(signal) = actions.next() => {
                let Ok(args) = signal.args() else { continue };
                if *args.id() != id {
                    continue;
                }
                match args.action_key().as_str() {
                    "accept" => return Some(true),
                    "reject" => return Some(false),
                    _ => {}
                }
            }
            Some(signal) = closed.next() => {
                if signal.args().is_ok_and(|a| *a.id() == id) {
                    id_out.store(0, Ordering::Relaxed);
                    return None;
                }
            }
            else => return None,
        }
    }
}
