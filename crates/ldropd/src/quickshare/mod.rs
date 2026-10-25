// SPDX-License-Identifier: GPL-3.0-or-later
//! Quick Share (SPEC §5): on the LAN, a TCP listener that takes requests while visible, the
//! mDNS service published per visibility session, mDNS browsing plus the BLE wake while a
//! discovery session runs, and outgoing transfers; over Bluetooth, the same through
//! [`bluetooth`].

mod bluetooth;

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Context;
use ldrop_core::paths::resolve_download_dir;
use ldrop_core::storage::{self, Staging, sanitize_name};
use ldrop_core::time::now_us;
use ldrop_core::{
    Device, DeviceKind, Direction, Item, MIME_TEXT, MIME_URL, MIME_WIFI, Medium, Peer, Platform, Protocol,
    TransferState, Visibility,
};
use ldrop_mdns::{Event, Mdns, Published};
use ldrop_quickshare::endpoint::{MDNS_SERVICE_TYPE, VENDOR_SAMSUNG, mdns_instance_name, parse_mdns_instance_name};
use ldrop_quickshare::link::{BoxStream, Stream};
use ldrop_quickshare::receive::{self, Answer, InboundEvent, Offer, ReceiveOutcome, Received};
use ldrop_quickshare::send::{self, OutboundEvent, Outgoing, OutgoingFile, SendOutcome};
use ldrop_quickshare::{
    DeviceType, EndpointId, EndpointInfo, Error as QsError, Link, LocalEndpoint, Medium as QsMedium, RejectReason,
    RemoteEndpoint, TextKind,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::consent::{self, Decision};
use crate::discovery;
use crate::error::{Error, Result};
use crate::state::Shared;
use crate::transfer::{self, TransferRecord};

const DEVICE_PREFIX: &str = "qs-";
/// At most this many prompts wait at once, and one per peer (SPEC §9).
const MAX_PROMPTS: usize = 3;
const MAX_PROMPTS_PER_PEER: usize = 1;
/// A peer whose request was declined or timed out cannot ask again for this long (SPEC §14).
const REJECT_COOLDOWN: Duration = Duration::from_secs(30);
/// Connections being served at once, handshakes included.
const MAX_INCOMING: usize = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Backend {
    /// `None` when the `lan` medium is off or its port could not be bound.
    lan: Option<Lan>,
    bt: bluetooth::Bluetooth,
    incoming: Arc<Semaphore>,
    /// Serializes starting and stopping discovery.
    discovery: tokio::sync::Mutex<Option<DiscoveryRun>>,
    inner: Mutex<Inner>,
}

struct Lan {
    mdns: Mdns,
    port: u16,
}

#[derive(Default)]
struct Inner {
    /// Set while visible; a new endpoint ID per visibility session.
    visible: Option<Visible>,
    /// How to reach the devices the current discovery run reported, by device ID.
    peers: HashMap<String, Route>,
    prompts: HashMap<Origin, usize>,
    cooldown: HashMap<Origin, Instant>,
}

struct Visible {
    id: EndpointId,
    /// The mDNS publication, when the LAN is on.
    _mdns: Option<Published>,
}

/// Where a connection came from. Prompt limits and cooldowns apply per origin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Origin {
    Ip(IpAddr),
    /// A Bluetooth address.
    Bluetooth(String),
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Ip(ip) => ip.fmt(f),
            Origin::Bluetooth(address) => write!(f, "Bluetooth {address}"),
        }
    }
}

/// The ways to reach a device found by discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Route {
    lan: Vec<SocketAddr>,
    /// Its Bluetooth address.
    bt: Option<String>,
}

impl Route {
    fn mediums(&self) -> Vec<Medium> {
        let lan = (!self.lan.is_empty()).then_some(Medium::Lan);
        lan.into_iter().chain(self.bt.as_ref().map(|_| Medium::Bt)).collect()
    }

    fn is_empty(&self) -> bool {
        self.lan.is_empty() && self.bt.is_none()
    }
}

struct DiscoveryRun {
    token: CancellationToken,
    task: JoinHandle<()>,
}

impl Backend {
    fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn own_endpoint(&self) -> Option<EndpointId> {
        self.inner().visible.as_ref().map(|v| v.id)
    }

    /// Reserves a prompt for `origin`, or `None` when the limits or a cooldown say no.
    fn try_prompt(self: &Arc<Self>, origin: &Origin) -> Option<PromptSlot> {
        let mut inner = self.inner();
        let now = Instant::now();
        inner.cooldown.retain(|_, until| *until > now);
        let total: usize = inner.prompts.values().sum();
        let mine = inner.prompts.get(origin).copied().unwrap_or(0);
        if inner.cooldown.contains_key(origin) || total >= MAX_PROMPTS || mine >= MAX_PROMPTS_PER_PEER {
            return None;
        }
        *inner.prompts.entry(origin.clone()).or_default() += 1;
        Some(PromptSlot { qs: self.clone(), origin: origin.clone() })
    }

    fn cool_down(&self, origin: &Origin) {
        self.inner().cooldown.insert(origin.clone(), Instant::now() + REJECT_COOLDOWN);
    }
}

/// A pending prompt, released when dropped.
struct PromptSlot {
    qs: Arc<Backend>,
    origin: Origin,
}

impl Drop for PromptSlot {
    fn drop(&mut self) {
        let mut inner = self.qs.inner();
        if let Some(n) = inner.prompts.get_mut(&self.origin) {
            *n -= 1;
            if *n == 0 {
                inner.prompts.remove(&self.origin);
            }
        }
    }
}

fn backend(s: &Shared) -> Option<Arc<Backend>> {
    s.quickshare.get().cloned()
}

/// Whether Quick Share is on with `medium` in the config.
fn uses(s: &Shared, medium: Medium) -> bool {
    let st = s.state();
    st.config.quickshare.enabled && st.config.quickshare.mediums.contains(&medium)
}

fn enabled(s: &Shared) -> bool {
    uses(s, Medium::Lan) || uses(s, Medium::Bt)
}

/// Binds the listener and starts following visibility. Needs `quickshare.enabled` at startup,
/// and the `lan` medium for the listener; later config changes are honored for new
/// connections and visibility sessions.
pub async fn start(s: &Arc<Shared>) -> anyhow::Result<()> {
    let marker = s.paths.state_dir.join(bluetooth::MARKER_FILE);
    bluetooth::restore_after_crash(&marker).await;
    if !enabled(s) {
        tracing::info!("Quick Share is disabled in the config");
        return Ok(());
    }
    let (lan, listener) = if uses(s, Medium::Lan) {
        match start_lan(s).await {
            Ok((lan, listener)) => (Some(lan), Some(listener)),
            Err(e) => {
                tracing::error!("Quick Share on the LAN is unavailable: {e:#}");
                (None, None)
            }
        }
    } else {
        tracing::info!("Quick Share on the LAN is disabled in the config");
        (None, None)
    };
    let qs = Arc::new(Backend {
        lan,
        bt: bluetooth::Bluetooth::new(marker),
        incoming: Arc::new(Semaphore::new(MAX_INCOMING)),
        discovery: tokio::sync::Mutex::new(None),
        inner: Mutex::new(Inner::default()),
    });
    if s.quickshare.set(qs).is_err() {
        anyhow::bail!("Quick Share backend started twice");
    }
    if let Some(listener) = listener {
        tokio::spawn(accept_loop(s.clone(), listener));
    }
    tokio::spawn(follow_visibility(s.clone()));
    Ok(())
}

async fn start_lan(s: &Shared) -> anyhow::Result<(Lan, TcpListener)> {
    let port = s.state().config.quickshare.tcp_port;
    let listener = bind(port).await.with_context(|| format!("cannot listen on TCP port {port}"))?;
    let port = listener.local_addr()?.port();
    let mdns = Mdns::new().context("cannot start mDNS")?;
    tracing::info!("Quick Share listening on TCP port {port}");
    Ok((Lan { mdns, port }, listener))
}

/// Puts the Bluetooth adapter back before the daemon exits.
pub async fn shutdown(s: &Shared) {
    if let Some(qs) = backend(s) {
        qs.inner().visible = None;
        qs.bt.hide().await;
    }
}

/// Dual-stack where IPv6 is available.
async fn bind(port: u16) -> std::io::Result<TcpListener> {
    match TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)).await {
        Ok(l) => Ok(l),
        Err(_) => TcpListener::bind(("0.0.0.0", port)).await,
    }
}

/// Announces us while visible, with a fresh endpoint ID each time (SPEC §5.1, §5.2, §14):
/// the mDNS service on the LAN, the encoded adapter name over Bluetooth.
async fn follow_visibility(s: Arc<Shared>) {
    let Some(qs) = backend(&s) else { return };
    let mut rx = s.visibility_tx.subscribe();
    loop {
        let (mode, _) = *rx.borrow_and_update();
        let visible = mode == Visibility::Everyone && enabled(&s);
        let shown = qs.own_endpoint().is_some();
        if visible && !shown {
            let id = EndpointId::random();
            let info = EndpointInfo::visible(&s.device_name(), DeviceType::Laptop);
            let mdns = match &qs.lan {
                Some(lan) if uses(&s, Medium::Lan) => {
                    match lan.mdns.publish(
                        MDNS_SERVICE_TYPE,
                        &mdns_instance_name(id),
                        lan.port,
                        &[("n", &info.to_txt())],
                    ) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::error!("cannot publish the Quick Share service: {e}");
                            None
                        }
                    }
                }
                _ => None,
            };
            qs.inner().visible = Some(Visible { id, _mdns: mdns });
            if uses(&s, Medium::Bt) {
                qs.bt.show(&s, id, &info).await;
            }
        } else if !visible && shown {
            qs.inner().visible = None;
            qs.bt.hide().await;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn accept_loop(s: Arc<Shared>, listener: TcpListener) {
    let Some(qs) = backend(&s) else { return };
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!("Quick Share: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        admit(&s, &qs, stream, Origin::Ip(addr.ip().to_canonical()), QsMedium::WifiLan);
    }
}

/// Serves an incoming connection while visible and within the connection limit.
fn admit(s: &Arc<Shared>, qs: &Arc<Backend>, stream: impl Stream, origin: Origin, medium: QsMedium) {
    if qs.own_endpoint().is_none() {
        tracing::debug!("Quick Share: dropping connection from {origin} while hidden");
        return;
    }
    let Ok(permit) = qs.incoming.clone().try_acquire_owned() else {
        tracing::warn!("Quick Share: dropping connection from {origin}: too many connections");
        return;
    };
    let (s, qs) = (s.clone(), qs.clone());
    tokio::spawn(async move {
        let _permit = permit;
        Incoming::serve(s, qs, stream, origin, medium).await;
    });
}

// ---------------------------------------------------------------------------------------------
// Receiving

struct Incoming {
    s: Arc<Shared>,
    qs: Arc<Backend>,
    origin: Origin,
    cancel: CancellationToken,
    /// Set once the offer became a transfer.
    id: Option<u32>,
    /// Number of files and texts in the offer; see [`items`].
    layout: (usize, usize),
    accepted: Option<(Staging, PathBuf)>,
}

impl Incoming {
    async fn serve(s: Arc<Shared>, qs: Arc<Backend>, stream: impl Stream, origin: Origin, medium: QsMedium) {
        let cancel = CancellationToken::new();
        let mut this = Incoming { s, qs, origin, cancel: cancel.clone(), id: None, layout: (0, 0), accepted: None };
        let (tx, rx) = mpsc::channel(16);
        let link = Link::fixed(medium);
        let (result, ()) = tokio::join!(receive::receive(stream, &link, tx, cancel), this.handle(rx));
        this.finish(result).await;
    }

    async fn handle(&mut self, mut rx: mpsc::Receiver<InboundEvent>) {
        while let Some(event) = rx.recv().await {
            match event {
                InboundEvent::Offer(offer, reply) => self.on_offer(*offer, reply).await,
                InboundEvent::Progress { bytes_done } => {
                    if let Some(id) = self.id {
                        transfer::update(&self.s, id, |p| p.bytes_done = bytes_done).await;
                    }
                }
            }
        }
    }

    async fn on_offer(&mut self, offer: Offer, mut reply: tokio::sync::oneshot::Sender<Answer>) {
        let Some(slot) = self.qs.try_prompt(&self.origin) else {
            tracing::info!(
                "Quick Share: declining a request from {}: too many pending requests, or declined recently",
                self.origin
            );
            let _ = reply.send(Answer::Reject(RejectReason::Declined));
            return;
        };
        tracing::debug!("Quick Share: request from {} ({})", offer.peer.display_name(), self.origin);
        let mut record = TransferRecord::new(
            Direction::Incoming,
            TransferState::AwaitingAcceptance,
            peer(&offer.peer),
            items(&offer),
        );
        record.cancel = self.cancel.clone();
        record.props.pin = offer.pin.clone();
        let timeout = u64::from(self.s.state().config.quickshare.prompt_timeout_secs);
        record.props.deadline = now_us() + timeout * 1_000_000;
        let id = match transfer::create(&self.s, record).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("cannot export transfer: {e}");
                let _ = reply.send(Answer::Reject(RejectReason::Declined));
                return;
            }
        };
        self.id = Some(id);
        self.layout = (offer.files.len(), offer.texts.len());

        let dir = resolve_download_dir(&self.s.state().config.download_dir);
        if !has_room(&dir, offer.total_size()) {
            let _ = reply.send(Answer::Reject(RejectReason::NotEnoughSpace));
            transfer::update(&self.s, id, |p| {
                p.state = TransferState::Rejected;
                p.error = format!("not enough free space in {}", dir.display());
            })
            .await;
            return;
        }

        let decision = consent::ask(&self.s, id, reply.closed()).await;
        drop(slot);
        match decision {
            Decision::Accept => match Staging::create(&dir) {
                Ok(staging) => {
                    let _ = reply.send(Answer::Accept { staging: staging.path().to_owned() });
                    self.accepted = Some((staging, dir));
                    transfer::update(&self.s, id, |p| {
                        p.state = TransferState::Transferring;
                        p.deadline = 0;
                    })
                    .await;
                }
                Err(e) => {
                    let _ = reply.send(Answer::Reject(RejectReason::Declined));
                    transfer::update(&self.s, id, |p| {
                        p.state = TransferState::Failed;
                        p.error = format!("cannot write to {}: {e}", dir.display());
                    })
                    .await;
                }
            },
            Decision::Reject | Decision::Timeout => {
                self.qs.cool_down(&self.origin);
                let (reason, error) = match decision {
                    Decision::Timeout => (RejectReason::TimedOut, "no answer before the deadline"),
                    _ => (RejectReason::Declined, ""),
                };
                let _ = reply.send(Answer::Reject(reason));
                transfer::update(&self.s, id, |p| {
                    p.state = TransferState::Rejected;
                    p.error = error.into();
                })
                .await;
            }
            // The session ends with Cancelled or PeerCancelled; `finish` records which.
            Decision::Cancelled => drop(reply),
        }
    }

    async fn finish(self, result: std::result::Result<ReceiveOutcome, QsError>) {
        let Some(id) = self.id else {
            if let Err(e) = result {
                tracing::debug!("Quick Share: connection from {} ended: {e}", self.origin);
            }
            return;
        };
        match result {
            Ok(ReceiveOutcome::Completed(received)) => {
                let items = self.s.state().transfers.get(&id).map(|r| r.props.items.clone()).unwrap_or_default();
                match self.commit(items, received) {
                    Ok(items) => {
                        transfer::update(&self.s, id, |p| {
                            p.items = items;
                            p.bytes_done = p.bytes_total;
                            p.state = TransferState::Completed;
                        })
                        .await
                    }
                    Err(e) => fail(&self.s, id, format!("cannot save the received files: {e}")).await,
                }
            }
            // Recorded when the answer was given.
            Ok(ReceiveOutcome::Rejected(_)) => {}
            Err(QsError::Cancelled) => cancelled(&self.s, id, "cancelled on this device").await,
            Err(QsError::PeerCancelled) => cancelled(&self.s, id, "cancelled by the sender").await,
            Err(e) => fail(&self.s, id, e.to_string()).await,
        }
    }

    /// Moves the staged files into the download dir and fills in text and Wi-Fi items.
    fn commit(&self, mut items: Vec<Item>, received: Received) -> std::io::Result<Vec<Item>> {
        let (files, texts) = self.layout;
        let files = files.min(items.len());
        let (file_items, rest) = items.split_at_mut(files);
        let (text_items, wifi_items) = rest.split_at_mut(texts.min(rest.len()));
        if !file_items.is_empty() {
            let (staging, dir) = self.accepted.as_ref().ok_or_else(|| std::io::Error::other("not accepted"))?;
            for (i, item) in file_items.iter_mut().enumerate() {
                let path = storage::commit(&staging.path().join(i.to_string()), dir, &item.name)?;
                item.name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                item.path = Some(path.display().to_string());
            }
        }
        for (item, text) in text_items.iter_mut().zip(received.texts) {
            if item.is_url() {
                item.name = text.clone();
            }
            item.text = Some(text);
        }
        for (item, wifi) in wifi_items.iter_mut().zip(received.wifi) {
            item.text = Some(wifi.password);
        }
        Ok(items)
    }
}

/// Prompt items: files (sanitized names), then texts, then Wi-Fi networks.
fn items(offer: &Offer) -> Vec<Item> {
    let files = offer.files.iter().map(|f| Item {
        name: sanitize_name(&f.name),
        mime: f.mime.clone(),
        size: f.size,
        path: None,
        text: None,
    });
    let texts = offer.texts.iter().map(|t| {
        let url = t.kind == TextKind::Url;
        Item {
            name: if t.title.is_empty() { if url { "Link" } else { "Text" }.to_owned() } else { t.title.clone() },
            mime: if url { MIME_URL } else { MIME_TEXT }.to_owned(),
            size: t.size,
            path: None,
            // Marks a text item until the content arrives (see `Item::text`).
            text: Some(String::new()),
        }
    });
    let wifi = offer.wifi.iter().map(|w| Item {
        name: w.ssid.clone(),
        mime: MIME_WIFI.to_owned(),
        size: 0,
        path: None,
        text: Some(String::new()),
    });
    files.chain(texts).chain(wifi).collect()
}

/// Free-space check on the download dir, or the closest existing parent when it does not exist
/// yet. Errors count as room: the write fails loudly later if there is none.
fn has_room(dir: &Path, bytes: u64) -> bool {
    let existing = dir.ancestors().find(|d| d.is_dir()).unwrap_or(dir);
    storage::has_room(existing, bytes).unwrap_or(true)
}

fn kind(t: DeviceType) -> DeviceKind {
    match t {
        DeviceType::Phone | DeviceType::Foldable => DeviceKind::Phone,
        DeviceType::Tablet => DeviceKind::Tablet,
        DeviceType::Laptop => DeviceKind::Laptop,
        DeviceType::Car => DeviceKind::Car,
        DeviceType::Xr => DeviceKind::Xr,
        DeviceType::Unknown => DeviceKind::Unknown,
    }
}

fn vendor(info: &EndpointInfo) -> Option<String> {
    (info.vendor == Some(VENDOR_SAMSUNG)).then(|| "samsung".to_owned())
}

fn peer(remote: &RemoteEndpoint) -> Peer {
    Peer {
        name: remote.display_name(),
        // Quick Share does not say; Android is by far the most common peer.
        platform: Platform::Android,
        kind: kind(remote.info.device_type),
        protocol: Protocol::QuickShare,
        vendor: vendor(&remote.info),
    }
}

async fn cancelled(s: &Arc<Shared>, id: u32, why: &str) {
    transfer::update(s, id, |p| {
        p.state = TransferState::Cancelled;
        p.error = why.into();
    })
    .await;
}

async fn fail(s: &Arc<Shared>, id: u32, error: String) {
    tracing::warn!("transfer {id} failed: {error}");
    transfer::update(s, id, |p| {
        p.state = TransferState::Failed;
        p.error = error;
    })
    .await;
}

// ---------------------------------------------------------------------------------------------
// Discovery

/// Starts or stops browsing, the BLE wake and the Bluetooth scan so they run exactly while a
/// discovery session exists. Called after every session change.
pub async fn sync_discovery(s: Arc<Shared>) {
    let Some(qs) = backend(&s) else { return };
    let mut run = qs.discovery.lock().await;
    let wanted = !s.state().sessions.is_empty() && enabled(&s);
    match (wanted, run.is_some()) {
        (true, false) => {
            let token = CancellationToken::new();
            let task = tokio::spawn(discover(s.clone(), qs.clone(), token.clone()));
            *run = Some(DiscoveryRun { token, task });
        }
        (false, true) => {
            let DiscoveryRun { token, task } = run.take().expect("checked");
            token.cancel();
            let _ = task.await;
        }
        _ => {}
    }
}

async fn discover(s: Arc<Shared>, qs: Arc<Backend>, token: CancellationToken) {
    tracing::debug!("Quick Share: discovery started");
    let wake = tokio::spawn(ble_wake(token.clone()));
    let bt = uses(&s, Medium::Bt).then(|| {
        let (s, qs, token) = (s.clone(), qs.clone(), token.clone());
        tokio::spawn(async move { qs.bt.discover(&s, &qs, token).await })
    });
    match &qs.lan {
        Some(lan) if uses(&s, Medium::Lan) => browse(&s, &qs, lan, &token).await,
        _ => token.cancelled().await,
    }
    let _ = wake.await;
    if let Some(bt) = bt {
        let _ = bt.await;
    }
    let lost: Vec<String> = qs.inner().peers.drain().map(|(id, _)| id).collect();
    for id in lost {
        discovery::device_lost(&s, &id).await;
    }
    tracing::debug!("Quick Share: discovery stopped");
}

async fn browse(s: &Arc<Shared>, qs: &Arc<Backend>, lan: &Lan, token: &CancellationToken) {
    let mut browser = match lan.mdns.browse(MDNS_SERVICE_TYPE) {
        Ok(browser) => browser,
        Err(e) => {
            tracing::error!("Quick Share: cannot browse mDNS: {e}");
            return token.cancelled().await;
        }
    };
    loop {
        let event = tokio::select! {
            () = token.cancelled() => return,
            event = browser.next() => event,
        };
        match event {
            Some(Event::Resolved(service)) => {
                let Some(endpoint) = parse_mdns_instance_name(&service.instance) else { continue };
                if qs.own_endpoint() == Some(endpoint) || service.addrs.is_empty() {
                    continue;
                }
                let Some(info) = service.txt.get("n").and_then(|n| EndpointInfo::from_txt(n).ok()) else {
                    continue;
                };
                tracing::debug!("Quick Share: found {:?} at {:?}", info.name, service.addrs);
                found(s, qs, endpoint, &info, |route| route.lan = service.addrs).await;
            }
            Some(Event::Removed { instance }) => {
                let Some(endpoint) = parse_mdns_instance_name(&instance) else { continue };
                lost(s, qs, &device_id(endpoint), |route| route.lan.clear()).await;
            }
            None => {
                tracing::error!("Quick Share: mDNS stopped");
                return token.cancelled().await;
            }
        }
    }
}

fn device_id(endpoint: EndpointId) -> String {
    format!("{DEVICE_PREFIX}{endpoint}")
}

/// A medium reported a device: `update` records how to reach it there.
async fn found(
    s: &Arc<Shared>,
    qs: &Backend,
    endpoint: EndpointId,
    info: &EndpointInfo,
    update: impl FnOnce(&mut Route),
) {
    // Hidden endpoints carry no name and are only reachable by QR code.
    let Some(name) = info.name.clone() else { return };
    let id = device_id(endpoint);
    let mediums = {
        let mut inner = qs.inner();
        let route = inner.peers.entry(id.clone()).or_default();
        update(route);
        route.mediums()
    };
    let device = Device {
        id,
        name,
        kind: kind(info.device_type),
        platform: Platform::Android,
        protocol: Protocol::QuickShare,
        mediums,
        vendor: vendor(info),
    };
    discovery::device_found(s, device).await;
}

/// A medium lost a device: `update` forgets the way to reach it there. Gone once no medium
/// reaches it.
async fn lost(s: &Arc<Shared>, qs: &Backend, id: &str, update: impl FnOnce(&mut Route)) {
    let mediums = {
        let mut inner = qs.inner();
        let Some(route) = inner.peers.get_mut(id) else { return };
        update(route);
        if route.is_empty() {
            inner.peers.remove(id);
        }
        inner.peers.get(id).map(Route::mediums)
    };
    match mediums {
        None => discovery::device_lost(s, id).await,
        Some(mediums) => {
            let device = s.state().devices.iter().find(|d| d.id == id).cloned();
            if let Some(device) = device {
                discovery::device_found(s, Device { mediums, ..device }).await;
            }
        }
    }
}

/// Makes Android devices in Everyone mode publish their mDNS service (SPEC §5.2).
async fn ble_wake(token: CancellationToken) {
    let data = ldrop_quickshare::ble_wake_service_data();
    let adv = tokio::select! {
        () = token.cancelled() => return,
        adv = ldrop_ble::advertise_service_data(ldrop_quickshare::BLE_WAKE_SERVICE_UUID, data) => adv,
    };
    match adv {
        Ok(Some(adv)) => {
            token.cancelled().await;
            adv.stop().await;
        }
        Ok(None) => tracing::info!("Quick Share: no powered Bluetooth adapter; phones may need to be awake to show up"),
        Err(e) => tracing::warn!("Quick Share: BLE wake advertisement failed: {e}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Sending

pub fn is_quickshare_device(device: &Device) -> bool {
    device.id.starts_with(DEVICE_PREFIX)
}

pub async fn start_outgoing(s: &Arc<Shared>, device: &Device, items: Vec<Item>) -> Result<u32> {
    let qs = backend(s).ok_or_else(|| Error::Unavailable("Quick Share is not running".into()))?;
    let route = qs
        .inner()
        .peers
        .get(&device.id)
        .cloned()
        .ok_or_else(|| Error::NotFound(format!("device `{}` is gone; start discovery again", device.id)))?;
    let outgoing = outgoing(&items)?;
    let peer = Peer {
        name: device.name.clone(),
        platform: device.platform,
        kind: device.kind,
        protocol: device.protocol,
        vendor: device.vendor.clone(),
    };
    let record = TransferRecord::new(Direction::Outgoing, TransferState::Connecting, peer, items);
    let cancel = record.cancel.clone();
    let id = transfer::create(s, record).await?;
    tokio::spawn(run_outgoing(s.clone(), qs, id, route, outgoing, cancel));
    Ok(id)
}

fn outgoing(items: &[Item]) -> Result<Outgoing> {
    if let [item] = items
        && let Some(text) = &item.text
    {
        let kind = if item.is_url() { TextKind::Url } else { TextKind::Text };
        return Ok(Outgoing::Text { text: text.clone(), kind });
    }
    let files = items
        .iter()
        .map(|i| {
            let path = i.path.as_ref().ok_or_else(|| Error::NotSupported("text and files cannot be mixed".into()))?;
            Ok(OutgoingFile { path: path.into(), name: i.name.clone(), mime: i.mime.clone() })
        })
        .collect::<Result<_>>()?;
    Ok(Outgoing::Files(files))
}

async fn run_outgoing(
    s: Arc<Shared>,
    qs: Arc<Backend>,
    id: u32,
    route: Route,
    outgoing: Outgoing,
    cancel: CancellationToken,
) {
    let connected = tokio::select! {
        connected = connect(&s, &qs, &route) => connected,
        () = cancel.cancelled() => return cancelled(&s, id, "cancelled on this device").await,
    };
    let (stream, medium) = match connected {
        Ok(connected) => connected,
        Err(e) => return fail(&s, id, format!("cannot connect: {e}")).await,
    };
    let local =
        LocalEndpoint { id: EndpointId::random(), info: EndpointInfo::visible(&s.device_name(), DeviceType::Laptop) };
    let (tx, mut rx) = mpsc::channel(16);
    let events = async {
        while let Some(event) = rx.recv().await {
            match event {
                OutboundEvent::Pin(pin) => {
                    transfer::update(&s, id, |p| {
                        p.state = TransferState::AwaitingAcceptance;
                        p.pin = pin;
                    })
                    .await
                }
                OutboundEvent::Accepted => transfer::update(&s, id, |p| p.state = TransferState::Transferring).await,
                OutboundEvent::Progress { bytes_done } => transfer::update(&s, id, |p| p.bytes_done = bytes_done).await,
            }
        }
    };
    let link = Link::fixed(medium);
    let (result, ()) = tokio::join!(send::send(stream, &local, &link, outgoing, tx, cancel), events);
    match result {
        Ok(SendOutcome::Completed) => {
            transfer::update(&s, id, |p| {
                p.bytes_done = p.bytes_total;
                p.state = TransferState::Completed;
            })
            .await
        }
        Ok(SendOutcome::Rejected(reason)) => {
            transfer::update(&s, id, |p| {
                p.state = TransferState::Rejected;
                p.error = match reason {
                    RejectReason::Declined => "",
                    RejectReason::NotEnoughSpace => "not enough free space on the receiver",
                    RejectReason::Unsupported => "the receiver does not support this content",
                    RejectReason::TimedOut => "no answer before the deadline",
                }
                .into();
            })
            .await
        }
        Err(QsError::Cancelled) => cancelled(&s, id, "cancelled on this device").await,
        Err(QsError::PeerCancelled) => cancelled(&s, id, "cancelled by the receiver").await,
        Err(e) => fail(&s, id, e.to_string()).await,
    }
}

/// Connects over the LAN if the device is there, else over Bluetooth.
async fn connect(s: &Arc<Shared>, qs: &Backend, route: &Route) -> std::result::Result<(BoxStream, QsMedium), String> {
    let mut errors = Vec::new();
    if !route.lan.is_empty() {
        match connect_tcp(&route.lan).await {
            Ok(stream) => return Ok((Box::new(stream), QsMedium::WifiLan)),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if let Some(address) = &route.bt {
        match qs.bt.connect(s, address).await {
            Ok(stream) => return Ok((Box::new(stream), QsMedium::Bluetooth)),
            Err(e) => errors.push(format!("Bluetooth: {e}")),
        }
    }
    Err(errors.join("; "))
}

/// Tries each address in turn (IPv4 first, as mDNS lists them).
async fn connect_tcp(addrs: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, "no address");
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(e)) => last = e,
            Err(_) => last = std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{addr} timed out")),
        }
        tracing::debug!("Quick Share: cannot connect to {addr}: {last}");
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use ldrop_quickshare::WifiSecurity;
    use ldrop_quickshare::receive::{OfferedFile, OfferedText, OfferedWifi};

    use super::*;

    fn offer() -> Offer {
        Offer {
            peer: RemoteEndpoint {
                id: EndpointId::random(),
                info: EndpointInfo::visible("Pixel", DeviceType::Phone),
                endpoint_name: None,
                links: Default::default(),
            },
            pin: "1234".into(),
            files: vec![
                OfferedFile { name: "../evil.sh".into(), mime: "text/x-sh".into(), size: 10 },
                OfferedFile { name: "notes.txt".into(), mime: MIME_TEXT.into(), size: 3 },
            ],
            texts: vec![OfferedText { title: String::new(), kind: TextKind::Url, size: 18 }],
            wifi: vec![OfferedWifi { ssid: "Home".into(), security: WifiSecurity::WpaPsk }],
        }
    }

    #[test]
    fn prompt_items() {
        let items = items(&offer());
        let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["evil.sh", "notes.txt", "Link", "Home"]);
        assert_eq!(items[2].mime, MIME_URL);
        assert_eq!(items[2].text.as_deref(), Some(""), "marked as text before it arrives");
        assert_eq!(items[0].text, None);
        assert_eq!(items[3].mime, MIME_WIFI);
    }

    #[test]
    fn outgoing_from_items() {
        let text = Item { name: "Text".into(), mime: MIME_TEXT.into(), text: Some("hi".into()), ..Default::default() };
        assert!(matches!(outgoing(&[text]).unwrap(), Outgoing::Text { kind: TextKind::Text, .. }));
        let file = Item { name: "a".into(), mime: "x/y".into(), path: Some("/tmp/a".into()), ..Default::default() };
        assert!(matches!(outgoing(&[file.clone(), file]).unwrap(), Outgoing::Files(f) if f.len() == 2));
    }

    #[test]
    fn routes() {
        let mut route = Route::default();
        assert!(route.is_empty());
        route.bt = Some("AA:BB:CC:DD:EE:FF".into());
        assert_eq!(route.mediums(), [Medium::Bt]);
        route.lan = vec!["192.168.1.5:4000".parse().unwrap()];
        assert_eq!(route.mediums(), [Medium::Lan, Medium::Bt]);
        route.bt = None;
        assert_eq!(route.mediums(), [Medium::Lan]);
        assert_eq!(Origin::Bluetooth("AA:BB:CC:DD:EE:FF".into()).to_string(), "Bluetooth AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn device_kinds() {
        assert_eq!(kind(DeviceType::Foldable), DeviceKind::Phone);
        assert_eq!(kind(DeviceType::Laptop), DeviceKind::Laptop);
    }
}
