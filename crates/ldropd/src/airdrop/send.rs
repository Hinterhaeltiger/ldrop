// SPDX-License-Identifier: GPL-3.0-or-later
//! Sending (SPEC §6.1, §8). While a discovery session runs, the Apple link is up and our BLE
//! advert wakes Apple devices nearby that are in Everyone mode. Receivers are found by mDNS on
//! the link and, since an iPhone's mDNS does not reach a Linux `awdl0`, by `/Discover` at each
//! AWDL peer owlyd hears (Owly DISC-5). One that answers with its name is a device. A send holds
//! the link and the advert until it is over.

use std::collections::{HashMap, HashSet};
use std::io;
use std::net::{Ipv6Addr, SocketAddrV6};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ldrop_airdrop::addr::{APPLE_PORT, link_local, parse_mac};
use ldrop_airdrop::ble::APPLE_COMPANY_ID;
use ldrop_airdrop::model;
use ldrop_core::{Device, Direction, Item, Medium, Peer, Protocol, TransferState};
use ldrop_wifi::owly::{Link, Owly};
use owly_airdrop::driver::mdns::{Browse, Mdns};
use owly_airdrop::driver::sender::{Event, Payload, Sender};
use owly_airdrop::proto::ble::Advert;
use owly_airdrop::proto::mdns::{BrowseEvent, RECEIVER_FLAGS, SUPPORTS_DVZIP, service_id};
use owly_airdrop::proto::sender::{Config, Outcome, Receiver};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Backend;
use super::link::LinkUse;
use crate::discovery;
use crate::error::{Error, Result};
use crate::state::Shared;
use crate::transfer::{self, TransferRecord};

const DEVICE_PREFIX: &str = "ad-";
/// How often the AWDL peers that are not listed yet get a `/Discover`: an iPhone opens its port
/// only some seconds after our advert woke it.
const KNOCK_EVERY: Duration = Duration::from_millis(500);
/// A peer that is silent must not hold up the others for long.
const KNOCK_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a listed receiver is asked again whether it still lists us.
const RECHECK: Duration = Duration::from_secs(5);
/// A receiver that has not answered for this long is gone.
const LOST_AFTER: Duration = Duration::from_secs(30);
/// How long a send waits for the Apple link to come up.
const LINK_WAIT: Duration = Duration::from_secs(30);
/// What text that is not a link is sent as: AirDrop carries only files and links.
const TEXT_FILE: &str = "Text.txt";

/// Where a receiver listens. The scope is the link's at the time of a send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Route {
    addr: Ipv6Addr,
    port: u16,
    /// Its TXT `flags`, which say how it takes an upload.
    flags: u32,
}

impl Route {
    /// An AWDL peer with no mDNS: Apple's port, and DVZip, which every Apple receiver takes
    /// (Owly UP-1b).
    fn peer(addr: Ipv6Addr) -> Route {
        Route { addr, port: APPLE_PORT, flags: RECEIVER_FLAGS | SUPPORTS_DVZIP }
    }
}

pub fn is_airdrop_device(device: &Device) -> bool {
    device.id.starts_with(DEVICE_PREFIX)
}

/// Discovery while any session wants it.
pub(super) struct DiscoveryRun {
    token: CancellationToken,
    task: JoinHandle<()>,
}

impl DiscoveryRun {
    pub(super) fn start(s: &Arc<Shared>, ad: &Arc<Backend>) -> DiscoveryRun {
        let token = CancellationToken::new();
        let task = tokio::spawn(discover(s.clone(), ad.clone(), token.clone()));
        DiscoveryRun { token, task }
    }

    pub(super) async fn stop(self) {
        self.token.cancel();
        let _ = self.task.await;
    }
}

async fn discover(s: Arc<Shared>, ad: Arc<Backend>, token: CancellationToken) {
    // `/Discover` carries no name, so one sender serves the whole session.
    let sender = match Sender::new(Config::new(s.device_name(), service_id(rand::random()))) {
        Ok(sender) => Arc::new(sender),
        Err(e) => {
            tracing::warn!("AirDrop: cannot look for Apple devices: {e}");
            return;
        }
    };
    tracing::debug!("AirDrop: discovery started");
    let _hold = ad.link.hold();
    let _advert = advertise(&s).await;
    let mut link_rx = ad.link.subscribe();
    loop {
        let link = link_rx.borrow_and_update().clone();
        let finding = async {
            if let Some(link) = link {
                Finder::new(&s, &ad, link, sender.clone()).run().await;
            }
            std::future::pending::<()>().await
        };
        tokio::select! {
            () = token.cancelled() => break,
            changed = link_rx.changed() => if changed.is_err() { break },
            () = finding => {}
        }
        forget_all(&s, &ad).await;
    }
    forget_all(&s, &ad).await;
    tracing::debug!("AirDrop: discovery stopped");
}

async fn forget_all(s: &Arc<Shared>, ad: &Backend) {
    let ids: Vec<String> = ad.receivers.lock().unwrap_or_else(|e| e.into_inner()).drain().map(|(id, _)| id).collect();
    for id in ids {
        discovery::device_lost(s, &id).await;
    }
}

/// The advert that makes Apple devices in Everyone mode turn AirDrop on, under a fresh tag
/// (Owly BLE-1, BLE-5). Not with `--airdrop-interface`, whose tests must not reach the radio.
async fn advertise(s: &Shared) -> Option<ldrop_ble::Advertisement> {
    if s.airdrop_interface.is_some() {
        return None;
    }
    let data = Advert::anonymous(rand::random()).manufacturer_data();
    match ldrop_ble::advertise_manufacturer_data(APPLE_COMPANY_ID, data).await {
        Ok(Some(advert)) => Some(advert),
        Ok(None) => {
            tracing::info!(
                "AirDrop: no powered Bluetooth adapter; Apple devices show up only while AirDrop is \
                 already active on them"
            );
            None
        }
        Err(e) => {
            tracing::warn!("AirDrop: cannot advertise over BLE: {e}");
            None
        }
    }
}

/// Where AWDL peers come from.
enum Peers {
    Owly(Owly),
    /// From `--airdrop-peer`.
    Fixed(Vec<[u8; 6]>),
    None,
}

impl Peers {
    async fn new(s: &Shared, ad: &Backend) -> Peers {
        if !ad.peers.is_empty() {
            return Peers::Fixed(ad.peers.clone());
        }
        if s.airdrop_interface.is_some() {
            return Peers::None;
        }
        match Owly::system().await {
            Ok(owly) => Peers::Owly(owly),
            Err(e) => {
                tracing::warn!("AirDrop: cannot list AWDL peers, so iPhones may not show up: {e}");
                Peers::None
            }
        }
    }

    /// Their link-local addresses, or `None` when that is not known right now.
    async fn addrs(&self) -> Option<HashSet<Ipv6Addr>> {
        let macs = match self {
            Peers::Owly(owly) => match owly.peers().await {
                Ok(peers) => peers.iter().filter_map(|p| parse_mac(&p.mac)).collect(),
                Err(e) => {
                    tracing::debug!("AirDrop: cannot list AWDL peers: {e}");
                    return None;
                }
            },
            Peers::Fixed(macs) => macs.clone(),
            Peers::None => return None,
        };
        Some(macs.into_iter().map(link_local).collect())
    }
}

/// A receiver that answered with its name.
struct Listed {
    id: String,
    name: String,
    route: Route,
    /// The mDNS instance it was announced as. `None` for an AWDL peer that was knocked on.
    instance: Option<String>,
    answered: Instant,
}

/// What a `/Discover` was sent to, and its answer: `Ok(None)` when the receiver does not list
/// us (Owly DISC-1).
type Answer = (Route, Option<String>, io::Result<Option<Receiver>>);

/// Finds receivers on one link until it goes.
struct Finder<'a> {
    s: &'a Arc<Shared>,
    ad: &'a Backend,
    link: Arc<Link>,
    sender: Arc<Sender>,
    /// What mDNS announces, by instance: asked until it answers, since a receiver may announce
    /// itself before it listens.
    announced: HashMap<String, Route>,
    listed: HashMap<Ipv6Addr, Listed>,
    /// Receivers that answered but do not list us, such as Apple devices in Contacts Only
    /// mode, and when: asked again only after [`RECHECK`].
    unlisted: HashMap<Ipv6Addr, Instant>,
    /// Addresses with a `/Discover` on the way.
    asking: HashSet<Ipv6Addr>,
    answers: JoinSet<Answer>,
}

impl<'a> Finder<'a> {
    fn new(s: &'a Arc<Shared>, ad: &'a Backend, link: Arc<Link>, sender: Arc<Sender>) -> Self {
        Finder {
            s,
            ad,
            link,
            sender,
            announced: HashMap::new(),
            listed: HashMap::new(),
            unlisted: HashMap::new(),
            asking: HashSet::new(),
            answers: JoinSet::new(),
        }
    }

    async fn run(mut self) {
        let mut browse = match Mdns::new(self.link.index).browse().await {
            Ok(browse) => Some(browse),
            Err(e) => {
                tracing::warn!("AirDrop: cannot browse mDNS on {}: {e}", self.link.interface);
                None
            }
        };
        let peers = Peers::new(self.s, self.ad).await;
        let mut tick = tokio::time::interval(KNOCK_EVERY);
        loop {
            tokio::select! {
                event = next_browse(&mut browse) => match event {
                    Some(BrowseEvent::Found(found)) if found.addr != self.link.address => {
                        let route = Route { addr: found.addr, port: found.port, flags: found.flags };
                        self.announced.insert(found.instance.clone(), route);
                        self.ask(route, Some(found.instance));
                    }
                    Some(BrowseEvent::Found(_)) => {}
                    Some(BrowseEvent::Lost(instance)) => {
                        self.announced.remove(&instance);
                        let addr = self.listed.iter().find(|(_, l)| l.instance.as_ref() == Some(&instance)).map(|(a, _)| *a);
                        if let Some(addr) = addr {
                            self.lose(addr).await;
                        }
                    }
                    None => browse = None,
                },
                Some(answer) = self.answers.join_next() => {
                    if let Ok((route, instance, result)) = answer {
                        self.asking.remove(&route.addr);
                        self.on_answer(route, instance, result).await;
                    }
                }
                _ = tick.tick() => {
                    let peers = peers.addrs().await;
                    self.on_tick(peers.as_ref()).await;
                }
            }
        }
    }

    /// Whether `addr` is neither listed nor known not to list us.
    fn unasked(&self, addr: Ipv6Addr) -> bool {
        !self.listed.contains_key(&addr) && !self.unlisted.contains_key(&addr)
    }

    /// Sends `route` a `/Discover`, unless one is on the way.
    fn ask(&mut self, route: Route, instance: Option<String>) {
        if !self.asking.insert(route.addr) {
            return;
        }
        let sender = self.sender.clone();
        let addr = SocketAddrV6::new(route.addr, route.port, 0, self.link.index);
        self.answers.spawn(async move {
            let answer = tokio::time::timeout(KNOCK_TIMEOUT, sender.discover(addr))
                .await
                .unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()));
            (route, instance, answer)
        });
    }

    /// Asks whoever is not listed yet, and the listed again now and then. `peers` are the AWDL
    /// peers' addresses when known.
    async fn on_tick(&mut self, peers: Option<&HashSet<Ipv6Addr>>) {
        let now = Instant::now();
        self.unlisted.retain(|_, at| now - *at < RECHECK);
        let announced: Vec<_> = self.announced.iter().map(|(instance, route)| (*route, instance.clone())).collect();
        for (route, instance) in announced {
            if self.unasked(route.addr) {
                self.ask(route, Some(instance));
            }
        }
        for &addr in peers.into_iter().flatten() {
            if addr != self.link.address && self.unasked(addr) {
                self.ask(Route::peer(addr), None);
            }
        }
        // A knocked-on receiver whose radio went quiet is gone; owlyd stops listing it soon after.
        if let Some(peers) = peers {
            let gone: Vec<_> = self
                .listed
                .values()
                .filter(|l| l.instance.is_none() && !peers.contains(&l.route.addr))
                .map(|l| l.route.addr)
                .collect();
            for addr in gone {
                self.lose(addr).await;
            }
        }
        let stale: Vec<_> = self
            .listed
            .values()
            .filter(|l| now - l.answered >= RECHECK)
            .map(|l| (l.route, l.instance.clone(), now - l.answered >= LOST_AFTER))
            .collect();
        for (route, instance, lost) in stale {
            if lost {
                self.lose(route.addr).await;
            } else {
                self.ask(route, instance);
            }
        }
    }

    async fn on_answer(&mut self, route: Route, instance: Option<String>, result: io::Result<Option<Receiver>>) {
        // Not there, or not yet: the next round asks again.
        let Ok(answer) = result else { return };
        let Some(receiver) = answer.filter(|r| !model::display_name(&r.computer_name).is_empty()) else {
            self.unlisted.insert(route.addr, Instant::now());
            if self.listed.contains_key(&route.addr) {
                tracing::debug!("AirDrop: {} no longer lists us", route.addr);
                self.lose(route.addr).await;
            }
            return;
        };
        let name = model::display_name(&receiver.computer_name);
        let model = receiver.model_name.unwrap_or_default();
        let now = Instant::now();
        let listed = match self.listed.get_mut(&route.addr) {
            Some(listed) => {
                listed.answered = now;
                // mDNS knows the port; a knock at an AWDL peer guesses it.
                if instance.is_some() {
                    listed.route = route;
                    listed.instance = instance;
                }
                if listed.name == name {
                    self.remember(route.addr);
                    return;
                }
                listed.name = name;
                listed
            }
            None => {
                let id = format!("{DEVICE_PREFIX}{:08x}", rand::random::<u32>());
                tracing::debug!("AirDrop: found {name:?} ({model}) at {} as {id}", route.addr);
                self.listed.entry(route.addr).or_insert(Listed { id, name, route, instance, answered: now })
            }
        };
        let device = Device {
            id: listed.id.clone(),
            name: listed.name.clone(),
            kind: model::kind(&model),
            platform: model::platform(&model),
            protocol: Protocol::AirDrop,
            mediums: vec![Medium::Awdl],
            vendor: None,
        };
        self.remember(route.addr);
        discovery::device_found(self.s, device).await;
    }

    /// Keeps the route to a listed receiver where a send finds it.
    fn remember(&self, addr: Ipv6Addr) {
        if let Some(listed) = self.listed.get(&addr) {
            self.ad.receivers.lock().unwrap_or_else(|e| e.into_inner()).insert(listed.id.clone(), listed.route);
        }
    }

    async fn lose(&mut self, addr: Ipv6Addr) {
        let Some(listed) = self.listed.remove(&addr) else { return };
        self.ad.receivers.lock().unwrap_or_else(|e| e.into_inner()).remove(&listed.id);
        discovery::device_lost(self.s, &listed.id).await;
    }
}

async fn next_browse(browse: &mut Option<Browse>) -> Option<BrowseEvent> {
    match browse {
        Some(browse) => browse.events.recv().await,
        None => std::future::pending().await,
    }
}

/// What a send carries.
enum Outgoing {
    Payload(Payload),
    /// Text that is not a link, which goes as a file.
    Text(String),
}

fn outgoing(items: &[Item]) -> Result<Outgoing> {
    if let [item] = items
        && let Some(text) = &item.text
    {
        return Ok(if item.is_url() {
            Outgoing::Payload(Payload::Urls(vec![text.clone()]))
        } else {
            Outgoing::Text(text.clone())
        });
    }
    let paths = items
        .iter()
        .map(|i| {
            i.path
                .as_ref()
                .map(PathBuf::from)
                .ok_or_else(|| Error::NotSupported("text and files cannot be mixed".into()))
        })
        .collect::<Result<_>>()?;
    Ok(Outgoing::Payload(Payload::Files(paths)))
}

pub async fn start_outgoing(s: &Arc<Shared>, device: &Device, items: Vec<Item>) -> Result<u32> {
    let ad = super::backend(s).ok_or_else(|| Error::Unavailable("AirDrop is not running".into()))?;
    let route = ad
        .receivers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&device.id)
        .copied()
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
    // Held from here: the share sheet may stop discovering as soon as this returns.
    let hold = ad.link.hold();
    tokio::spawn(run_outgoing(s.clone(), ad, id, route, outgoing, cancel, hold));
    Ok(id)
}

async fn run_outgoing(
    s: Arc<Shared>,
    ad: Arc<Backend>,
    id: u32,
    route: Route,
    outgoing: Outgoing,
    cancel: CancellationToken,
    _hold: LinkUse,
) {
    // Keeps the receiver's AirDrop awake while its user decides.
    let _advert = advertise(&s).await;
    let outcome = tokio::select! {
        outcome = deliver(&s, &ad, id, route, outgoing) => outcome,
        () = cancel.cancelled() => return end(&s, id, TransferState::Cancelled, "cancelled on this device".into()).await,
    };
    match outcome {
        Ok(Outcome::Delivered) => {
            transfer::update(&s, id, |p| {
                p.bytes_done = p.bytes_total;
                p.state = TransferState::Completed;
            })
            .await
        }
        Ok(Outcome::Declined) => end(&s, id, TransferState::Rejected, String::new()).await,
        Ok(Outcome::Failed(error)) | Err(error) => {
            tracing::warn!("transfer {id} failed: {error}");
            end(&s, id, TransferState::Failed, error).await
        }
    }
}

async fn end(s: &Arc<Shared>, id: u32, state: TransferState, error: String) {
    transfer::update(s, id, |p| {
        p.state = state;
        p.error = error;
    })
    .await;
}

/// Asks the receiver to take `outgoing`, and uploads it if it does.
async fn deliver(
    s: &Arc<Shared>,
    ad: &Backend,
    id: u32,
    route: Route,
    outgoing: Outgoing,
) -> std::result::Result<Outcome, String> {
    let link = wait_for_link(ad).await.ok_or("the Apple link did not come up")?;
    let spool = super::spool_dir(s);
    std::fs::create_dir_all(&spool).map_err(|e| format!("cannot write to {}: {e}", spool.display()))?;
    // The text's directory goes once the send is over.
    let (payload, _text) = match outgoing {
        Outgoing::Payload(payload) => (payload, None),
        Outgoing::Text(text) => {
            let dir = tempfile::Builder::new().prefix("text-").tempdir_in(&spool).map_err(|e| e.to_string())?;
            let path = dir.path().join(TEXT_FILE);
            std::fs::write(&path, text).map_err(|e| e.to_string())?;
            (Payload::Files(vec![path]), Some(dir))
        }
    };
    let config = Config::new(s.device_name(), service_id(rand::random()));
    let sender = Sender::new(config).map_err(|e| e.to_string())?.spool_in(&spool);
    let addr = SocketAddrV6::new(route.addr, route.port, 0, link.index);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let events = async {
        while let Some(event) = rx.recv().await {
            match event {
                Event::Asking => transfer::update(s, id, |p| p.state = TransferState::AwaitingAcceptance).await,
                Event::Accepted => transfer::update(s, id, |p| p.state = TransferState::Transferring).await,
                // In bytes of the compressed archive, so scaled to the files'.
                Event::Progress(p) => {
                    transfer::update(s, id, |t| t.bytes_done = scale(t.bytes_total, p.sent, p.total)).await
                }
            }
        }
    };
    let (outcome, ()) = tokio::join!(sender.send(addr, &payload, route.flags, Some(tx)), events);
    outcome.map_err(|e| e.to_string())
}

fn scale(total: u64, sent: u64, of: u64) -> u64 {
    if of == 0 { 0 } else { (u128::from(total) * u128::from(sent.min(of)) / u128::from(of)) as u64 }
}

async fn wait_for_link(ad: &Backend) -> Option<Arc<Link>> {
    let mut rx = ad.link.subscribe();
    let up = tokio::time::timeout(LINK_WAIT, rx.wait_for(Option::is_some)).await.ok()?.ok()?;
    up.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_goes() {
        let url = Item {
            name: "https://example.com".into(),
            mime: ldrop_core::MIME_URL.into(),
            text: Some("https://example.com".into()),
            ..Default::default()
        };
        assert!(matches!(outgoing(&[url]), Ok(Outgoing::Payload(Payload::Urls(u))) if u == ["https://example.com"]));
        let text =
            Item { name: "Text".into(), mime: "text/plain".into(), text: Some("hi".into()), ..Default::default() };
        assert!(matches!(outgoing(std::slice::from_ref(&text)), Ok(Outgoing::Text(t)) if t == "hi"));
        let file = Item { name: "a".into(), mime: "x/y".into(), path: Some("/tmp/a".into()), ..Default::default() };
        assert!(
            matches!(outgoing(std::slice::from_ref(&file)), Ok(Outgoing::Payload(Payload::Files(p))) if p == [PathBuf::from("/tmp/a")])
        );
        assert!(matches!(outgoing(&[file, text]), Err(Error::NotSupported(_))));
    }

    #[test]
    fn progress_scales_to_the_files() {
        assert_eq!(scale(1000, 50, 100), 500);
        assert_eq!(scale(1000, 150, 100), 1000);
        assert_eq!(scale(1000, 0, 0), 0);
        assert_eq!(scale(u64::MAX, u64::MAX / 2, u64::MAX), u64::MAX / 2);
    }
}
