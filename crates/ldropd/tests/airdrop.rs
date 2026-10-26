// SPDX-License-Identifier: GPL-3.0-or-later
//! AirDrop end to end: `ldropd --airdrop-interface owb` on one end of a veth pair, and
//! owly-airdrop's sender or receiver, standing in for an iPhone, on the other. Each test runs
//! again in a network namespace of its own (`unshare --user --map-root-user --net`), the way
//! Owly's tests do, so it needs no root and touches no real network. Skipped when user
//! namespaces, `ip` or `dbus-daemon` are not available.

use std::io::{BufRead, BufReader};
use std::net::{Ipv6Addr, SocketAddrV6};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ldrop_airdrop::addr::{APPLE_PORT, RECEIVER_PORT, link_local};
use ldrop_client::{Client, Decision, DiscoveryEvent, DiscoverySession, PendingRequest, TransferInfo};
use ldrop_core::dbus::BUS_NAME;
use ldrop_core::paths::Paths;
use ldrop_core::{Device, DeviceKind, Direction, MIME_URL, Medium, Platform, Protocol, TransferState, Visibility};
use owly_airdrop::driver::mdns::{Mdns, Registration};
use owly_airdrop::driver::receiver::{self, Receiver, ReceiverEvent, Request};
use owly_airdrop::driver::sender::{Event, Payload, Sender};
use owly_airdrop::driver::tls::Identity;
use owly_airdrop::proto::mdns::{BrowseEvent, RECEIVER_FLAGS, Service, service_id};
use owly_airdrop::proto::messages::Offer;
use owly_airdrop::proto::sender::{Config, Outcome};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::{Connection, OwnedObjectPath};

const ENV: &str = "LDROP_TEST_NETNS";
const WAIT: Duration = Duration::from_secs(15);

const BUS_CONFIG: &str = r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir=@DIR@</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#;

/// Defines a test that runs in a namespace of its own on a fresh veth pair.
macro_rules! netns_test {
    (async fn $name:ident($veth:ident: Veth) $body:block) => {
        #[test]
        fn $name() {
            if let Some($veth) = enter(stringify!($name)) {
                tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap().block_on(async move $body)
            }
        }
    };
}

#[derive(Debug, Clone)]
struct Iface {
    name: &'static str,
    index: u32,
    addr: Ipv6Addr,
}

/// `a` is the sender's end, `b` ldropd's.
#[derive(Debug, Clone)]
struct Veth {
    a: Iface,
    b: Iface,
}

impl Veth {
    /// How the sender, on `a`, reaches ldropd's receiver on `b`.
    fn receiver(&self) -> SocketAddrV6 {
        SocketAddrV6::new(self.b.addr, RECEIVER_PORT, 0, self.a.index)
    }
}

/// In the test process, runs the test again inside a namespace and checks that it passed,
/// returning `None`. Inside the namespace, sets up the veth pair.
fn enter(test: &str) -> Option<Veth> {
    if std::env::var_os(ENV).is_some() {
        // `LDROP_LOG=debug` shows the daemon's side.
        let filter = tracing_subscriber::EnvFilter::try_from_env("LDROP_LOG").unwrap_or_else(|_| "warn".into());
        let _ = tracing_subscriber::fmt().with_env_filter(filter).with_test_writer().try_init();
        // owly-airdrop's TLS, for a phone that starts before the daemon.
        let _ = rustls::crypto::ring::default_provider().install_default();
        return Some(setup());
    }
    let works = |cmd: &mut Command| cmd.stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success());
    if !works(Command::new("unshare").args(["--user", "--map-root-user", "--net", "true"])) {
        eprintln!("skipping {test}: cannot make a user and network namespace (unshare)");
        return None;
    }
    if !works(Command::new("ip").arg("-V")) || !works(Command::new("dbus-daemon").arg("--version")) {
        eprintln!("skipping {test}: needs ip (iproute2) and dbus-daemon");
        return None;
    }
    let status = Command::new("unshare")
        .args(["--user", "--map-root-user", "--net", "--"])
        .arg(std::env::current_exe().unwrap())
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(ENV, "1")
        .status()
        .unwrap();
    assert!(status.success(), "{test} failed in its network namespace");
    None
}

fn ip(args: &[&str]) {
    let status = Command::new("ip").args(args).status().unwrap();
    assert!(status.success(), "ip {args:?} failed");
}

fn setup() -> Veth {
    ip(&["link", "set", "lo", "up"]);
    ip(&["link", "add", "owa", "type", "veth", "peer", "name", "owb"]);
    let ends = [("owa", [0x02, 0, 0, 0, 0, 0x0a]), ("owb", [0x02, 0, 0, 0, 0, 0x0b])];
    for (name, mac) in ends {
        // Addresses are usable at once, without a second of duplicate address detection.
        std::fs::write(format!("/proc/sys/net/ipv6/conf/{name}/accept_dad"), "0").unwrap();
        ip(&["link", "set", name, "address", &mac.map(|b| format!("{b:02x}")).join(":")]);
    }
    for (name, _) in ends {
        ip(&["link", "set", name, "up"]);
    }
    let [a, b] = ends.map(|(name, mac)| {
        let addr = link_local(mac);
        wait_for_address(name, addr);
        Iface { name, index: nix::net::if_::if_nametoindex(name).unwrap(), addr }
    });
    Veth { a, b }
}

fn wait_for_address(name: &str, addr: Ipv6Addr) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = Command::new("ip").args(["-6", "addr", "show", "dev", name]).output().unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        if text.contains(&format!("{addr}/64")) && !text.contains("tentative") {
            return;
        }
        assert!(Instant::now() < deadline, "{name} never got {addr}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// ldropd on a private bus, with `owb` as its Apple link and an agent channel for its prompts.
struct Daemon {
    bus: Child,
    client: Client,
    requests: mpsc::Receiver<PendingRequest>,
    downloads: PathBuf,
    /// Where it builds what it sends.
    spool: PathBuf,
    _registration: ldrop_client::AgentRegistration,
    _daemon: Connection,
    _root: TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.bus.kill();
        let _ = self.bus.wait();
    }
}

impl Daemon {
    /// Visible to Everyone, for tests of receiving.
    async fn start(veth: &Veth, config: &str) -> Daemon {
        let d = Daemon::launch(veth, config, &[]).await;
        d.client.set_visibility(Visibility::Everyone, Duration::from_secs(600)).await.unwrap();
        d
    }

    /// Hidden, which is all sending needs. Discovery finds receivers over mDNS, and by knocking
    /// at `peers`, MAC addresses that stand in for the AWDL peers owlyd hears.
    async fn launch(veth: &Veth, config: &str, peers: &[&str]) -> Daemon {
        let root = tempfile::tempdir().unwrap();
        let downloads = root.path().join("Downloads");
        let paths = Paths::under(&root.path().join("config"), &root.path().join("state"));
        std::fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        let config = format!(
            "device_name = \"Test Laptop\"\ndownload_dir = {:?}\n\n[quickshare]\nenabled = false\n\n{config}",
            downloads.display().to_string()
        );
        std::fs::write(&paths.config_file, config).unwrap();
        let bus_config = root.path().join("bus.conf");
        std::fs::write(&bus_config, BUS_CONFIG.replace("@DIR@", &root.path().display().to_string())).unwrap();
        let mut bus = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", bus_config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();

        let daemon = connect(address.trim()).await;
        let opts = ldropd::Options {
            paths,
            sim: false,
            probe_hardware: false,
            network: true,
            airdrop_interface: Some(veth.b.name.to_owned()),
            airdrop_peers: peers.iter().map(|&p| p.to_owned()).collect(),
        };
        let spool = opts.paths.state_dir.join("spool");
        ldropd::start(&daemon, opts).await.unwrap();
        daemon.request_name(BUS_NAME).await.unwrap();
        let client = Client::with_connection(connect(address.trim()).await).await.unwrap();
        let (registration, requests) = client.register_agent_channel().await.unwrap();
        Daemon { bus, client, requests, downloads, spool, _registration: registration, _daemon: daemon, _root: root }
    }

    async fn request(&mut self) -> PendingRequest {
        within("a prompt", self.requests.recv()).await.unwrap()
    }

    /// The only incoming transfer, once it is over.
    async fn finished(&self) -> TransferInfo {
        let transfers = self.client.transfers().await.unwrap();
        let incoming: Vec<_> = transfers.iter().filter(|t| t.direction == Direction::Incoming).collect();
        assert_eq!(incoming.len(), 1, "{transfers:#?}");
        within("the transfer's end", self.client.follow(&incoming[0].path.as_ref(), |_| {})).await.unwrap()
    }
}

async fn connect(address: &str) -> Connection {
    zbus::connection::Builder::address(address).build().await.unwrap()
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, fut).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn sender() -> Sender {
    Sender::new(Config::new("Test iPhone", "0123456789ab")).unwrap()
}

/// Waits for the receiver to listen, and checks what it says about itself.
async fn discover(veth: &Veth, sender: &Sender) {
    let receiver = within("the receiver", async {
        loop {
            if let Ok(Some(receiver)) = sender.discover(veth.receiver()).await {
                return receiver;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert_eq!(receiver.computer_name, "Test Laptop");
}

async fn send(veth: &Veth, sender: &Sender, payload: Payload) -> (Outcome, Vec<Event>) {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let outcome = within("the send", sender.send(veth.receiver(), &payload, RECEIVER_FLAGS, Some(tx))).await.unwrap();
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (outcome, events)
}

fn write(dir: &Path, name: &str, data: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    path
}

netns_test! {
    async fn accepted_files_and_folders_arrive(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;

        let src = tempfile::tempdir().unwrap();
        let hello = write(src.path(), "hello.txt", b"hello from an iPhone");
        write(src.path(), "Trip/day 1/a.txt", &[7; 300_000]);
        let payload = Payload::Files(vec![hello, src.path().join("Trip")]);
        let sending = tokio::spawn(async move { send(&veth, &sender, payload).await });

        let pending = d.request().await;
        let request = &pending.request;
        assert_eq!(request.peer.name, "Test iPhone");
        assert_eq!((request.peer.platform, request.peer.protocol), (Platform::Linux, Protocol::AirDrop));
        let names: Vec<_> = request.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["hello.txt", "Trip"]);
        assert!(request.pin.is_empty());
        assert!(request.time_left() <= Duration::from_secs(15), "owly-airdrop gives up on the prompt after 15 s");
        pending.respond(Decision::Accept);

        let (outcome, events) = sending.await.unwrap();
        assert_eq!(outcome, Outcome::Delivered);
        assert_eq!(events[..2], [Event::Asking, Event::Accepted]);
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Completed, "{}", done.error);
        assert_eq!(std::fs::read(d.downloads.join("hello.txt")).unwrap(), b"hello from an iPhone");
        assert_eq!(std::fs::read(d.downloads.join("Trip/day 1/a.txt")).unwrap().len(), 300_000);
        let sizes: Vec<_> = done.items.iter().map(|i| (i.name.as_str(), i.size)).collect();
        assert_eq!(sizes, [("hello.txt", 20), ("Trip", 300_000)]);
        assert!(done.items.iter().all(|i| i.path.is_some()));
        assert_eq!(done.bytes_done, done.bytes_total);
    }
}

netns_test! {
    async fn a_decline_reaches_the_sender_and_cools_down(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;
        let src = tempfile::tempdir().unwrap();
        let file = write(src.path(), "secret.pdf", b"%PDF");

        let payload = Payload::Files(vec![file.clone()]);
        let veth2 = veth.clone();
        let sending = tokio::spawn(async move { send(&veth2, &sender, payload).await });
        d.request().await.respond(Decision::Decline);
        assert_eq!(sending.await.unwrap().0, Outcome::Declined);
        assert_eq!(d.finished().await.state, TransferState::Rejected);

        // Asking again right away is declined without a prompt.
        let (outcome, _) = send(&veth, &self::sender(), Payload::Files(vec![file])).await;
        assert_eq!(outcome, Outcome::Declined);
        assert!(d.requests.try_recv().is_err());
        assert!(!d.downloads.join("secret.pdf").exists());
    }
}

netns_test! {
    async fn links_arrive_as_text(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;
        let url = "https://example.com/a?b=c".to_owned();
        let payload = Payload::Urls(vec![url.clone()]);
        let sending = tokio::spawn(async move { send(&veth, &sender, payload).await });

        let pending = d.request().await;
        assert_eq!(pending.request.items[0].mime, MIME_URL);
        assert_eq!(pending.request.items[0].name, url);
        pending.respond(Decision::Accept);
        assert_eq!(sending.await.unwrap().0, Outcome::Delivered);
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Completed);
        assert_eq!(done.items[0].text.as_deref(), Some(url.as_str()));
    }
}

netns_test! {
    async fn oversized_offers_are_refused_without_a_prompt(veth: Veth) {
        let mut d = Daemon::start(&veth, "[airdrop]\nmax_upload_bytes = 1000\n").await;
        let sender = sender();
        discover(&veth, &sender).await;
        let src = tempfile::tempdir().unwrap();
        let file = write(src.path(), "big.bin", &[0; 5000]);
        let (outcome, _) = send(&veth, &sender, Payload::Files(vec![file])).await;
        assert_eq!(outcome, Outcome::Declined);
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Rejected);
        assert!(done.error.contains("max_upload_bytes"), "{}", done.error);
        assert!(d.requests.try_recv().is_err());
    }
}

netns_test! {
    async fn cancelling_the_prompt_declines(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;
        let src = tempfile::tempdir().unwrap();
        let file = write(src.path(), "a.txt", b"a");
        let sending = tokio::spawn(async move { send(&veth, &sender, Payload::Files(vec![file])).await });
        let mut pending = d.request().await;
        d.client.cancel(&pending.request.transfer.as_ref()).await.unwrap();
        within("the prompt to close", pending.expired()).await;
        assert_eq!(sending.await.unwrap().0, Outcome::Declined);
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Cancelled);
        assert_eq!(done.error, "cancelled on this device");
    }
}

netns_test! {
    async fn announced_over_mdns_while_visible(veth: Veth) {
        let d = Daemon::start(&veth, "").await;
        let mut browse = Mdns::new(veth.a.index).browse().await.unwrap();
        let found = within("the announcement", async {
            loop {
                if let Some(BrowseEvent::Found(found)) = browse.events.recv().await {
                    return found;
                }
            }
        })
        .await;
        assert_eq!((found.addr, found.port, found.flags), (veth.b.addr, RECEIVER_PORT, RECEIVER_FLAGS));
        assert_eq!(found.instance.len(), 12, "a random service ID");

        d.client.set_visibility(Visibility::Hidden, Duration::ZERO).await.unwrap();
        let lost = within("the goodbye", async {
            loop {
                if let Some(BrowseEvent::Lost(instance)) = browse.events.recv().await {
                    return instance;
                }
            }
        })
        .await;
        assert_eq!(lost, found.instance);
        // And nothing listens any more.
        within("the receiver to stop", async {
            while sender().discover(veth.receiver()).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
    }
}

netns_test! {
    async fn a_sender_that_gives_up_withdraws_the_prompt(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;
        let src = tempfile::tempdir().unwrap();
        let file = write(src.path(), "a.txt", b"a");
        let sending = tokio::spawn(async move { send(&veth, &sender, Payload::Files(vec![file])).await });
        let mut pending = d.request().await;
        // Dropping the send closes its connection, as an iPhone does when its user taps Cancel.
        sending.abort();
        within("the prompt to close", pending.expired()).await;
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Cancelled);
        assert_eq!(done.error, "cancelled by the sender");
    }
}

netns_test! {
    async fn an_unanswered_prompt_is_declined_before_the_sender_gives_up(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;
        let src = tempfile::tempdir().unwrap();
        let file = write(src.path(), "a.txt", b"a");
        let addr = veth.receiver();
        let sending =
            tokio::spawn(async move { sender.send(addr, &Payload::Files(vec![file]), RECEIVER_FLAGS, None).await });
        let mut pending = d.request().await;
        let asked = Instant::now();
        tokio::time::timeout(Duration::from_secs(20), pending.expired()).await.expect("the prompt to time out");
        let outcome = within("the sender's answer", sending).await.unwrap().unwrap();
        assert!(asked.elapsed() < Duration::from_secs(15), "before owly-airdrop's own 15 s");
        assert_eq!(outcome, Outcome::Declined);
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Rejected);
        assert_eq!(done.error, "no answer before the deadline");
    }
}

netns_test! {
    async fn a_name_taken_in_downloads_gets_a_new_one(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        write(&d.downloads, "notes.txt", b"mine");
        let sender = sender();
        discover(&veth, &sender).await;
        let src = tempfile::tempdir().unwrap();
        let file = write(src.path(), "notes.txt", b"theirs");
        let sending = tokio::spawn(async move { send(&veth, &sender, Payload::Files(vec![file])).await });
        d.request().await.respond(Decision::Accept);
        assert_eq!(sending.await.unwrap().0, Outcome::Delivered);
        let done = d.finished().await;
        assert_eq!((done.items[0].name.as_str(), done.items[0].mime.as_str()), ("notes (1).txt", "text/plain"));
        assert_eq!(std::fs::read(d.downloads.join("notes.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(d.downloads.join("notes (1).txt")).unwrap(), b"theirs");
    }
}

/// Slows what `iface` sends to about 2 MB/s, so an upload is still running when a test acts on
/// it. `false` when `tc` cannot.
fn throttle(iface: &str) -> bool {
    Command::new("tc")
        .args(["qdisc", "add", "dev", iface, "root", "tbf", "rate", "16mbit", "burst", "32kb", "latency", "400ms"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A file the archive cannot compress, which takes about 2 s on a throttled link.
fn noise(dir: &Path) -> PathBuf {
    let bytes: Vec<u8> = (0..4 << 20).map(|_| rand::random()).collect();
    write(dir, "clip.mov", &bytes)
}

/// Starts sending a file of noise over a throttled link, accepts it, and returns once some of it
/// arrived: the transfer's path and the send. `None` when the link cannot be throttled.
async fn uploading(
    veth: &Veth,
    d: &mut Daemon,
    src: &Path,
) -> Option<(OwnedObjectPath, JoinHandle<std::io::Result<Outcome>>)> {
    if !throttle(veth.a.name) {
        eprintln!("skipping: tc cannot slow the link down");
        return None;
    }
    let sender = sender();
    discover(veth, &sender).await;
    let file = noise(src);
    let addr = veth.receiver();
    let sending =
        tokio::spawn(async move { sender.send(addr, &Payload::Files(vec![file]), RECEIVER_FLAGS, None).await });
    let pending = d.request().await;
    let path = pending.request.transfer.clone();
    pending.respond(Decision::Accept);
    within("the upload to start", async {
        while d.client.transfer(&path.as_ref()).await.unwrap().bytes_done == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    Some((path, sending))
}

impl Daemon {
    /// Waits until what arrived of an upload that did not finish is removed.
    async fn nothing_left_behind(&self) {
        within("what arrived to be removed", async {
            while std::fs::read_dir(&self.downloads).is_ok_and(|mut entries| entries.next().is_some()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }
}

netns_test! {
    async fn cancelling_an_upload_stops_it(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let src = tempfile::tempdir().unwrap();
        let Some((path, sending)) = uploading(&veth, &mut d, src.path()).await else { return };
        d.client.cancel(&path.as_ref()).await.unwrap();

        let outcome = within("the sender to notice", sending).await.unwrap();
        assert!(!matches!(outcome, Ok(Outcome::Delivered)), "{outcome:?}");
        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Cancelled);
        assert_eq!(done.error, "cancelled on this device");
        d.nothing_left_behind().await;
    }
}

netns_test! {
    async fn a_sender_that_stops_mid_upload_fails_the_transfer(veth: Veth) {
        let mut d = Daemon::start(&veth, "").await;
        let src = tempfile::tempdir().unwrap();
        let Some((_, sending)) = uploading(&veth, &mut d, src.path()).await else { return };
        sending.abort();

        let done = d.finished().await;
        assert_eq!(done.state, TransferState::Failed);
        assert_eq!(done.error, "the sender closed the connection");
        d.nothing_left_behind().await;
    }
}

netns_test! {
    async fn a_new_name_restarts_the_receiver(veth: Veth) {
        let d = Daemon::start(&veth, "").await;
        let sender = sender();
        discover(&veth, &sender).await;
        d.client.manager().set_device_name("Renamed Laptop").await.unwrap();
        within("the new name", async {
            loop {
                if let Ok(Some(r)) = sender.discover(veth.receiver()).await
                    && r.computer_name == "Renamed Laptop"
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
    }
}

/// An Apple device in Everyone mode on `owa`, for tests of sending: owly-airdrop's receiver.
struct Phone {
    events: mpsc::Receiver<ReceiverEvent>,
    _announced: Option<Registration>,
    _inbox: TempDir,
}

/// How a [`Phone`] can be found.
#[derive(PartialEq)]
enum Found {
    /// Announced over mDNS, on a port of its own, which the sender must take from there.
    Mdns,
    /// Only listening on Apple's port, like an iPhone, whose mDNS does not reach a Linux
    /// `awdl0`: the sender must knock.
    Knock,
}

/// The MAC address of `owa`, where a [`Phone`] listens.
const PHONE: &str = "02:00:00:00:00:0a";

impl Phone {
    async fn start(veth: &Veth, found: Found) -> Phone {
        let inbox = tempfile::tempdir().unwrap();
        let mut config = owly_airdrop::proto::receiver::Config::new("Anna's iPhone");
        config.model_name = "iPhone15,2".into();
        let port = if found == Found::Mdns { 0 } else { APPLE_PORT };
        let addr = SocketAddrV6::new(veth.a.addr, port, 0, veth.a.index);
        let options = receiver::Options::new(config, inbox.path().to_owned());
        let (receiver, events) = Receiver::bind(addr, &Identity::generate().unwrap(), options).await.unwrap();
        let port = receiver.local_addr().unwrap().port();
        tokio::spawn(receiver.run());
        let announced = match found {
            Found::Mdns => {
                let service = Service {
                    instance: service_id([1, 2, 3, 4, 5, 6]),
                    host: "anna".into(),
                    port,
                    addr: veth.a.addr,
                    flags: RECEIVER_FLAGS,
                };
                Some(Mdns::new(veth.a.index).register(service).await.unwrap())
            }
            Found::Knock => None,
        };
        Phone { events, _announced: announced, _inbox: inbox }
    }

    /// What happens next to a transfer.
    async fn next(&mut self) -> ReceiverEvent {
        loop {
            match within("the phone", self.events.recv()).await.expect("the phone stopped") {
                ReceiverEvent::Discovered { .. } | ReceiverEvent::Progress { .. } => {}
                event => return event,
            }
        }
    }

    async fn ask(&mut self) -> Request {
        match self.next().await {
            ReceiverEvent::Request(request) => request,
            other => panic!("expected an /Ask, got {other:?}"),
        }
    }

    async fn received(&mut self) -> Vec<PathBuf> {
        match self.next().await {
            ReceiverEvent::Received { paths, .. } => paths,
            other => panic!("expected files, got {other:?}"),
        }
    }
}

fn names(offer: &Offer) -> Vec<&str> {
    match offer {
        Offer::Files(files) => files.iter().map(|f| f.name.as_str()).collect(),
        Offer::Urls(urls) => urls.iter().map(String::as_str).collect(),
    }
}

impl Daemon {
    /// Starts discovery and waits for the device it finds.
    async fn find(&self) -> (DiscoverySession, Device) {
        let mut session = self.client.discover().await.unwrap();
        let device = within("a device", async {
            loop {
                if let Some(DiscoveryEvent::Found(device)) = session.next().await {
                    return device;
                }
            }
        })
        .await;
        (session, device)
    }

    async fn wait_for(&self, transfer: &OwnedObjectPath, what: &str, done: impl Fn(&TransferInfo) -> bool) {
        within(what, async {
            while !done(&self.client.transfer(&transfer.as_ref()).await.unwrap()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }

    async fn sent(&self, transfer: &OwnedObjectPath) -> TransferInfo {
        within("the send's end", self.client.follow(&transfer.as_ref(), |_| {})).await.unwrap()
    }

    /// Waits until nothing of a send is left where it was built.
    async fn spool_is_empty(&self) {
        within("the spool to empty", async {
            while std::fs::read_dir(&self.spool).is_ok_and(|mut entries| entries.next().is_some()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
    }
}

netns_test! {
    async fn files_go_to_a_receiver_found_over_mdns(veth: Veth) {
        let mut phone = Phone::start(&veth, Found::Mdns).await;
        let d = Daemon::launch(&veth, "", &[]).await;
        let (session, device) = d.find().await;
        assert_eq!(device.name, "Anna's iPhone");
        assert_eq!((device.platform, device.kind, device.protocol), (Platform::Apple, DeviceKind::Phone, Protocol::AirDrop));
        assert_eq!(device.mediums, [Medium::Awdl]);

        let src = tempfile::tempdir().unwrap();
        let files = [write(src.path(), "hello.txt", b"hello from Linux"), write(src.path(), "b.bin", &[7; 300_000])];
        let transfer = d.client.send_files(&device.id, &files).await.unwrap();
        // The send holds the link by itself.
        session.stop().await.unwrap();

        let request = phone.ask().await;
        assert_eq!((request.ask.sender_computer_name.as_str(), request.ask.sender_model_name.as_str()), ("Test Laptop", "Linux"));
        assert_eq!(names(&request.ask.offer), ["hello.txt", "b.bin"]);
        d.wait_for(&transfer, "the send to wait for an answer", |t| t.state == TransferState::AwaitingAcceptance).await;
        request.accept();

        let paths = phone.received().await;
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"hello from Linux");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), [7; 300_000]);
        let done = d.sent(&transfer).await;
        assert_eq!(done.state, TransferState::Completed, "{}", done.error);
        assert_eq!((done.direction, done.peer.name.as_str()), (Direction::Outgoing, "Anna's iPhone"));
        assert_eq!(done.bytes_done, done.bytes_total);
        d.spool_is_empty().await;
    }
}

netns_test! {
    async fn links_go_to_a_receiver_found_by_knocking(veth: Veth) {
        let mut phone = Phone::start(&veth, Found::Knock).await;
        let d = Daemon::launch(&veth, "", &[PHONE]).await;
        let (_session, device) = d.find().await;
        assert_eq!(device.name, "Anna's iPhone");

        let url = "https://example.com/a?b=c";
        let transfer = d.client.send_text(&device.id, url, true).await.unwrap();
        let request = phone.ask().await;
        assert!(matches!(request.ask.offer, Offer::Urls(_)));
        assert_eq!(names(&request.ask.offer), [url]);
        request.accept();
        match phone.next().await {
            ReceiverEvent::Urls { urls, .. } => assert_eq!(urls, [url]),
            other => panic!("expected the link, got {other:?}"),
        }
        let done = d.sent(&transfer).await;
        assert_eq!(done.state, TransferState::Completed, "{}", done.error);
    }
}

netns_test! {
    async fn text_goes_as_a_file(veth: Veth) {
        let mut phone = Phone::start(&veth, Found::Mdns).await;
        let d = Daemon::launch(&veth, "", &[]).await;
        let (_session, device) = d.find().await;
        let transfer = d.client.send_text(&device.id, "remember the milk", false).await.unwrap();
        let request = phone.ask().await;
        assert_eq!(names(&request.ask.offer), ["Text.txt"]);
        request.accept();
        let paths = phone.received().await;
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"remember the milk");
        assert_eq!(d.sent(&transfer).await.state, TransferState::Completed);
        d.spool_is_empty().await;
    }
}

netns_test! {
    async fn a_decline_is_a_rejection(veth: Veth) {
        let mut phone = Phone::start(&veth, Found::Mdns).await;
        let d = Daemon::launch(&veth, "", &[]).await;
        let (_session, device) = d.find().await;
        let src = tempfile::tempdir().unwrap();
        let transfer = d.client.send_files(&device.id, &[write(src.path(), "a.txt", b"a")]).await.unwrap();
        phone.ask().await.decline();
        let done = d.sent(&transfer).await;
        assert_eq!(done.state, TransferState::Rejected);
        assert!(done.error.is_empty(), "{}", done.error);
    }
}

netns_test! {
    async fn cancelling_while_the_receiver_decides_withdraws_the_offer(veth: Veth) {
        let mut phone = Phone::start(&veth, Found::Mdns).await;
        let d = Daemon::launch(&veth, "", &[]).await;
        let (_session, device) = d.find().await;
        let src = tempfile::tempdir().unwrap();
        let transfer = d.client.send_files(&device.id, &[write(src.path(), "a.txt", b"a")]).await.unwrap();
        let request = phone.ask().await;
        d.client.cancel(&transfer.as_ref()).await.unwrap();
        let done = d.sent(&transfer).await;
        assert_eq!((done.state, done.error.as_str()), (TransferState::Cancelled, "cancelled on this device"));
        assert!(matches!(phone.next().await, ReceiverEvent::Withdrawn { id } if id == request.id));
    }
}

netns_test! {
    async fn cancelling_an_upload_ends_it_at_the_receiver(veth: Veth) {
        if !throttle(veth.b.name) {
            eprintln!("skipping: tc cannot slow the link down");
            return;
        }
        let mut phone = Phone::start(&veth, Found::Mdns).await;
        let d = Daemon::launch(&veth, "", &[]).await;
        let (_session, device) = d.find().await;
        let src = tempfile::tempdir().unwrap();
        let transfer = d.client.send_files(&device.id, &[noise(src.path())]).await.unwrap();
        phone.ask().await.accept();
        d.wait_for(&transfer, "the upload to get going", |t| {
            t.state == TransferState::Transferring && t.bytes_done > 0 && t.bytes_done < t.bytes_total
        })
        .await;
        d.client.cancel(&transfer.as_ref()).await.unwrap();
        let done = d.sent(&transfer).await;
        assert_eq!((done.state, done.error.as_str()), (TransferState::Cancelled, "cancelled on this device"));
        assert!(matches!(phone.next().await, ReceiverEvent::Failed { .. }));
        d.spool_is_empty().await;
    }
}

netns_test! {
    async fn a_receiver_that_leaves_is_lost(veth: Veth) {
        let phone = Phone::start(&veth, Found::Mdns).await;
        let d = Daemon::launch(&veth, "", &[]).await;
        let (mut session, device) = d.find().await;
        // Its mDNS goodbye.
        drop(phone);
        let lost = within("the device to go", async {
            loop {
                if let Some(DiscoveryEvent::Lost(id)) = session.next().await {
                    return id;
                }
            }
        })
        .await;
        assert_eq!(lost, device.id);
        assert!(d.client.send_text(&device.id, "too late", false).await.is_err());
    }
}

netns_test! {
    async fn the_link_is_up_while_discovering(veth: Veth) {
        let d = Daemon::launch(&veth, "", &[]).await;
        assert_eq!(d.client.airdrop_link().await.unwrap().state, "down");
        let _session = d.client.discover().await.unwrap();
        let link = within("the link", async {
            loop {
                let link = d.client.airdrop_link().await.unwrap();
                if link.is_up() {
                    return link;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert_eq!((link.interface.as_deref(), link.wifi_paused), (Some("owb"), false));
    }
}
