// SPDX-License-Identifier: GPL-3.0-or-later
//! The Agent (SPEC §10, §11.1): `ldropd` asks the active frontend to accept or decline incoming
//! transfers. The client serves `org.ldrop.Ldrop1.Agent` and hands each request to the frontend.

use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use ldrop_core::dbus::{BUS_NAME, RequestInfoDict};
use ldrop_core::time::remaining;
use ldrop_core::{Item, Peer, TransferState};
use tokio::sync::{mpsc, oneshot};
use zbus::message::Type as MessageType;
use zbus::{MatchRule, MessageStream, OwnedObjectPath, interface};

use crate::transfers::transfer_id;
use crate::{Client, Result};

/// Path prefix of agent objects served by this client.
const AGENT_PATH_PREFIX: &str = "/org/ldrop/client/agent";

/// Extra wait past the deadline, so the daemon's own timeout decides the outcome.
const DEADLINE_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Accept,
    Decline,
}

/// An incoming transfer waiting for the user's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct IncomingRequest {
    pub transfer: OwnedObjectPath,
    pub id: u32,
    pub peer: Peer,
    /// Quick Share PIN to compare with the sender's screen; empty for AirDrop.
    pub pin: String,
    pub items: Vec<Item>,
    pub bytes_total: u64,
    /// Unix µs.
    pub deadline: u64,
}

impl IncomingRequest {
    fn new(transfer: OwnedObjectPath, info: RequestInfoDict) -> Self {
        IncomingRequest {
            id: transfer_id(transfer.as_str()).unwrap_or(0),
            transfer,
            peer: Peer::from(&info.peer.unwrap_or_default()),
            pin: info.pin.unwrap_or_default(),
            items: info.items.iter().flatten().map(Item::from).collect(),
            bytes_total: info.bytes_total.unwrap_or(0),
            deadline: info.deadline.unwrap_or(0),
        }
    }

    /// Time left to answer.
    pub fn time_left(&self) -> Duration {
        remaining(self.deadline).unwrap_or(Duration::ZERO)
    }
}

/// A request handed to a frontend through [`Client::register_agent_channel`].
#[derive(Debug)]
pub struct PendingRequest {
    pub request: IncomingRequest,
    reply: oneshot::Sender<bool>,
}

impl PendingRequest {
    pub fn respond(self, decision: Decision) {
        let _ = self.reply.send(decision == Decision::Accept);
    }

    /// Resolves once an answer is no longer wanted: the deadline passed, or the transfer was
    /// accepted, declined or cancelled elsewhere. Frontends close their prompt then.
    pub async fn expired(&mut self) {
        self.reply.closed().await;
    }
}

/// Implemented by frontends that prompt the user. See [`Client::serve_agent`].
pub trait Agent: Send + Sync + 'static {
    fn request_accept(&self, request: IncomingRequest) -> impl Future<Output = Decision> + Send;
}

/// A registered agent. Dropping it leaves the agent registered until this client disconnects.
pub struct AgentRegistration {
    client: Client,
    path: OwnedObjectPath,
}

impl AgentRegistration {
    pub fn path(&self) -> &OwnedObjectPath {
        &self.path
    }

    pub async fn unregister(self) -> Result<()> {
        let result = self.client.manager().unregister_agent(&self.path.as_ref()).await;
        let _ = self.client.connection().object_server().remove::<AgentIface, _>(&self.path).await;
        result
    }
}

struct AgentIface {
    client: Client,
    tx: mpsc::Sender<PendingRequest>,
}

#[interface(name = "org.ldrop.Ldrop1.Agent")]
impl AgentIface {
    #[zbus(out_args("accept"))]
    async fn request_accept(&self, transfer: OwnedObjectPath, info: RequestInfoDict) -> bool {
        let request = IncomingRequest::new(transfer, info);
        let path = request.transfer.clone();
        let wait = request.time_left() + DEADLINE_GRACE;
        let (reply, rx) = oneshot::channel();
        if self.tx.send(PendingRequest { request, reply }).await.is_err() {
            return false;
        }
        tokio::select! {
            answer = rx => answer.unwrap_or(false),
            _ = tokio::time::sleep(wait) => false,
            // Dropping `rx` here tells the frontend its prompt expired.
            _ = decided_elsewhere(&self.client, &path) => false,
        }
    }
}

/// Resolves when the transfer leaves `AwaitingAcceptance`.
async fn decided_elsewhere(client: &Client, path: &OwnedObjectPath) {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(BUS_NAME)
        .path(path.as_ref())
        .interface("org.freedesktop.DBus.Properties")
        .member("PropertiesChanged")
        .build();
    let stream = match rule {
        Ok(rule) => MessageStream::for_match_rule(rule, client.connection(), None).await,
        Err(e) => Err(e),
    };
    let Ok(mut stream) = stream else { return std::future::pending().await };
    loop {
        match client.transfer(&path.as_ref()).await {
            Ok(t) if t.state == TransferState::AwaitingAcceptance => {}
            _ => return,
        }
        if stream.next().await.is_none() {
            return std::future::pending().await;
        }
    }
}

static NEXT_AGENT: AtomicU32 = AtomicU32::new(0);

impl Client {
    /// Serves an agent object and registers it with the daemon. Requests arrive on the
    /// returned channel; each must be answered with [`PendingRequest::respond`] before it
    /// expires. Unanswered requests time out; nothing is ever auto-accepted.
    pub async fn register_agent_channel(&self) -> Result<(AgentRegistration, mpsc::Receiver<PendingRequest>)> {
        let n = NEXT_AGENT.fetch_add(1, Ordering::Relaxed);
        let path = OwnedObjectPath::try_from(format!("{AGENT_PATH_PREFIX}{n}"))?;
        let (tx, rx) = mpsc::channel(8);
        self.connection().object_server().at(&path, AgentIface { client: self.clone(), tx }).await?;
        if let Err(e) = self.manager().register_agent(&path.as_ref()).await {
            let _ = self.connection().object_server().remove::<AgentIface, _>(&path).await;
            return Err(e);
        }
        Ok((AgentRegistration { client: self.clone(), path }, rx))
    }

    /// Registers `agent`; each request runs on its own task.
    pub async fn serve_agent<A: Agent>(&self, agent: A) -> Result<AgentRegistration> {
        let (registration, mut rx) = self.register_agent_channel().await?;
        let agent = std::sync::Arc::new(agent);
        tokio::spawn(async move {
            while let Some(mut pending) = rx.recv().await {
                let agent = agent.clone();
                tokio::spawn(async move {
                    let request = pending.request.clone();
                    let decision = tokio::select! {
                        decision = agent.request_accept(request) => Some(decision),
                        _ = pending.expired() => None,
                    };
                    if let Some(decision) = decision {
                        pending.respond(decision);
                    }
                });
            }
        });
        Ok(registration)
    }
}
