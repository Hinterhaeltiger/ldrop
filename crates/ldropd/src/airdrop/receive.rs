// SPDX-License-Identifier: GPL-3.0-or-later
//! Receiving (SPEC §6.1, §6.5, §9). While we are shown, a BLE monitor watches for Apple share
//! sheets nearby; one in range brings the Apple link up. While the link is up, an owly-airdrop
//! receiver listens on it and our service is announced over mDNS on it. Each `/Ask` becomes a
//! transfer with the usual prompt. Once hidden, the session stays until its transfers are done.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ldrop_airdrop::addr::RECEIVER_PORT;
use ldrop_airdrop::{ble, model, uti};
use ldrop_ble::{Monitor, Pattern, Range, Seen};
use ldrop_core::paths::resolve_download_dir;
use ldrop_core::storage::{self, sanitize_name};
use ldrop_core::time::now_us;
use ldrop_core::{Direction, Item, MIME_URL, Peer, Protocol, TransferState, format_size};
use ldrop_wifi::owly::Link;
use owly_airdrop::driver::mdns::{Mdns, Registration};
use owly_airdrop::driver::receiver::{Canceller, Options, Receiver, ReceiverEvent, Request};
use owly_airdrop::driver::tls::Identity;
use owly_airdrop::proto::mdns::{Burst, RECEIVER_FLAGS, Service, service_id};
use owly_airdrop::proto::messages::{AskRequest, Offer};
use owly_airdrop::proto::receiver::Config;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Backend;
use super::link::{self, LinkUse};
use crate::consent::{self, Decision};
use crate::prompts::PromptSlot;
use crate::state::Shared;
use crate::transfer::{self, TransferRecord};

/// How long the link stays up after the last share sheet nearby closed, or the last
/// `/Discover`: long enough for someone to pick us.
const WAKE_HOLD: Duration = Duration::from_secs(30);
/// A share sheet counts as gone after this long without its advert.
const LOST_AFTER_SECS: u16 = 10;
/// owly-airdrop answers `/Ask` with a decline after 15 s (ASK-2), so our prompt ends first.
const PROMPT_SECS: u64 = 14;
/// What JPEG 2000 previews (`FileIcon`) are labeled as.
const MIME_JP2: &str = "image/jp2";
/// How long to wait before trying again to listen on a link where that failed.
const RETRY_LISTEN: Duration = Duration::from_secs(5);

pub(super) async fn session(s: Arc<Shared>, ad: Arc<Backend>) {
    let identity = match Identity::generate() {
        Ok(identity) => identity,
        Err(e) => return tracing::error!("AirDrop: cannot make a TLS identity: {e}"),
    };
    let (msgs, mut msgs_rx) = mpsc::unbounded_channel();
    let mut shown_rx = ad.shown.subscribe();
    let mut link_rx = ad.link.subscribe();
    let mut this = Session {
        s,
        ad,
        identity,
        shown: false,
        wake: None,
        nearby: HashSet::new(),
        wake_hold: None,
        keep: None,
        server: None,
        refused: None,
        transfers: HashMap::new(),
        msgs,
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tracing::debug!("AirDrop: receive session started");
    loop {
        let shown = *shown_rx.borrow_and_update();
        if !shown && this.transfers.is_empty() {
            break;
        }
        this.follow_shown(shown).await;
        let link = link_rx.borrow_and_update().clone();
        this.follow_link(link).await;
        this.expire_wake();
        tokio::select! {
            _ = shown_rx.changed() => {}
            _ = link_rx.changed() => {}
            seen = next_seen(&mut this.wake) => this.on_seen(seen),
            event = next_event(&mut this.server) => this.on_event(event).await,
            Some(msg) = msgs_rx.recv() => this.on_msg(msg).await,
            _ = tick.tick() => {}
        }
    }
    this.follow_shown(false).await;
    if let Some(server) = this.server.take() {
        server.stop().await;
    }
    tracing::debug!("AirDrop: receive session ended");
}

struct Session {
    s: Arc<Shared>,
    ad: Arc<Backend>,
    identity: Identity,
    /// What [`Session::follow_shown`] last set up for.
    shown: bool,
    /// Watches for share sheets while shown.
    wake: Option<Monitor>,
    /// Share sheets in range, by BlueZ device path.
    nearby: HashSet<String>,
    /// Holds the link while a share sheet is near, and until the instant after.
    wake_hold: Option<(LinkUse, Instant)>,
    /// Holds the link for as long as we are shown, without the BLE wake.
    keep: Option<LinkUse>,
    server: Option<Server>,
    /// The link the receiver could not be started on, and when, so it is not retried every
    /// second.
    refused: Option<(Arc<Link>, Instant)>,
    /// Requests that became transfers, by owly-airdrop's ID.
    transfers: HashMap<u64, Pending>,
    msgs: mpsc::UnboundedSender<Msg>,
}

/// The receiver on the current link.
struct Server {
    link: Arc<Link>,
    /// What it was started with.
    settings: Settings,
    events: mpsc::Receiver<ReceiverEvent>,
    canceller: Canceller,
    task: JoinHandle<()>,
    /// The mDNS service while shown.
    mdns: Option<Registration>,
}

impl Server {
    /// Stops listening, and waits until the port is free again.
    async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The config a receiver is started with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settings {
    name: String,
    dir: PathBuf,
    max_upload: u64,
}

impl Settings {
    fn now(s: &Shared) -> Settings {
        let name = s.device_name();
        let st = s.state();
        Settings {
            name,
            dir: resolve_download_dir(&st.config.download_dir),
            max_upload: st.config.airdrop.max_upload_bytes,
        }
    }
}

struct Pending {
    id: u32,
    /// To the transfer's [`answer`] task. Dropped once what was sent arrived, which ends it.
    ended: mpsc::UnboundedSender<Ended>,
    /// The offer as prompted, for naming what arrives.
    offered: Vec<Item>,
}

/// How a transfer ended on the sender's side, or the receiver's.
enum Ended {
    Withdrawn,
    Failed(String),
}

/// From an [`answer`] task.
enum Msg {
    /// The prompt ended without the transfer going ahead.
    Done(u64),
    /// Cancelled on this device after it was accepted.
    Cancel(u64),
}

async fn next_seen(wake: &mut Option<Monitor>) -> Option<Seen> {
    match wake {
        Some(monitor) => monitor.next().await,
        None => std::future::pending().await,
    }
}

async fn next_event(server: &mut Option<Server>) -> Option<ReceiverEvent> {
    match server {
        Some(server) => server.events.recv().await,
        None => std::future::pending().await,
    }
}

impl Session {
    async fn follow_shown(&mut self, shown: bool) {
        if shown == self.shown {
            return;
        }
        self.shown = shown;
        if shown {
            let keep = self.s.state().config.airdrop.keep_link_up || self.s.airdrop_interface.is_some();
            if keep {
                self.keep = Some(self.ad.link.hold());
            } else {
                self.wake = start_wake(&self.s).await;
                if self.wake.is_none() {
                    if link::exclusive(&self.s) {
                        tracing::warn!(
                            "AirDrop: without the BLE wake, Apple devices find us only with \
                             `airdrop.keep_link_up = true`, which keeps Wi-Fi off while visible"
                        );
                    } else {
                        tracing::info!("AirDrop: keeping the Apple link up while visible instead");
                        self.keep = Some(self.ad.link.hold());
                    }
                }
            }
            tracing::info!("AirDrop: visible to Apple devices");
        } else {
            self.keep = None;
            self.wake_hold = None;
            self.nearby.clear();
            if let Some(wake) = self.wake.take() {
                wake.stop().await;
            }
            if let Some(server) = &mut self.server {
                server.mdns = None;
            }
        }
    }

    /// Serves on the link while it is up, and announces us there while shown. A new name or
    /// download directory restarts the receiver once nothing is in flight.
    async fn follow_link(&mut self, link: Option<Arc<Link>>) {
        let settings = Settings::now(&self.s);
        let moved =
            self.server.as_ref().is_some_and(|server| !link.as_ref().is_some_and(|l| Arc::ptr_eq(l, &server.link)));
        let changed =
            self.server.as_ref().is_some_and(|server| server.settings != settings) && self.transfers.is_empty();
        if moved || changed {
            if changed {
                tracing::info!("AirDrop: restarting the receiver with the new settings");
            }
            if let Some(server) = self.server.take() {
                server.stop().await;
            }
            if moved {
                self.end_all("the Apple link went down");
            }
        }
        if self.server.is_none()
            && let Some(link) = link
            && !self.refused.as_ref().is_some_and(|(r, at)| Arc::ptr_eq(r, &link) && at.elapsed() < RETRY_LISTEN)
        {
            self.server = self.serve(&link, settings).await;
            self.refused = self.server.is_none().then(|| (link, Instant::now()));
        }
        if let Some(server) = &mut self.server
            && self.shown
            && server.mdns.is_none()
        {
            server.mdns = announce(&server.link).await;
        }
    }

    async fn serve(&self, link: &Arc<Link>, settings: Settings) -> Option<Server> {
        let mut config = Config::new(settings.name.clone());
        config.limits.max_upload_body = settings.max_upload;
        let addr = SocketAddrV6::new(link.address, RECEIVER_PORT, 0, link.index);
        let options = Options::new(config, settings.dir.clone());
        let (receiver, events) = match Receiver::bind(addr, &self.identity, options).await {
            Ok(bound) => bound,
            Err(e) => {
                tracing::error!("AirDrop: cannot listen on [{}%{}]:{RECEIVER_PORT}: {e}", link.address, link.interface);
                return None;
            }
        };
        let canceller = receiver.canceller();
        let task = tokio::spawn(receiver.run());
        tracing::info!("AirDrop: receiving on [{}%{}]:{RECEIVER_PORT}", link.address, link.interface);
        Some(Server { link: link.clone(), settings, events, canceller, task, mdns: None })
    }

    fn on_seen(&mut self, seen: Option<Seen>) {
        match seen {
            Some(Seen::Found(device)) => {
                if self.nearby.is_empty() {
                    tracing::debug!("AirDrop: a share sheet is open nearby");
                }
                self.nearby.insert(device);
                self.hold_wake();
            }
            Some(Seen::Lost(device)) => {
                self.nearby.remove(&device);
                self.hold_wake();
            }
            None => {
                tracing::warn!("AirDrop: the BLE monitor stopped");
                self.wake = None;
            }
        }
    }

    /// Keeps the link up for [`WAKE_HOLD`] from now.
    fn hold_wake(&mut self) {
        if !self.shown {
            return;
        }
        let until = Instant::now() + WAKE_HOLD;
        match &mut self.wake_hold {
            Some((_, t)) => *t = until,
            None => self.wake_hold = Some((self.ad.link.hold(), until)),
        }
    }

    fn expire_wake(&mut self) {
        if !self.nearby.is_empty() {
            self.hold_wake();
        } else if self.wake_hold.as_ref().is_some_and(|(_, until)| Instant::now() >= *until) {
            self.wake_hold = None;
        }
    }

    async fn on_event(&mut self, event: Option<ReceiverEvent>) {
        let Some(event) = event else {
            tracing::warn!("AirDrop: the receiver stopped");
            self.server = None;
            return self.end_all("the receiver stopped");
        };
        match event {
            ReceiverEvent::Discovered { peer, request } => {
                let name = request.sender_computer_name.unwrap_or_default();
                tracing::debug!("AirDrop: {name:?} ({peer}) looked for us");
                self.hold_wake();
            }
            ReceiverEvent::Request(request) => self.on_request(request).await,
            ReceiverEvent::Withdrawn { id } => self.tell(id, Ended::Withdrawn),
            ReceiverEvent::Progress { id, bytes } => {
                if let Some(p) = self.transfers.get(&id) {
                    transfer::update(&self.s, p.id, |t| t.bytes_done = bytes).await;
                }
            }
            ReceiverEvent::Urls { id, urls } => {
                let Some(p) = self.transfers.remove(&id) else { return };
                let items = urls.iter().map(|url| url_item(url, Some(url.clone()))).collect();
                complete(&self.s, p.id, items).await;
            }
            ReceiverEvent::Received { id, paths } => {
                let Some(p) = self.transfers.remove(&id) else { return };
                complete(&self.s, p.id, received_items(&p.offered, &paths)).await;
            }
            ReceiverEvent::Failed { id, reason } => self.tell(id, Ended::Failed(reason)),
        }
    }

    /// Hands the end of a transfer to its [`answer`] task, which knows whether it was prompting.
    fn tell(&self, id: u64, ended: Ended) {
        if let Some(p) = self.transfers.get(&id) {
            let _ = p.ended.send(ended);
        }
    }

    async fn on_request(&mut self, request: Request) {
        let ask = &request.ask;
        tracing::debug!(
            "AirDrop: request from {:?} ({}, {})",
            ask.sender_computer_name,
            ask.sender_model_name,
            request.peer
        );
        if !self.shown {
            tracing::debug!("AirDrop: declining a request while hidden");
            return request.decline();
        }
        let origin = *request.peer.ip();
        let Some(slot) = self.ad.prompts.try_prompt(&origin) else {
            tracing::info!(
                "AirDrop: declining a request from {origin}: too many pending requests, or declined recently"
            );
            return request.decline();
        };
        let offered = items(&ask.offer);
        let mut record =
            TransferRecord::new(Direction::Incoming, TransferState::AwaitingAcceptance, peer(ask), offered.clone());
        record.props.deadline = now_us() + PROMPT_SECS * 1_000_000;
        if let Some(icon) = &ask.file_icon {
            record.props.preview = icon.clone();
            record.props.preview_mime = MIME_JP2.into();
        }
        let cancel = record.cancel.clone();
        let id = match transfer::create(&self.s, record).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("cannot export transfer: {e}");
                return request.decline();
            }
        };
        let (ended, ended_rx) = mpsc::unbounded_channel();
        self.transfers.insert(request.id, Pending { id, ended, offered });
        let ask = Ask { request, id, slot, ended: ended_rx, cancel, hold: self.ad.link.hold() };
        tokio::spawn(answer(self.s.clone(), self.ad.clone(), ask, self.msgs.clone()));
    }

    async fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Done(owly_id) => {
                self.transfers.remove(&owly_id);
            }
            Msg::Cancel(owly_id) => {
                let Some(p) = self.transfers.remove(&owly_id) else { return };
                if let Some(server) = &self.server {
                    server.canceller.cancel(owly_id);
                }
                cancelled(&self.s, p.id, "cancelled on this device").await;
            }
        }
    }

    /// The receiver is gone, and with it every prompt and upload.
    fn end_all(&self, why: &str) {
        for p in self.transfers.values() {
            let _ = p.ended.send(Ended::Failed(why.to_owned()));
        }
    }
}

async fn start_wake(s: &Shared) -> Option<Monitor> {
    let rssi = s.state().config.airdrop.wake_rssi;
    let (start, ad_type, content) = ble::monitor_pattern();
    let range =
        Range { high: rssi, high_timeout: 1, low: rssi.saturating_sub(10).max(-127), low_timeout: LOST_AFTER_SECS };
    match ldrop_ble::monitor(&[Pattern { start, ad_type, content }], range).await {
        Ok(Some(monitor)) => Some(monitor),
        Ok(None) => {
            tracing::info!("AirDrop: no powered Bluetooth adapter that can watch for share sheets");
            None
        }
        Err(e) => {
            tracing::warn!("AirDrop: cannot watch for share sheets over BLE: {e}");
            None
        }
    }
}

/// Publishes `_airdrop._tcp` on the link, under a fresh service ID each time.
async fn announce(link: &Link) -> Option<Registration> {
    let burst = link.announce.map(|a| Burst { count: a.count, interval: a.interval, pause: a.pause });
    let instance = service_id(rand::random());
    let service = Service {
        instance: instance.clone(),
        host: instance,
        port: RECEIVER_PORT,
        addr: link.address,
        flags: RECEIVER_FLAGS,
    };
    match Mdns::new(link.index).with_burst(burst).register(service).await {
        Ok(registration) => Some(registration),
        Err(e) => {
            tracing::error!("AirDrop: cannot announce us on {}: {e}", link.interface);
            None
        }
    }
}

/// A request waiting for its answer, and what the answer needs.
struct Ask {
    request: Request,
    id: u32,
    slot: PromptSlot<Ipv6Addr>,
    ended: mpsc::UnboundedReceiver<Ended>,
    cancel: CancellationToken,
    hold: LinkUse,
}

/// Checks the offer, asks the user, answers the sender, and after an accept, turns a cancel on
/// this device into closing the connection. Holds the link until the transfer is over.
async fn answer(s: Arc<Shared>, ad: Arc<Backend>, ask: Ask, msgs: mpsc::UnboundedSender<Msg>) {
    let Ask { request, id, slot, mut ended, cancel, hold: _hold } = ask;
    let owly_id = request.id;
    let origin = *request.peer.ip();
    let (dir, max) = {
        let st = s.state();
        (resolve_download_dir(&st.config.download_dir), st.config.airdrop.max_upload_bytes)
    };
    let bytes = s.state().transfers.get(&id).map(|r| r.props.bytes_total).unwrap_or(0);
    let refusal = if bytes > max {
        Some(format!("larger than the {} that airdrop.max_upload_bytes allows", format_size(max)))
    } else if !storage::fits(&dir, bytes) {
        Some(format!("not enough free space in {}", dir.display()))
    } else {
        None
    };
    if let Some(error) = refusal {
        request.decline();
        transfer::update(&s, id, |p| {
            p.state = TransferState::Rejected;
            p.error = error;
        })
        .await;
        let _ = msgs.send(Msg::Done(owly_id));
        return;
    }

    // `None` if the prompt ended for another reason.
    let mut why = None;
    let decision = consent::ask(&s, id, async { why = ended.recv().await }).await;
    drop(slot);
    match decision {
        Decision::Accept => {
            if let Err(e) = std::fs::create_dir_all(&dir) {
                request.decline();
                let _ = msgs.send(Msg::Done(owly_id));
                return fail(&s, id, format!("cannot write to {}: {e}", dir.display())).await;
            }
            request.accept();
            transfer::update(&s, id, |p| {
                p.state = TransferState::Transferring;
                p.deadline = 0;
            })
            .await;
            tokio::select! {
                end = ended.recv() => {
                    match end {
                        Some(Ended::Failed(error)) => fail(&s, id, plain_failure(error)).await,
                        // The sender gave up just as the user accepted.
                        Some(Ended::Withdrawn) => cancelled(&s, id, "cancelled by the sender").await,
                        // It arrived.
                        None => return,
                    }
                    let _ = msgs.send(Msg::Done(owly_id));
                }
                () = cancel.cancelled() => {
                    let _ = msgs.send(Msg::Cancel(owly_id));
                }
            }
        }
        Decision::Reject | Decision::Timeout => {
            ad.prompts.cool_down(&origin);
            request.decline();
            let error = if decision == Decision::Timeout { "no answer before the deadline" } else { "" };
            transfer::update(&s, id, |p| {
                p.state = TransferState::Rejected;
                p.error = error.into();
            })
            .await;
            let _ = msgs.send(Msg::Done(owly_id));
        }
        Decision::Cancelled => {
            request.decline();
            match why {
                Some(Ended::Failed(error)) => fail(&s, id, error).await,
                _ if cancel.is_cancelled() => cancelled(&s, id, "cancelled on this device").await,
                _ => cancelled(&s, id, "cancelled by the sender").await,
            }
            let _ = msgs.send(Msg::Done(owly_id));
        }
    }
}

/// An upload's failure as owly-airdrop words it, in words for the user where it is the common
/// case: the sender went away.
fn plain_failure(error: String) -> String {
    if error.contains("close_notify") || error.contains("connection reset") {
        "the sender closed the connection".to_owned()
    } else {
        error
    }
}

fn peer(ask: &AskRequest) -> Peer {
    let model = ask.sender_model_name.as_str();
    let name = ask.sender_computer_name.trim();
    let name = if name.is_empty() {
        match model::kind(model) {
            ldrop_core::DeviceKind::Phone => "iPhone",
            ldrop_core::DeviceKind::Tablet => "iPad",
            _ => "Apple device",
        }
    } else {
        name
    };
    Peer {
        name: name.to_owned(),
        platform: model::platform(model),
        kind: model::kind(model),
        protocol: Protocol::AirDrop,
        vendor: None,
    }
}

/// Prompt items: files and folders as the sender names them, or the links.
fn items(offer: &Offer) -> Vec<Item> {
    match offer {
        Offer::Files(files) => files
            .iter()
            .map(|f| Item {
                name: sanitize_name(&f.name),
                mime: if f.is_directory {
                    uti::MIME_DIRECTORY.to_owned()
                } else {
                    uti::to_mime(&f.file_type).map_or_else(|| guess_mime(Path::new(&f.name)), str::to_owned)
                },
                size: f.size.unwrap_or(0),
                path: None,
                text: None,
            })
            .collect(),
        // Marked as text until accepted (see `Item::text`).
        Offer::Urls(urls) => urls.iter().map(|url| url_item(url, Some(String::new()))).collect(),
    }
}

fn url_item(url: &str, text: Option<String>) -> Item {
    Item { name: url.to_owned(), mime: MIME_URL.to_owned(), size: url.len() as u64, path: None, text }
}

fn guess_mime(path: &Path) -> String {
    mime_guess::from_path(path).first_or_octet_stream().essence_str().to_owned()
}

/// What arrived, as items: owly-airdrop moved each top-level entry into the download dir,
/// renamed on collisions.
fn received_items(offered: &[Item], paths: &[PathBuf]) -> Vec<Item> {
    paths
        .iter()
        .map(|path| {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let is_dir = path.symlink_metadata().is_ok_and(|m| m.is_dir());
            let mime = if is_dir {
                uti::MIME_DIRECTORY.to_owned()
            } else {
                offered.iter().find(|o| o.name == name).map_or_else(|| guess_mime(path), |o| o.mime.clone())
            };
            Item { name, mime, size: disk_size(path), path: Some(path.display().to_string()), text: None }
        })
        .collect()
}

/// Bytes in a file, or in the files under a directory. Symlinks are not followed.
fn disk_size(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map(|entries| entries.flatten().map(|e| disk_size(&e.path())).sum()).unwrap_or(0)
}

async fn complete(s: &Arc<Shared>, id: u32, items: Vec<Item>) {
    let total: u64 = items.iter().map(|i| i.size).sum();
    transfer::update(s, id, |p| {
        p.items = items;
        p.bytes_total = total;
        p.bytes_done = total;
        p.state = TransferState::Completed;
    })
    .await;
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

#[cfg(test)]
mod tests {
    use owly_airdrop::proto::messages::AskFile;

    use super::*;

    fn file(name: &str, uti: &str, size: Option<u64>, is_directory: bool) -> AskFile {
        AskFile { name: name.into(), file_type: uti.into(), size, bom_path: format!("./{name}"), is_directory }
    }

    #[test]
    fn prompt_items() {
        let offer = Offer::Files(vec![
            file("../IMG_0001.HEIC", "public.heic", Some(2_000_000), false),
            file("notes.weird", "com.example.unknown", None, false),
            file("Trip", "public.folder", Some(10), true),
        ]);
        let items = items(&offer);
        let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["IMG_0001.HEIC", "notes.weird", "Trip"]);
        assert_eq!(items[0].mime, "image/heic");
        assert_eq!(items[0].size, 2_000_000);
        assert_eq!(items[1].mime, "application/octet-stream");
        assert_eq!(items[1].size, 0, "size unknown in the legacy shape");
        assert_eq!(items[2].mime, uti::MIME_DIRECTORY);

        let links = items_of_urls(&["https://example.com/a"]);
        assert_eq!(links[0].mime, MIME_URL);
        assert_eq!(links[0].text.as_deref(), Some(""), "marked as text before it is accepted");
    }

    fn items_of_urls(urls: &[&str]) -> Vec<Item> {
        items(&Offer::Urls(urls.iter().map(|u| u.to_string()).collect()))
    }

    #[test]
    fn peers() {
        let mut ask = AskRequest {
            transfer_id: None,
            transfer_type: None,
            sender_computer_name: "  ".into(),
            sender_model_name: "iPhone15,2".into(),
            sender_id: String::new(),
            bundle_id: String::new(),
            sender_record: None,
            file_icon: None,
            offer: Offer::Urls(Vec::new()),
            described: String::new(),
        };
        let p = peer(&ask);
        assert_eq!((p.name.as_str(), p.kind, p.protocol), ("iPhone", ldrop_core::DeviceKind::Phone, Protocol::AirDrop));
        ask.sender_computer_name = "Anna's MacBook".into();
        ask.sender_model_name = "Linux".into();
        let p = peer(&ask);
        assert_eq!((p.name.as_str(), p.platform), ("Anna's MacBook", ldrop_core::Platform::Linux));
    }

    #[test]
    fn received_files_and_folders() {
        let dir = tempfile::tempdir().unwrap();
        let photo = dir.path().join("IMG (1).jpg");
        std::fs::write(&photo, b"jpeg").unwrap();
        let folder = dir.path().join("Trip");
        std::fs::create_dir_all(folder.join("day 1")).unwrap();
        std::fs::write(folder.join("a.txt"), b"12345").unwrap();
        std::fs::write(folder.join("day 1/b.txt"), b"123").unwrap();
        let offered = vec![Item { name: "Trip".into(), mime: uti::MIME_DIRECTORY.into(), ..Default::default() }];

        let items = received_items(&offered, &[photo.clone(), folder]);
        assert_eq!(items[0].name, "IMG (1).jpg");
        assert_eq!(items[0].mime, "image/jpeg", "renamed on a collision, so guessed from the name");
        assert_eq!(items[0].size, 4);
        assert_eq!(items[0].path.as_deref(), Some(photo.to_str().unwrap()));
        assert_eq!(items[1].mime, uti::MIME_DIRECTORY);
        assert_eq!(items[1].size, 8);
    }
}
