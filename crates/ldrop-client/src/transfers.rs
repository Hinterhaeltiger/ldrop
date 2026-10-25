// SPDX-License-Identifier: GPL-3.0-or-later
//! Transfer snapshots and a live view of all transfers (SPEC §10, §11.1).

use std::collections::{BTreeMap, HashMap};

use futures_util::StreamExt;
use ldrop_core::dbus::{BUS_NAME, ItemDict, PeerDict, ROOT_PATH, TRANSFER_IFACE, TRANSFER_PATH_PREFIX};
use ldrop_core::{Direction, Item, Peer, TransferState};
use zbus::message::Type as MessageType;
use zbus::{DeserializeDict, MatchRule, MessageStream, ObjectPath, OwnedObjectPath, OwnedVariant, Type};

use crate::{Client, Error, Result};

/// One transfer as the daemon reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferInfo {
    pub path: OwnedObjectPath,
    /// Last path segment, what `ldrop accept|reject|cancel` take.
    pub id: u32,
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

/// Body of `Properties.GetAll("org.ldrop.Ldrop1.Transfer")`.
#[derive(Debug, Default, DeserializeDict, Type)]
#[zbus(signature = "a{sv}", rename_all = "PascalCase")]
struct TransferAll {
    direction: Option<String>,
    state: Option<String>,
    peer: Option<PeerDict>,
    pin: Option<String>,
    items: Option<Vec<ItemDict>>,
    bytes_total: Option<u64>,
    bytes_done: Option<u64>,
    error: Option<String>,
    deadline: Option<u64>,
    created_at: Option<u64>,
    preview: Option<Vec<u8>>,
    preview_mime: Option<String>,
}

/// Parses the `N` of `/org/ldrop/Ldrop1/transfer/N`.
pub fn transfer_id(path: &str) -> Option<u32> {
    path.strip_prefix(TRANSFER_PATH_PREFIX)?.strip_prefix('/')?.parse().ok()
}

/// Accepts `7` or a full object path.
pub fn transfer_path(id_or_path: &str) -> Option<OwnedObjectPath> {
    if id_or_path.starts_with('/') {
        transfer_id(id_or_path)?;
        OwnedObjectPath::try_from(id_or_path).ok()
    } else {
        let id: u32 = id_or_path.parse().ok()?;
        OwnedObjectPath::try_from(format!("{TRANSFER_PATH_PREFIX}/{id}")).ok()
    }
}

impl TransferInfo {
    fn from_all(path: OwnedObjectPath, all: TransferAll) -> Self {
        TransferInfo {
            id: transfer_id(path.as_str()).unwrap_or(0),
            path,
            direction: all.direction.and_then(|d| d.parse().ok()).unwrap_or(Direction::Incoming),
            state: all.state.and_then(|s| s.parse().ok()).unwrap_or(TransferState::Failed),
            peer: Peer::from(&all.peer.unwrap_or_default()),
            pin: all.pin.unwrap_or_default(),
            items: all.items.iter().flatten().map(Item::from).collect(),
            bytes_total: all.bytes_total.unwrap_or(0),
            bytes_done: all.bytes_done.unwrap_or(0),
            error: all.error.unwrap_or_default(),
            deadline: all.deadline.unwrap_or(0),
            created_at: all.created_at.unwrap_or(0),
            preview: all.preview.unwrap_or_default(),
            preview_mime: all.preview_mime.unwrap_or_default(),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.state.is_final()
    }

    /// Incoming and waiting for this user's answer.
    pub fn needs_answer(&self) -> bool {
        self.direction == Direction::Incoming && self.state == TransferState::AwaitingAcceptance
    }
}

impl Client {
    pub async fn transfer(&self, path: &ObjectPath<'_>) -> Result<TransferInfo> {
        let reply = self
            .connection()
            .call_method(Some(BUS_NAME), path, Some("org.freedesktop.DBus.Properties"), "GetAll", &(TRANSFER_IFACE,))
            .await?;
        let all: TransferAll = reply.body().deserialize()?;
        Ok(TransferInfo::from_all(path.to_owned().into(), all))
    }

    /// Active transfers and recent history, oldest first.
    pub async fn transfers(&self) -> Result<Vec<TransferInfo>> {
        let mut out = Vec::new();
        for path in self.manager().list_transfers().await? {
            match self.transfer(&path.as_ref()).await {
                Ok(t) => out.push(t),
                // Removed between listing and reading (history eviction).
                Err(Error::ZBus(zbus::Error::MethodError(..))) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Live view of all transfers: the current list plus a stream of changes.
    pub async fn watch_transfers(&self) -> Result<TransferWatch> {
        let rule =
            MatchRule::builder().msg_type(MessageType::Signal).sender(BUS_NAME).path_namespace(ROOT_PATH).build()?;
        let stream = MessageStream::for_match_rule(rule, self.connection(), None).await?;
        let transfers = self.transfers().await?.into_iter().map(|t| (t.id, t)).collect();
        Ok(TransferWatch { client: self.clone(), stream, transfers })
    }

    /// Follows one transfer until it reaches a final state, calling `on_change` on each update.
    pub async fn follow(
        &self,
        path: &ObjectPath<'_>,
        mut on_change: impl FnMut(&TransferInfo),
    ) -> Result<TransferInfo> {
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(BUS_NAME)
            .path(path.to_owned())
            .interface("org.freedesktop.DBus.Properties")
            .member("PropertiesChanged")
            .build()?;
        let mut stream = MessageStream::for_match_rule(rule, self.connection(), None).await?;
        let mut info = self.transfer(path).await?;
        on_change(&info);
        while !info.state.is_final() {
            if stream.next().await.is_none() {
                return Err(Error::Failed("connection to ldropd closed".into()));
            }
            let next = self.transfer(path).await?;
            if next != info {
                info = next;
                on_change(&info);
            }
        }
        Ok(info)
    }
}

/// A change seen by [`TransferWatch`].
#[derive(Debug, Clone, PartialEq)]
pub enum TransferEvent {
    /// A new transfer appeared (incoming request or a send).
    Added(TransferInfo),
    Changed(TransferInfo),
    /// Removed from the daemon (history eviction or `ClearHistory`).
    Removed(u32),
}

pub struct TransferWatch {
    client: Client,
    stream: MessageStream,
    transfers: BTreeMap<u32, TransferInfo>,
}

impl TransferWatch {
    /// Current transfers, oldest first.
    pub fn transfers(&self) -> impl Iterator<Item = &TransferInfo> {
        self.transfers.values()
    }

    /// Next change, or `None` when the bus connection closed.
    pub async fn next(&mut self) -> Option<TransferEvent> {
        while let Some(msg) = self.stream.next().await {
            let Ok(msg) = msg else { continue };
            let header = msg.header();
            let member = header.member().map(|m| m.to_string()).unwrap_or_default();
            let path = match member.as_str() {
                "PropertiesChanged" => header.path().map(|p| p.to_string()),
                "InterfacesAdded" => msg
                    .body()
                    .deserialize::<(OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>)>()
                    .ok()
                    .map(|(p, _)| p.to_string()),
                "InterfacesRemoved" => {
                    msg.body().deserialize::<(OwnedObjectPath, Vec<String>)>().ok().map(|(p, _)| p.to_string())
                }
                _ => None,
            };
            let Some(path) = path else { continue };
            let Some(id) = transfer_id(&path) else { continue };

            if member == "InterfacesRemoved" {
                if self.transfers.remove(&id).is_some() {
                    return Some(TransferEvent::Removed(id));
                }
                continue;
            }
            let Ok(path) = OwnedObjectPath::try_from(path) else { continue };
            let Ok(info) = self.client.transfer(&path.as_ref()).await else { continue };
            match self.transfers.insert(id, info.clone()) {
                None => return Some(TransferEvent::Added(info)),
                Some(old) if old != info => return Some(TransferEvent::Changed(info)),
                Some(_) => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_paths() {
        assert_eq!(transfer_path("7").unwrap().as_str(), "/org/ldrop/Ldrop1/transfer/7");
        assert_eq!(transfer_path("/org/ldrop/Ldrop1/transfer/7").unwrap().as_str(), "/org/ldrop/Ldrop1/transfer/7");
        assert!(transfer_path("/org/ldrop/Ldrop1/discovery/7").is_none());
        assert!(transfer_path("x").is_none());
    }
}
