// SPDX-License-Identifier: GPL-3.0-or-later
//! `org.ldrop.Ldrop1.Manager` at `/org/ldrop/Ldrop1` (SPEC §10).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ldrop_core::config::{self, MAX_VISIBILITY_TIMEOUT_SECS};
use ldrop_core::dbus::{CapabilitiesDict, MANAGER_IFACE, ROOT_PATH};
use ldrop_core::time::now_us;
use ldrop_core::uri::file_uri_to_path;
use ldrop_core::{Direction, Item, MIME_TEXT, MIME_URL, TransferState, Visibility, VisibilityError};
use zbus::fdo::Properties;
use zbus::message::Header;
use zbus::names::InterfaceName;
use zbus::object_server::SignalEmitter;
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant, interface};

use crate::error::{Error, Result};
use crate::state::{AgentReg, Shared};
use crate::{discovery, doctor, sim, transfer};

/// `Capabilities` is re-probed on read when older than this.
const CAPABILITIES_MAX_AGE: Duration = Duration::from_secs(3);

pub struct Manager {
    pub s: Arc<Shared>,
}

fn sender(hdr: &Header<'_>) -> Result<String> {
    hdr.sender().map(|s| s.to_string()).ok_or_else(|| Error::Failed("message has no sender".into()))
}

#[interface(name = "org.ldrop.Ldrop1.Manager")]
impl Manager {
    #[zbus(property)]
    fn device_name(&self) -> String {
        self.s.device_name()
    }

    #[zbus(property)]
    fn set_device_name(&mut self, name: String) -> zbus::fdo::Result<()> {
        let name = name.trim().to_owned();
        if name.chars().count() > 100 || name.chars().any(char::is_control) {
            return Err(zbus::fdo::Error::InvalidArgs("device names are at most 100 printable characters".into()));
        }
        self.write_config("device_name", &name)
    }

    #[zbus(property)]
    fn visibility(&self) -> String {
        self.s.state().visibility.to_string()
    }

    #[zbus(property)]
    fn visibility_expires_at(&self) -> u64 {
        self.s.state().visibility_expires_at
    }

    #[zbus(property)]
    fn download_dir(&self) -> String {
        self.s.download_dir()
    }

    #[zbus(property)]
    fn set_download_dir(&mut self, dir: String) -> zbus::fdo::Result<()> {
        if !dir.is_empty() {
            let path = Path::new(&dir);
            if !path.is_absolute() {
                return Err(zbus::fdo::Error::InvalidArgs("download dir must be an absolute path".into()));
            }
            if !path.is_dir() {
                return Err(zbus::fdo::Error::InvalidArgs(format!("{dir} is not a directory")));
            }
        }
        self.write_config("download_dir", &dir)
    }

    #[zbus(property)]
    async fn capabilities(&self) -> CapabilitiesDict {
        let fresh = self.s.state().report_at.is_some_and(|t| t.elapsed() < CAPABILITIES_MAX_AGE);
        if !fresh {
            doctor::update(&self.s).await;
        }
        CapabilitiesDict::from(&self.s.state().report)
    }

    /// `hidden` or `everyone`; `timeout_secs` 0 = configured default (600 s).
    async fn set_visibility(&self, mode: &str, timeout_secs: u32) -> Result<()> {
        let mode = Visibility::parse_request(mode).map_err(|e| match e {
            VisibilityError::NotSupported(m) => {
                Error::NotSupported(format!("visibility `{m}` needs vendor account certificates"))
            }
            VisibilityError::Unknown(e) => Error::Failed(e.to_string()),
        })?;
        let timeout = match timeout_secs {
            0 => self.s.state().config.visibility_timeout_secs,
            n => n.min(MAX_VISIBILITY_TIMEOUT_SECS),
        };
        set_visibility(&self.s, mode, Duration::from_secs(timeout.into())).await;
        Ok(())
    }

    #[zbus(out_args("session"))]
    async fn start_discovery(&self, #[zbus(header)] hdr: Header<'_>) -> Result<OwnedObjectPath> {
        Ok(discovery::start(&self.s, sender(&hdr)?).await?)
    }

    /// `opts` is reserved for per-send options; none are defined yet.
    #[zbus(out_args("transfer"))]
    async fn send(
        &self,
        device_id: &str,
        uris: Vec<String>,
        opts: HashMap<String, OwnedVariant>,
    ) -> Result<OwnedObjectPath> {
        for key in opts.keys() {
            tracing::debug!("Send: ignoring unknown option `{key}`");
        }
        if uris.is_empty() {
            return Err(Error::Failed("nothing to send".into()));
        }
        let items = uris.iter().map(|u| file_item(u)).collect::<Result<Vec<_>>>()?;
        self.start_send(device_id, items).await
    }

    #[zbus(out_args("transfer"))]
    async fn send_text(&self, device_id: &str, text: &str, is_url: bool) -> Result<OwnedObjectPath> {
        if text.is_empty() {
            return Err(Error::Failed("nothing to send".into()));
        }
        let item = Item {
            name: if is_url { text.to_owned() } else { "Text".to_owned() },
            mime: if is_url { MIME_URL } else { MIME_TEXT }.to_owned(),
            size: text.len() as u64,
            path: None,
            text: Some(text.to_owned()),
        };
        self.start_send(device_id, vec![item]).await
    }

    #[zbus(out_args("session", "url"))]
    async fn create_qr_session(&self) -> Result<(OwnedObjectPath, String)> {
        Err(Error::NotSupported("QR-code sharing is not implemented yet (milestone M11)".into()))
    }

    async fn accept(&self, transfer: ObjectPath<'_>) -> Result<()> {
        self.decide(&transfer, true)
    }

    async fn reject(&self, transfer: ObjectPath<'_>) -> Result<()> {
        self.decide(&transfer, false)
    }

    async fn cancel(&self, transfer: ObjectPath<'_>) -> Result<()> {
        let id = transfer_id(&transfer)?;
        let st = self.s.state();
        let rec = st.transfers.get(&id).ok_or_else(|| Error::NotFound(format!("no transfer {transfer}")))?;
        if rec.props.state.is_final() {
            return Err(Error::Failed(format!("transfer already {}", rec.props.state)));
        }
        rec.cancel.cancel();
        Ok(())
    }

    async fn register_agent(&self, #[zbus(header)] hdr: Header<'_>, agent: ObjectPath<'_>) -> Result<()> {
        let reg = AgentReg { owner: sender(&hdr)?, path: agent.into() };
        let mut st = self.s.state();
        st.agents.retain(|a| a != &reg);
        tracing::info!("agent registered: {} {}", reg.owner, reg.path);
        st.agents.push(reg);
        Ok(())
    }

    async fn unregister_agent(&self, #[zbus(header)] hdr: Header<'_>, agent: ObjectPath<'_>) -> Result<()> {
        let reg = AgentReg { owner: sender(&hdr)?, path: agent.into() };
        let mut st = self.s.state();
        let before = st.agents.len();
        st.agents.retain(|a| a != &reg);
        if st.agents.len() == before {
            return Err(Error::NotFound("agent not registered".into()));
        }
        Ok(())
    }

    /// Active transfers plus the last 50 finished ones, oldest first.
    #[zbus(out_args("transfers"))]
    async fn list_transfers(&self) -> Vec<OwnedObjectPath> {
        self.s.state().transfers.keys().map(|id| transfer::path(*id)).collect()
    }

    async fn clear_history(&self) -> Result<()> {
        transfer::clear_history(&self.s).await.map_err(|e| Error::Failed(format!("cannot clear history: {e}")))
    }

    #[zbus(signal)]
    pub async fn incoming_request(emitter: &SignalEmitter<'_>, transfer: ObjectPath<'_>) -> zbus::Result<()>;
}

impl Manager {
    fn write_config(&self, key: &str, value: &str) -> zbus::fdo::Result<()> {
        let config =
            config::set(&self.s.paths.config_file, key, value).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;
        self.s.state().config = config;
        Ok(())
    }

    fn decide(&self, transfer: &ObjectPath<'_>, accept: bool) -> Result<()> {
        let id = transfer_id(transfer)?;
        let mut st = self.s.state();
        let rec = st.transfers.get_mut(&id).ok_or_else(|| Error::NotFound(format!("no transfer {transfer}")))?;
        if rec.props.direction != Direction::Incoming || rec.props.state != TransferState::AwaitingAcceptance {
            return Err(Error::Failed("transfer is not waiting for acceptance".into()));
        }
        let tx = rec.decision.take().ok_or_else(|| Error::Failed("transfer is not waiting for acceptance".into()))?;
        let _ = tx.send(accept);
        Ok(())
    }

    async fn start_send(&self, device_id: &str, items: Vec<Item>) -> Result<OwnedObjectPath> {
        let device = self
            .s
            .state()
            .devices
            .iter()
            .find(|d| d.id == device_id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no device `{device_id}`; start discovery first")))?;
        if sim::is_sim_device(&device) {
            let id = sim::start_outgoing(&self.s, &device, items).await?;
            return Ok(transfer::path(id));
        }
        Err(Error::Unavailable(format!("no {} backend is available yet", device.protocol)))
    }
}

fn transfer_id(path: &ObjectPath<'_>) -> Result<u32> {
    transfer::id_from_path(path.as_str()).ok_or_else(|| Error::NotFound(format!("{path} is not a transfer")))
}

fn file_item(uri: &str) -> Result<Item> {
    let path = file_uri_to_path(uri).ok_or_else(|| Error::Failed(format!("not a local file:// URI: {uri}")))?;
    let meta = std::fs::metadata(&path).map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(Error::NotSupported(format!("{} is not a regular file", path.display())));
    }
    Ok(Item {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        mime: mime_guess::from_path(&path).first_or_octet_stream().essence_str().to_owned(),
        size: meta.len(),
        path: Some(path.display().to_string()),
        text: None,
    })
}

/// Sets visibility, emits the property changes, and arms the auto-revert timer.
pub async fn set_visibility(s: &Arc<Shared>, mode: Visibility, timeout: Duration) {
    let generation = apply_visibility(s, mode, timeout).await;
    if mode == Visibility::Everyone {
        let s = s.clone();
        tokio::spawn(async move {
            tokio::time::sleep(timeout).await;
            let current = s.state().visibility_generation == generation;
            if current {
                apply_visibility(&s, Visibility::Hidden, Duration::ZERO).await;
            }
        });
    }
}

/// Returns the new visibility generation.
async fn apply_visibility(s: &Arc<Shared>, mode: Visibility, timeout: Duration) -> u64 {
    let (generation, expires_at) = {
        let mut st = s.state();
        st.visibility = mode;
        st.visibility_expires_at = match mode {
            Visibility::Everyone => now_us() + timeout.as_micros() as u64,
            Visibility::Hidden => 0,
        };
        st.visibility_generation += 1;
        (st.visibility_generation, st.visibility_expires_at)
    };
    tracing::info!("visibility: {mode}");
    s.visibility_tx.send_replace((mode, expires_at));
    emit_visibility(s, mode, expires_at).await;
    generation
}

/// One `PropertiesChanged` for both properties, so clients never see a half-applied change.
async fn emit_visibility(s: &Shared, mode: Visibility, expires_at: u64) {
    let Ok(em) = SignalEmitter::new(&s.conn, ROOT_PATH) else { return };
    let changed = HashMap::from([
        ("Visibility", Variant::from(mode.as_str())),
        ("VisibilityExpiresAt", Variant::from(expires_at)),
    ]);
    let iface = InterfaceName::from_static_str_unchecked(MANAGER_IFACE);
    if let Err(e) = Properties::properties_changed(&em, iface, changed, Cow::Borrowed(&[])).await {
        tracing::warn!("cannot emit visibility change: {e}");
    }
}

/// Emits `DeviceName`/`DownloadDir` changes after the config file was edited externally.
pub async fn emit_config_props(s: &Shared, name: bool, dir: bool) {
    let Ok(iface_ref) = s.conn.object_server().interface::<_, Manager>(ROOT_PATH).await else { return };
    let iface = iface_ref.get().await;
    let em = iface_ref.signal_emitter();
    if name {
        let _ = iface.device_name_changed(em).await;
    }
    if dir {
        let _ = iface.download_dir_changed(em).await;
    }
}

pub async fn emit_capabilities(s: &Shared) {
    let Ok(iface_ref) = s.conn.object_server().interface::<_, Manager>(ROOT_PATH).await else { return };
    let iface = iface_ref.get().await;
    let _ = iface.capabilities_changed(iface_ref.signal_emitter()).await;
}
