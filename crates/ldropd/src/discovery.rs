// SPDX-License-Identifier: GPL-3.0-or-later
//! Discovery sessions (`/org/ldrop/Ldrop1/discovery/N`, SPEC §1.2, §8, §10) and the merged
//! device registry they report from.

use std::sync::Arc;

use ldrop_core::Device;
use ldrop_core::dbus::{DISCOVERY_PATH_PREFIX, DeviceDict};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{OwnedObjectPath, interface};

use crate::error::{Error, Result};
use crate::state::{SessionReg, Shared};
use crate::{airdrop, quickshare, sim};

pub fn path(id: u32) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{DISCOVERY_PATH_PREFIX}/{id}")).expect("valid object path")
}

pub struct DiscoveryIface {
    s: Arc<Shared>,
    id: u32,
}

#[interface(name = "org.ldrop.Ldrop1.Discovery")]
impl DiscoveryIface {
    /// Ends the session. Only the client that started it may stop it.
    async fn stop(&self, #[zbus(header)] hdr: Header<'_>) -> Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        let owner = self.s.state().sessions.get(&self.id).map(|r| r.owner.clone());
        match owner {
            None => Err(Error::NotFound("discovery session already stopped".into())),
            Some(owner) if owner != sender => Err(Error::PermissionDenied("session belongs to another client".into())),
            Some(_) => {
                stop(&self.s, self.id).await;
                Ok(())
            }
        }
    }

    #[zbus(signal)]
    async fn device_found(emitter: &SignalEmitter<'_>, id: &str, props: DeviceDict) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn device_lost(emitter: &SignalEmitter<'_>, id: &str) -> zbus::Result<()>;
}

/// Starts a session for `owner` and reports every device already known to it.
pub async fn start(s: &Arc<Shared>, owner: String) -> zbus::Result<OwnedObjectPath> {
    let (id, first) = {
        let mut st = s.state();
        let id = st.next_session;
        st.next_session += 1;
        st.sessions.insert(id, SessionReg { owner });
        (id, st.sessions.len() == 1)
    };
    let path = path(id);
    s.conn.object_server().at(&path, DiscoveryIface { s: s.clone(), id }).await?;
    tracing::debug!("discovery session {id} started");

    // Report after the method reply so the caller knows the path first.
    let s2 = s.clone();
    tokio::spawn(async move {
        tokio::task::yield_now().await;
        let devices = s2.state().devices.clone();
        for device in &devices {
            emit_found(&s2, id, device).await;
        }
        if first && s2.sim {
            sim::on_discovery_started(&s2).await;
        }
    });
    tokio::spawn(quickshare::sync_discovery(s.clone()));
    tokio::spawn(airdrop::sync_discovery(s.clone()));
    Ok(path)
}

pub async fn stop(s: &Arc<Shared>, id: u32) {
    if s.state().sessions.remove(&id).is_some() {
        let _ = s.conn.object_server().remove::<DiscoveryIface, _>(path(id)).await;
        tracing::debug!("discovery session {id} stopped");
        tokio::spawn(quickshare::sync_discovery(s.clone()));
        tokio::spawn(airdrop::sync_discovery(s.clone()));
    }
}

/// Ends every session owned by a client that left the bus.
pub async fn stop_owned_by(s: &Arc<Shared>, owner: &str) {
    let ids: Vec<u32> = s.state().sessions.iter().filter(|(_, r)| r.owner == owner).map(|(id, _)| *id).collect();
    for id in ids {
        stop(s, id).await;
    }
}

/// A backend saw a device. New devices go to every running session; known ones are updated.
pub async fn device_found(s: &Arc<Shared>, device: Device) {
    let sessions: Vec<u32> = {
        let mut st = s.state();
        if let Some(known) = st.devices.iter_mut().find(|d| d.id == device.id) {
            *known = device;
            return;
        }
        st.devices.push(device.clone());
        st.sessions.keys().copied().collect()
    };
    for id in sessions {
        emit_found(s, id, &device).await;
    }
}

/// A backend no longer sees a device.
pub async fn device_lost(s: &Arc<Shared>, id: &str) {
    let sessions: Vec<u32> = {
        let mut st = s.state();
        let before = st.devices.len();
        st.devices.retain(|d| d.id != id);
        if st.devices.len() == before {
            return;
        }
        st.sessions.keys().copied().collect()
    };
    for session in sessions {
        if let Ok(em) = SignalEmitter::new(&s.conn, path(session))
            && let Err(e) = DiscoveryIface::device_lost(&em, id).await
        {
            tracing::warn!("cannot emit DeviceLost: {e}");
        }
    }
}

async fn emit_found(s: &Shared, session: u32, device: &Device) {
    if let Ok(em) = SignalEmitter::new(&s.conn, path(session))
        && let Err(e) = DiscoveryIface::device_found(&em, &device.id, DeviceDict::from(device)).await
    {
        tracing::warn!("cannot emit DeviceFound: {e}");
    }
}
