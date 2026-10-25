// SPDX-License-Identifier: GPL-3.0-or-later
//! Client library for ldrop frontends (SPEC §11.1): typed proxies, discovery and transfer
//! streams, and the Agent for accept/decline prompts. Frontends hold no protocol code.

mod agent;
pub mod proxies;
mod transfers;

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use ldrop_core::capabilities::DoctorReport;
use ldrop_core::dbus::{BUS_NAME, DISCOVERY_IFACE, DISCOVERY_PATH_PREFIX, DeviceDict, MANAGER_IFACE, ROOT_PATH};
use ldrop_core::uri::path_to_file_uri;
use ldrop_core::{Device, Visibility};
use zbus::fdo::{DBusProxy, PropertiesProxy};
use zbus::message::Type as MessageType;
use zbus::names::{InterfaceName, WellKnownName};
use zbus::{Connection, MatchRule, MessageStream, ObjectPath, OwnedObjectPath};

pub use agent::{Agent, AgentRegistration, Decision, IncomingRequest, PendingRequest};
pub use ldrop_core;
pub use proxies::{Error, Result};
pub use transfers::{TransferEvent, TransferInfo, TransferWatch, transfer_id, transfer_path};
pub use zbus;

use proxies::{DiscoveryProxy, ManagerProxy};

/// A connection to `ldropd`.
#[derive(Clone)]
pub struct Client {
    conn: Connection,
    manager: ManagerProxy<'static>,
}

/// Snapshot of the daemon's top-level state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub device_name: String,
    pub visibility: Visibility,
    /// Unix µs; 0 when hidden.
    pub visibility_expires_at: u64,
    pub download_dir: String,
}

/// What a discovery session reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    Found(Device),
    Lost(String),
}

impl Client {
    /// Connects to the session bus. The daemon is D-Bus activated on first use.
    pub async fn connect() -> Result<Self> {
        Self::with_connection(Connection::session().await?).await
    }

    pub async fn with_connection(conn: Connection) -> Result<Self> {
        let manager = ManagerProxy::new(&conn).await?;
        Ok(Client { conn, manager })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn manager(&self) -> &ManagerProxy<'static> {
        &self.manager
    }

    /// Reads the daemon state directly (not from the proxy cache), so it reflects every method
    /// call that returned before.
    pub async fn status(&self) -> Result<Status> {
        let props = PropertiesProxy::builder(&self.conn).destination(BUS_NAME).path(ROOT_PATH).build().await?;
        let iface = InterfaceName::from_static_str_unchecked(MANAGER_IFACE);
        let get = |name: &'static str| {
            let props = &props;
            let iface = iface.clone();
            async move { props.get(iface, name).await }
        };
        Ok(Status {
            device_name: String::try_from(get("DeviceName").await?)?,
            visibility: String::try_from(get("Visibility").await?)?.parse().unwrap_or(Visibility::Hidden),
            visibility_expires_at: u64::try_from(get("VisibilityExpiresAt").await?)?,
            download_dir: String::try_from(get("DownloadDir").await?)?,
        })
    }

    pub async fn capabilities(&self) -> Result<DoctorReport> {
        Ok(self.manager.capabilities().await?.to_report())
    }

    /// `timeout` zero = the configured default (10 min).
    pub async fn set_visibility(&self, mode: Visibility, timeout: Duration) -> Result<()> {
        let secs = u32::try_from(timeout.as_secs()).unwrap_or(u32::MAX);
        self.manager.set_visibility(mode.as_str(), secs).await
    }

    /// Starts a discovery session. It ends with [`DiscoverySession::stop`] or when this client
    /// disconnects.
    pub async fn discover(&self) -> Result<DiscoverySession> {
        // Subscribe before starting so no early DeviceFound is missed.
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(BUS_NAME)
            .interface(DISCOVERY_IFACE)
            .path_namespace(DISCOVERY_PATH_PREFIX)
            .build()?;
        let stream = MessageStream::for_match_rule(rule, &self.conn, None).await?;
        let path = self.manager.start_discovery().await?;
        Ok(DiscoverySession { conn: self.conn.clone(), path, stream })
    }

    pub async fn send_files(&self, device_id: &str, paths: &[impl AsRef<Path>]) -> Result<OwnedObjectPath> {
        let mut uris = Vec::with_capacity(paths.len());
        for p in paths {
            let abs =
                std::path::absolute(p.as_ref()).map_err(|e| Error::Failed(format!("{}: {e}", p.as_ref().display())))?;
            uris.push(path_to_file_uri(&abs));
        }
        let uris: Vec<&str> = uris.iter().map(String::as_str).collect();
        self.manager.send(device_id, &uris, HashMap::new()).await
    }

    pub async fn send_text(&self, device_id: &str, text: &str, is_url: bool) -> Result<OwnedObjectPath> {
        self.manager.send_text(device_id, text, is_url).await
    }

    pub async fn accept(&self, transfer: &ObjectPath<'_>) -> Result<()> {
        self.manager.accept(transfer).await
    }

    pub async fn reject(&self, transfer: &ObjectPath<'_>) -> Result<()> {
        self.manager.reject(transfer).await
    }

    pub async fn cancel(&self, transfer: &ObjectPath<'_>) -> Result<()> {
        self.manager.cancel(transfer).await
    }

    pub async fn clear_history(&self) -> Result<()> {
        self.manager.clear_history().await
    }

    /// Resolves once the daemon has left the bus (stopped, crashed or restarting). Agents and
    /// discovery sessions registered with it are gone then.
    pub async fn daemon_exited(&self) -> Result<()> {
        let dbus = DBusProxy::new(&self.conn).await?;
        let mut changes = dbus.receive_name_owner_changed_with_args(&[(0, BUS_NAME)]).await?;
        let name = WellKnownName::from_static_str_unchecked(BUS_NAME);
        if !dbus.name_has_owner(name.into()).await? {
            return Ok(());
        }
        while let Some(signal) = changes.next().await {
            if signal.args().is_ok_and(|a| a.new_owner().is_none()) {
                return Ok(());
            }
        }
        Ok(())
    }
}

/// A running discovery session.
pub struct DiscoverySession {
    conn: Connection,
    path: OwnedObjectPath,
    stream: MessageStream,
}

impl DiscoverySession {
    pub fn path(&self) -> &OwnedObjectPath {
        &self.path
    }

    /// Next device change, or `None` when the bus connection closed.
    pub async fn next(&mut self) -> Option<DiscoveryEvent> {
        while let Some(msg) = self.stream.next().await {
            let Ok(msg) = msg else { continue };
            let header = msg.header();
            if header.path().map(|p| p.as_str()) != Some(self.path.as_str()) {
                continue;
            }
            let body = msg.body();
            match header.member().map(|m| m.as_str()) {
                Some("DeviceFound") => {
                    if let Ok((id, props)) = body.deserialize::<(String, DeviceDict)>() {
                        return Some(DiscoveryEvent::Found(props.to_device(&id)));
                    }
                }
                Some("DeviceLost") => {
                    if let Ok((id,)) = body.deserialize::<(String,)>() {
                        return Some(DiscoveryEvent::Lost(id));
                    }
                }
                _ => {}
            }
        }
        None
    }

    pub async fn stop(self) -> Result<()> {
        DiscoveryProxy::builder(&self.conn).path(self.path).build().await?.stop().await
    }
}
