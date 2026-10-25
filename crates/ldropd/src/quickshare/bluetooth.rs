// SPDX-License-Identifier: GPL-3.0-or-later
//! Quick Share over Bluetooth classic (SPEC §5.2), the way Nearby runs on Linux and ChromeOS:
//! while visible, the adapter takes the encoded device name and turns discoverable, and a
//! legacy BLE advertisement makes Android phones look for it; an RFCOMM service takes their
//! connections. Discovery reads the names of Quick Share computers nearby and the BLE
//! advertisements of Quick Share phones, and sending connects to their RFCOMM service.
//!
//! Slow (RFCOMM manages about 100–200 KB/s), but it needs no shared network.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ldrop_ble::Advertisement;
use ldrop_bt::{Adapter, Incoming, Rfcomm, Seen, Visible};
use ldrop_quickshare::Medium as QsMedium;
use ldrop_quickshare::bluetooth::{
    Advertised, COPRESENCE_SERVICE_UUID, LEGACY_WAKE_DATA, RFCOMM_SERVICE_UUID, device_name, parse_ble_advertisement,
    parse_device_name,
};
use ldrop_quickshare::{EndpointId, EndpointInfo};
use tokio::net::UnixStream;
use tokio_util::sync::{CancellationToken, DropGuard};

use super::{Backend, Origin, admit, backend, device_id, found, lost};
use crate::state::Shared;

/// Holds the adapter's own name and visibility while ldrop has changed them.
pub const MARKER_FILE: &str = "bt-adapter.json";
/// The name in our RFCOMM service's SDP record: Nearby's service ID.
const SERVICE_NAME: &str = "NearbySharing";

pub struct Bluetooth {
    marker: PathBuf,
    state: tokio::sync::Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Set up on first use, and again after the adapter went away.
    radio: Option<Radio>,
    /// Set while visible.
    shown: Option<Shown>,
}

/// The adapter in use and our RFCOMM service on it.
struct Radio {
    adapter: Adapter,
    rfcomm: Rfcomm,
    /// Stops taking connections when the radio is replaced.
    _accepting: DropGuard,
}

struct Shown {
    visible: Visible,
    wake: Option<Advertisement>,
}

impl Bluetooth {
    pub fn new(marker: PathBuf) -> Self {
        Bluetooth { marker, state: tokio::sync::Mutex::new(State::default()) }
    }

    /// The powered adapter and our service on it, set up if needed.
    async fn radio(&self, s: &Arc<Shared>, state: &mut State) -> Result<(Adapter, Rfcomm), String> {
        if let Some(radio) = &state.radio {
            if radio.adapter.powered().await.unwrap_or(false) {
                return Ok((radio.adapter.clone(), radio.rfcomm.clone()));
            }
            tracing::info!("Bluetooth: {} went away", radio.adapter.path());
            state.radio = None;
        }
        let adapter = Adapter::first_powered()
            .await
            .map_err(|e| format!("BlueZ is unavailable: {e}"))?
            .ok_or("no powered Bluetooth adapter")?;
        let (rfcomm, incoming) = Rfcomm::register(&adapter, RFCOMM_SERVICE_UUID, SERVICE_NAME)
            .await
            .map_err(|e| format!("cannot register the RFCOMM service: {e}"))?;
        let accepting = CancellationToken::new();
        tokio::spawn(accept_loop(s.clone(), incoming, accepting.clone()));
        tracing::info!("Quick Share on Bluetooth uses {}", adapter.path());
        state.radio =
            Some(Radio { adapter: adapter.clone(), rfcomm: rfcomm.clone(), _accepting: accepting.drop_guard() });
        Ok((adapter, rfcomm))
    }

    /// Makes us discoverable as endpoint `id` until [`hide`](Self::hide).
    pub async fn show(&self, s: &Arc<Shared>, id: EndpointId, info: &EndpointInfo) {
        let mut state = self.state.lock().await;
        if state.shown.is_some() {
            return;
        }
        let adapter = match self.radio(s, &mut state).await {
            Ok((adapter, _)) => adapter,
            Err(e) => return tracing::info!("Quick Share is not visible over Bluetooth: {e}"),
        };
        let visible = match adapter.make_visible(&device_name(id, info), &self.marker).await {
            Ok(visible) => visible,
            Err(e) => return tracing::warn!("Quick Share is not visible over Bluetooth: {e}"),
        };
        // Android only scans for Bluetooth classic devices after seeing this advertisement
        // (docs/research/quickshare-upgrade.md §1).
        let wake = match ldrop_ble::advertise_service_data(COPRESENCE_SERVICE_UUID, LEGACY_WAKE_DATA.to_vec()).await {
            Ok(wake) => wake,
            Err(e) => {
                tracing::warn!("Bluetooth: Android phones may not find this computer, BLE advertising failed: {e}");
                None
            }
        };
        tracing::debug!("Quick Share visible over Bluetooth");
        state.shown = Some(Shown { visible, wake });
    }

    /// Puts the adapter's name and visibility back.
    pub async fn hide(&self) {
        let shown = self.state.lock().await.shown.take();
        if let Some(Shown { visible, wake }) = shown {
            if let Some(wake) = wake {
                wake.stop().await;
            }
            visible.restore().await;
            tracing::debug!("Quick Share hidden over Bluetooth");
        }
    }

    /// Reports Quick Share devices discoverable over Bluetooth until `token` is cancelled.
    pub async fn discover(&self, s: &Arc<Shared>, qs: &Backend, token: CancellationToken) {
        let radio = {
            let mut state = self.state.lock().await;
            self.radio(s, &mut state).await
        };
        let adapter = match radio {
            Ok((adapter, _)) => adapter,
            Err(e) => return tracing::info!("Quick Share discovery runs without Bluetooth: {e}"),
        };
        let mut scan = match adapter.discover().await {
            Ok(scan) => scan,
            Err(e) => return tracing::warn!("Bluetooth: cannot scan: {e}"),
        };
        // Device IDs of the Quick Share devices seen, by Bluetooth address.
        let mut seen: HashMap<String, String> = HashMap::new();
        loop {
            let event = tokio::select! {
                () = token.cancelled() => break,
                event = scan.next() => event,
            };
            match event {
                Some(Seen::Device { address, name, service_data }) => {
                    let advertised = quick_share_device(&address, name.as_deref(), &service_data)
                        .filter(|(a, _)| qs.own_endpoint() != Some(a.id));
                    match advertised {
                        Some((advertised, classic)) => {
                            let id = device_id(advertised.id);
                            // A new visibility session on the phone means a new endpoint ID.
                            if let Some(old) = seen.insert(address.clone(), id.clone())
                                && old != id
                            {
                                lost(s, qs, &old, |route| route.bt = None).await;
                            }
                            if !qs.inner().peers.contains_key(&id) {
                                tracing::debug!(
                                    "Quick Share: found {:?} over Bluetooth at {classic} (seen at {address})",
                                    advertised.info.name
                                );
                            }
                            found(s, qs, advertised.id, &advertised.info, |route| route.bt = Some(classic)).await;
                        }
                        // The phone took its own name back: it is no longer visible.
                        None => {
                            if let Some(id) = seen.remove(&address) {
                                lost(s, qs, &id, |route| route.bt = None).await;
                            }
                        }
                    }
                }
                Some(Seen::Lost { address }) => {
                    if let Some(id) = seen.remove(&address) {
                        lost(s, qs, &id, |route| route.bt = None).await;
                    }
                }
                None => {
                    tracing::warn!("Bluetooth: scanning stopped");
                    token.cancelled().await;
                    break;
                }
            }
        }
        drop(scan);
        for id in seen.into_values() {
            lost(s, qs, &id, |route| route.bt = None).await;
        }
    }

    /// Connects to the Quick Share service of the device at `address`.
    pub async fn connect(&self, s: &Arc<Shared>, address: &str) -> Result<UnixStream, String> {
        let radio = {
            let mut state = self.state.lock().await;
            self.radio(s, &mut state).await
        };
        let (_, rfcomm) = radio?;
        rfcomm.connect(address).await.map(|conn| conn.stream).map_err(|e| e.to_string())
    }
}

/// A Quick Share device in a discovery sighting, and the Bluetooth classic address its
/// RFCOMM service listens on.
///
/// Computers announce themselves the legacy way, with an encoded device name, at that very
/// address. Android phones advertise over BLE (Copresence service data), from a random
/// address, naming their classic address inside; such a phone is not discoverable over
/// classic at all. Advertisements without a classic address (fast ones, from hidden
/// devices) cannot be reached and are skipped.
fn quick_share_device(
    address: &str,
    name: Option<&str>,
    service_data: &HashMap<String, Vec<u8>>,
) -> Option<(Advertised, String)> {
    if let Some(advertised) = name.and_then(parse_device_name) {
        return Some((advertised, address.to_owned()));
    }
    let advertised = parse_ble_advertisement(service_data.get(COPRESENCE_UUID)?)?;
    let classic = advertised.bluetooth_address.clone()?;
    Some((advertised, classic))
}

/// [`COPRESENCE_SERVICE_UUID`] as BlueZ names it in `ServiceData`.
const COPRESENCE_UUID: &str = "0000fef3-0000-1000-8000-00805f9b34fb";

async fn accept_loop(s: Arc<Shared>, mut incoming: Incoming, stop: CancellationToken) {
    loop {
        let conn = tokio::select! {
            () = stop.cancelled() => return,
            conn = incoming.next() => conn,
        };
        let (Some(conn), Some(qs)) = (conn, backend(&s)) else { return };
        admit(&s, &qs, conn.stream, Origin::Bluetooth(conn.address), QsMedium::Bluetooth);
    }
}

/// Puts back the adapter name and visibility a crashed run left behind.
pub async fn restore_after_crash(marker: &Path) {
    if let Err(e) = ldrop_bt::restore_after_crash(marker).await {
        tracing::warn!("Bluetooth: cannot restore the adapter name after an unclean exit: {e}");
    }
}
