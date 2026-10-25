// SPDX-License-Identifier: GPL-3.0-or-later
//! End-to-end tests of the D-Bus API (SPEC §10) through `ldrop-client`, against `ldropd
//! --dev-sim` on a private `dbus-daemon`. Skipped when `dbus-daemon` is not installed.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ldrop_client::proxies::DebugProxy;
use ldrop_client::{Agent, Client, Decision, DiscoveryEvent, Error, IncomingRequest, TransferEvent, TransferInfo};
use ldrop_core::dbus::BUS_NAME;
use ldrop_core::paths::Paths;
use ldrop_core::{Direction, TransferState, Visibility};
use tempfile::TempDir;
use zbus::Connection;

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

const WAIT: Duration = Duration::from_secs(15);

/// A private bus plus a simulated daemon with its own config and state directories.
struct Harness {
    bus: Child,
    address: String,
    root: PathBuf,
    _tmp: Option<TempDir>,
    _daemon: Connection,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.bus.kill();
        let _ = self.bus.wait();
    }
}

impl Harness {
    /// `None` when `dbus-daemon` is unavailable.
    async fn start(config_toml: &str) -> Option<Harness> {
        let tmp = tempfile::tempdir().unwrap();
        let mut h = Self::start_in(tmp.path(), config_toml).await?;
        h._tmp = Some(tmp);
        Some(h)
    }

    /// Starts on an existing directory, keeping config and history already there.
    async fn start_in(root: &Path, config_toml: &str) -> Option<Harness> {
        let bus_config = root.join("bus.conf");
        std::fs::write(&bus_config, BUS_CONFIG.replace("@DIR@", &root.display().to_string())).unwrap();
        let config_file = paths(root).config_file;
        std::fs::create_dir_all(config_file.parent().unwrap()).unwrap();
        std::fs::write(&config_file, config_toml).unwrap();

        let mut bus = match Command::new("dbus-daemon")
            .arg(format!("--config-file={}", bus_config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!("skipping: cannot run dbus-daemon ({e})");
                return None;
            }
        };
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let address = address.trim().to_owned();

        let daemon = connect(&address).await;
        let opts = ldropd::Options { paths: paths(root), sim: true, probe_hardware: false };
        ldropd::start(&daemon, opts).await.unwrap();
        daemon.request_name(BUS_NAME).await.unwrap();
        Some(Harness { bus, address, root: root.to_owned(), _tmp: None, _daemon: daemon })
    }

    async fn client(&self) -> Client {
        Client::with_connection(connect(&self.address).await).await.unwrap()
    }

    fn paths(&self) -> Paths {
        paths(&self.root)
    }
}

fn paths(dir: &Path) -> Paths {
    Paths::under(&dir.join("config"), &dir.join("state"))
}

async fn connect(address: &str) -> Connection {
    zbus::connection::Builder::address(address).build().await.unwrap()
}

macro_rules! harness {
    ($config:expr) => {
        match Harness::start($config).await {
            Some(h) => h,
            None => return,
        }
    };
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, fut).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

async fn find_device(client: &Client, id: &str) -> ldrop_client::DiscoverySession {
    let mut session = client.discover().await.unwrap();
    within("device", async {
        loop {
            if let Some(DiscoveryEvent::Found(d)) = session.next().await
                && d.id == id
            {
                break;
            }
        }
    })
    .await;
    session
}

async fn inject(client: &Client, peer: &str, platform: &str, names: &[&str]) -> TransferInfo {
    let debug = DebugProxy::new(client.connection()).await.unwrap();
    let path = debug.inject_incoming(peer, platform, names, 1000).await.unwrap();
    client.transfer(&path.as_ref()).await.unwrap()
}

async fn finish(client: &Client, t: &TransferInfo) -> TransferInfo {
    within("final state", client.follow(&t.path.as_ref(), |_| {})).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn visibility_times_out() {
    let h = harness!("");
    let client = h.client().await;
    assert_eq!(client.status().await.unwrap().visibility, Visibility::Hidden);

    client.set_visibility(Visibility::Everyone, Duration::from_secs(1)).await.unwrap();
    let status = client.status().await.unwrap();
    assert_eq!(status.visibility, Visibility::Everyone);
    assert!(status.visibility_expires_at > 0);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let status = client.status().await.unwrap();
    assert_eq!(status.visibility, Visibility::Hidden);
    assert_eq!(status.visibility_expires_at, 0);

    let err = client.manager().set_visibility("contacts", 0).await.unwrap_err();
    assert!(matches!(err, Error::NotSupported(_)), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn renewed_visibility_ignores_old_timer() {
    let h = harness!("");
    let client = h.client().await;
    client.set_visibility(Visibility::Everyone, Duration::from_secs(1)).await.unwrap();
    client.set_visibility(Visibility::Everyone, Duration::from_secs(5)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(client.status().await.unwrap().visibility, Visibility::Everyone);
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_sessions_belong_to_their_client() {
    let h = harness!("");
    let alice = h.client().await;
    let bob = h.client().await;

    let session = find_device(&bob, "sim-iphone").await;
    let path = session.path().clone();
    let stop_as_alice = || async {
        alice
            .connection()
            .call_method(Some(BUS_NAME), path.as_ref(), Some("org.ldrop.Ldrop1.Discovery"), "Stop", &())
            .await
    };
    let err = stop_as_alice().await.unwrap_err();
    assert!(err.to_string().contains("PermissionDenied"), "{err}");

    // Bob leaves the bus: the session ends with him.
    drop(session);
    drop(bob);
    within("session cleanup", async {
        loop {
            match stop_as_alice().await {
                Err(e) if e.to_string().contains("PermissionDenied") => {
                    tokio::time::sleep(Duration::from_millis(50)).await
                }
                Err(_) => break,
                Ok(_) => panic!("stopped someone else's session"),
            }
        }
    })
    .await;

    // A new session reports devices already known.
    let session = find_device(&alice, "sim-pixel").await;
    session.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn send_completes_with_pin() {
    let h = harness!("");
    let client = h.client().await;
    let file = h.root.join("photo.jpg");
    std::fs::write(&file, b"jpeg").unwrap();

    let session = find_device(&client, "sim-pixel").await;
    let path = client.send_files("sim-pixel", &[&file]).await.unwrap();
    session.stop().await.unwrap();

    let mut states = Vec::new();
    let mut pin = String::new();
    let done = within(
        "send",
        client.follow(&path.as_ref(), |t| {
            if states.last() != Some(&t.state) {
                states.push(t.state);
            }
            if !t.pin.is_empty() {
                pin = t.pin.clone();
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(done.state, TransferState::Completed);
    assert_eq!(done.direction, Direction::Outgoing);
    assert_eq!(done.bytes_done, 4);
    assert_eq!(done.items[0].name, "photo.jpg");
    assert_eq!(done.items[0].mime, "image/jpeg");
    assert_eq!(pin.len(), 4, "Quick Share shows a PIN");
    assert!(states.contains(&TransferState::AwaitingAcceptance));
    assert!(states.contains(&TransferState::Transferring));
}

#[tokio::test(flavor = "multi_thread")]
async fn send_declined_and_unknown_device() {
    let h = harness!("");
    let client = h.client().await;
    let session = find_device(&client, "sim-iphone").await;

    // The simulated peer declines items whose name contains "decline".
    let file = h.root.join("decline-me.txt");
    std::fs::write(&file, b"x").unwrap();
    let path = client.send_files("sim-iphone", &[&file]).await.unwrap();
    let done = within("decline", client.follow(&path.as_ref(), |_| {})).await.unwrap();
    assert_eq!(done.state, TransferState::Rejected);
    assert!(done.pin.is_empty(), "AirDrop has no PIN");

    let err = client.send_text("nope", "hi", false).await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
    session.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_outgoing() {
    let h = harness!("");
    let client = h.client().await;
    let _session = find_device(&client, "sim-pixel").await;
    let path = client.send_text("sim-pixel", "hello", false).await.unwrap();
    client.cancel(&path.as_ref()).await.unwrap();
    let done = within("cancel", client.follow(&path.as_ref(), |_| {})).await.unwrap();
    assert_eq!(done.state, TransferState::Cancelled);
    let err = client.cancel(&path.as_ref()).await.unwrap_err();
    assert!(matches!(err, Error::Failed(_)), "{err:?}");
}

struct Answer {
    decision: Decision,
    seen: Arc<Mutex<Vec<IncomingRequest>>>,
}

impl Agent for Answer {
    async fn request_accept(&self, request: IncomingRequest) -> Decision {
        self.seen.lock().unwrap().push(request);
        self.decision
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_accepts() {
    let h = harness!("");
    let frontend = h.client().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _reg = frontend.serve_agent(Answer { decision: Decision::Accept, seen: seen.clone() }).await.unwrap();

    let peer = h.client().await;
    let t = inject(&peer, "Pixel 9", "android", &["a.jpg", "b.pdf"]).await;
    let done = finish(&peer, &t).await;
    assert_eq!(done.state, TransferState::Completed);
    assert_eq!(done.direction, Direction::Incoming);

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let r = &seen[0];
    assert_eq!(r.transfer, t.path);
    assert_eq!(r.peer.name, "Pixel 9");
    assert_eq!(r.pin.len(), 4);
    assert_eq!(r.items.len(), 2);
    assert_eq!(r.bytes_total, 2000);
    assert!(r.time_left() > Duration::from_secs(30));
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_declines_and_unregisters() {
    let h = harness!("");
    let frontend = h.client().await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let reg = frontend.serve_agent(Answer { decision: Decision::Decline, seen: seen.clone() }).await.unwrap();

    let t = inject(&frontend, "Anna's iPhone", "apple", &["IMG_1.HEIC"]).await;
    assert_eq!(finish(&frontend, &t).await.state, TransferState::Rejected);
    assert!(seen.lock().unwrap()[0].pin.is_empty());

    reg.unregister().await.unwrap();
    // Without an agent or notification server the request waits for Manager.Accept.
    let t = inject(&frontend, "Anna's iPhone", "apple", &["IMG_2.HEIC"]).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert!(frontend.transfer(&t.path.as_ref()).await.unwrap().needs_answer());
    frontend.accept(&t.path.as_ref()).await.unwrap();
    assert_eq!(finish(&frontend, &t).await.state, TransferState::Completed);
}

#[tokio::test(flavor = "multi_thread")]
async fn agent_channel_expires_when_answered_elsewhere() {
    let h = harness!("");
    let frontend = h.client().await;
    let (_reg, mut requests) = frontend.register_agent_channel().await.unwrap();

    let other = h.client().await;
    let t = inject(&other, "Pixel 9", "android", &["a.jpg"]).await;
    let mut pending = within("agent request", requests.recv()).await.unwrap();
    assert_eq!(pending.request.transfer, t.path);

    other.reject(&t.path.as_ref()).await.unwrap();
    within("prompt expiry", pending.expired()).await;
    assert_eq!(finish(&other, &t).await.state, TransferState::Rejected);
    // A second answer is refused.
    assert!(other.accept(&t.path.as_ref()).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn unanswered_request_times_out() {
    let h = harness!("[quickshare]\nprompt_timeout_secs = 5\n");
    let client = h.client().await;
    let t = inject(&client, "Pixel 9", "android", &["a.jpg"]).await;
    assert!(t.needs_answer());
    assert!(t.deadline > 0);
    let done = finish(&client, &t).await;
    assert_eq!(done.state, TransferState::Rejected);
    assert!(done.error.contains("deadline"), "{}", done.error);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_sees_new_transfers_and_history_clear() {
    let h = harness!("");
    let client = h.client().await;
    let mut watch = client.watch_transfers().await.unwrap();
    assert_eq!(watch.transfers().count(), 0);

    let t = inject(&client, "Pixel 9", "android", &["a.jpg"]).await;
    match within("added", watch.next()).await.unwrap() {
        TransferEvent::Added(info) => assert_eq!(info.id, t.id),
        other => panic!("expected Added, got {other:?}"),
    }
    client.reject(&t.path.as_ref()).await.unwrap();
    within("rejected", async {
        loop {
            if let Some(TransferEvent::Changed(info)) = watch.next().await
                && info.state == TransferState::Rejected
            {
                break;
            }
        }
    })
    .await;

    client.clear_history().await.unwrap();
    assert_eq!(within("removed", watch.next()).await.unwrap(), TransferEvent::Removed(t.id));
    assert!(client.transfers().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn history_survives_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(h) = Harness::start_in(tmp.path(), "").await else { return };
    let client = h.client().await;
    let t = inject(&client, "Pixel 9", "android", &["a.jpg"]).await;
    client.accept(&t.path.as_ref()).await.unwrap();
    finish(&client, &t).await;
    let history = std::fs::read_to_string(h.paths().history_file()).unwrap();
    assert!(history.contains("a.jpg"));
    drop(client);
    drop(h);

    let h = Harness::start_in(tmp.path(), "").await.unwrap();
    let client = h.client().await;
    let list = client.transfers().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].state, TransferState::Completed);
    assert_eq!(list[0].items[0].name, "a.jpg");
    assert_eq!(list[0].peer.name, "Pixel 9");

    client.clear_history().await.unwrap();
    assert!(client.transfers().await.unwrap().is_empty());
    assert!(!h.paths().history_file().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn device_name_and_download_dir_persist() {
    let h = harness!("");
    let client = h.client().await;
    client.manager().set_device_name("Test Box").await.unwrap();
    assert_eq!(client.status().await.unwrap().device_name, "Test Box");
    let text = std::fs::read_to_string(h.paths().config_file).unwrap();
    assert!(text.contains("device_name = \"Test Box\""), "{text}");

    assert!(client.manager().set_device_name("bad\nname").await.is_err());
    assert!(client.manager().set_download_dir("relative/dir").await.is_err());

    let downloads = h.root.join("dl");
    std::fs::create_dir(&downloads).unwrap();
    client.manager().set_download_dir(downloads.to_str().unwrap()).await.unwrap();
    assert_eq!(client.status().await.unwrap().download_dir, downloads.display().to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn capabilities_without_hardware() {
    let h = harness!("");
    let report = h.client().await.capabilities().await.unwrap();
    assert_eq!(report.backends.len(), 8);
    assert!(!report.wifi.present);
}
