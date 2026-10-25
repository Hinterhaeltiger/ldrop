// SPDX-License-Identifier: GPL-3.0-or-later
//! Sender and receiver talking over an in-memory pipe.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use ldrop_quickshare::link::{
    BoxFuture, BoxStream, Hosted, Joined, Link, LocalLinks, Medium, PeerLinks, UpgradePath, Upgrader, WifiNetwork,
};
use ldrop_quickshare::receive::{self, Answer, InboundEvent, Offer, ReceiveOutcome};
use ldrop_quickshare::send::{self, OutboundEvent, Outgoing, OutgoingFile, SendOutcome};
use ldrop_quickshare::{DeviceType, EndpointId, EndpointInfo, Error, LocalEndpoint, RejectReason, TextKind};
use rand::Rng;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const LIMIT: Duration = Duration::from_secs(20);

struct Pair {
    sender: JoinHandle<Result<SendOutcome, Error>>,
    receiver: JoinHandle<Result<ReceiveOutcome, Error>>,
    out_events: mpsc::Receiver<OutboundEvent>,
    in_events: mpsc::Receiver<InboundEvent>,
    send_cancel: CancellationToken,
    recv_cancel: CancellationToken,
}

impl Pair {
    fn start(outgoing: Outgoing) -> Self {
        let (a, b) = tokio::io::duplex(64 * 1024);
        Self::start_with(outgoing, a, Link::fixed(Medium::WifiLan), b, Link::fixed(Medium::WifiLan))
    }

    fn start_with<A, B>(outgoing: Outgoing, a: A, send_link: Link, b: B, recv_link: Link) -> Self
    where
        A: AsyncRead + AsyncWrite + Send + Unpin + 'static,
        B: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let local =
            LocalEndpoint { id: EndpointId::random(), info: EndpointInfo::visible("Test laptop", DeviceType::Laptop) };
        let (out_tx, out_events) = mpsc::channel(64);
        let (in_tx, in_events) = mpsc::channel(64);
        let send_cancel = CancellationToken::new();
        let recv_cancel = CancellationToken::new();
        let sender = tokio::spawn({
            let cancel = send_cancel.clone();
            async move { send::send(a, &local, &send_link, outgoing, out_tx, cancel).await }
        });
        let receiver = tokio::spawn({
            let cancel = recv_cancel.clone();
            async move { receive::receive(b, &recv_link, in_tx, cancel).await }
        });
        Pair { sender, receiver, out_events, in_events, send_cancel, recv_cancel }
    }

    async fn offer(&mut self) -> (Offer, oneshot::Sender<Answer>) {
        match timeout(LIMIT, self.in_events.recv()).await.unwrap().expect("receiver ended early") {
            InboundEvent::Offer(offer, reply) => (*offer, reply),
            InboundEvent::Progress { .. } => panic!("progress before offer"),
        }
    }

    async fn sender_pin(&mut self) -> String {
        match timeout(LIMIT, self.out_events.recv()).await.unwrap() {
            Some(OutboundEvent::Pin(pin)) => pin,
            other => panic!("expected PIN, got {other:?}"),
        }
    }

    async fn wait_out(&mut self, want: impl Fn(&OutboundEvent) -> bool) {
        loop {
            let ev = timeout(LIMIT, self.out_events.recv()).await.unwrap().expect("sender ended early");
            if want(&ev) {
                return;
            }
        }
    }

    async fn results(self) -> (Result<SendOutcome, Error>, Result<ReceiveOutcome, Error>) {
        let s = timeout(LIMIT, self.sender).await.expect("sender hangs").unwrap();
        let r = timeout(LIMIT, self.receiver).await.expect("receiver hangs").unwrap();
        (s, r)
    }
}

fn random_bytes(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    rand::rng().fill(v.as_mut_slice());
    v
}

fn write_files(dir: &Path, contents: &[Vec<u8>]) -> Vec<OutgoingFile> {
    contents
        .iter()
        .enumerate()
        .map(|(i, data)| {
            let path = dir.join(format!("src-{i}.bin"));
            std::fs::write(&path, data).unwrap();
            OutgoingFile { path, name: format!("file {i}.bin"), mime: "application/octet-stream".into() }
        })
        .collect()
}

#[tokio::test]
async fn files_arrive_intact_and_pins_match() {
    let src = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let contents = vec![Vec::new(), b"hello".to_vec(), random_bytes(1_300_000)];
    let mut pair = Pair::start(Outgoing::Files(write_files(src.path(), &contents)));

    let pin = pair.sender_pin().await;
    let (offer, reply) = pair.offer().await;
    assert_eq!(offer.pin, pin);
    assert_eq!(pin.len(), 4);
    assert_eq!(offer.peer.display_name(), "Test laptop");
    assert_eq!(offer.peer.info.device_type, DeviceType::Laptop);
    assert_eq!(
        offer.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        ["file 0.bin", "file 1.bin", "file 2.bin"]
    );
    assert_eq!(offer.total_size(), 1_300_005);
    reply.send(Answer::Accept { staging: staging.path().to_owned() }).unwrap();
    pair.wait_out(|e| matches!(e, OutboundEvent::Accepted)).await;

    let mut last_progress = 0;
    while let Some(ev) = pair.in_events.recv().await {
        if let InboundEvent::Progress { bytes_done } = ev {
            assert!(bytes_done >= last_progress);
            last_progress = bytes_done;
        }
    }
    assert_eq!(last_progress, 1_300_005);
    let (s, r) = pair.results().await;
    assert_eq!(s.unwrap(), SendOutcome::Completed);
    assert!(matches!(r.unwrap(), ReceiveOutcome::Completed(_)));
    for (i, data) in contents.iter().enumerate() {
        assert_eq!(&std::fs::read(staging.path().join(i.to_string())).unwrap(), data, "file {i}");
    }
}

#[tokio::test]
async fn many_files() {
    let src = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let contents: Vec<Vec<u8>> = (0..150).map(|i| random_bytes(i * 37)).collect();
    let mut pair = Pair::start(Outgoing::Files(write_files(src.path(), &contents)));
    let (offer, reply) = pair.offer().await;
    assert_eq!(offer.files.len(), 150);
    reply.send(Answer::Accept { staging: staging.path().to_owned() }).unwrap();
    let (s, r) = pair.results().await;
    assert_eq!(s.unwrap(), SendOutcome::Completed);
    assert!(matches!(r.unwrap(), ReceiveOutcome::Completed(_)));
    for (i, data) in contents.iter().enumerate() {
        assert_eq!(&std::fs::read(staging.path().join(i.to_string())).unwrap(), data);
    }
}

#[tokio::test]
async fn text_and_url() {
    for (text, kind, title) in [
        ("https://example.org/page?x=1", TextKind::Url, "example.org"),
        ("Grüße\nzweite Zeile", TextKind::Text, "Grüße"),
    ] {
        let mut pair = Pair::start(Outgoing::Text { text: text.into(), kind });
        let (offer, reply) = pair.offer().await;
        assert!(offer.files.is_empty());
        assert_eq!(offer.texts.len(), 1);
        assert_eq!(offer.texts[0].kind, kind);
        assert_eq!(offer.texts[0].title, title);
        assert_eq!(offer.texts[0].size, text.len() as u64);
        let staging = tempfile::tempdir().unwrap();
        reply.send(Answer::Accept { staging: staging.path().to_owned() }).unwrap();
        let (s, r) = pair.results().await;
        assert_eq!(s.unwrap(), SendOutcome::Completed);
        let ReceiveOutcome::Completed(received) = r.unwrap() else { panic!("not completed") };
        assert_eq!(received.texts, [text]);
        assert_eq!(std::fs::read_dir(staging.path()).unwrap().count(), 0, "text never touches disk");
    }
}

#[tokio::test]
async fn rejections_reach_the_sender() {
    for reason in [RejectReason::Declined, RejectReason::NotEnoughSpace, RejectReason::TimedOut] {
        let mut pair = Pair::start(Outgoing::Text { text: "x".into(), kind: TextKind::Text });
        let (_, reply) = pair.offer().await;
        reply.send(Answer::Reject(reason)).unwrap();
        let (s, r) = pair.results().await;
        assert_eq!(s.unwrap(), SendOutcome::Rejected(reason));
        assert!(matches!(r.unwrap(), ReceiveOutcome::Rejected(got) if got == reason));
    }
}

#[tokio::test]
async fn receiver_cancels_mid_transfer() {
    let src = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let mut pair = Pair::start(Outgoing::Files(write_files(src.path(), &[random_bytes(64 * 1024 * 1024)])));
    let (_, reply) = pair.offer().await;
    reply.send(Answer::Accept { staging: staging.path().to_owned() }).unwrap();
    loop {
        if let Some(InboundEvent::Progress { .. }) = timeout(LIMIT, pair.in_events.recv()).await.unwrap() {
            break;
        }
    }
    pair.recv_cancel.cancel();
    let (s, r) = pair.results().await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(matches!(s, Err(Error::PeerCancelled)), "{s:?}");
}

#[tokio::test]
async fn sender_cancels_mid_transfer() {
    let src = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let mut pair = Pair::start(Outgoing::Files(write_files(src.path(), &[random_bytes(64 * 1024 * 1024)])));
    let (_, reply) = pair.offer().await;
    reply.send(Answer::Accept { staging: staging.path().to_owned() }).unwrap();
    pair.wait_out(|e| matches!(e, OutboundEvent::Progress { .. })).await;
    pair.send_cancel.cancel();
    let (s, r) = pair.results().await;
    assert!(matches!(s, Err(Error::Cancelled)), "{s:?}");
    assert!(matches!(r, Err(Error::PeerCancelled)), "{r:?}");
}

#[tokio::test]
async fn sender_cancels_while_waiting_for_consent() {
    let mut pair = Pair::start(Outgoing::Text { text: "x".into(), kind: TextKind::Text });
    let (_, mut reply) = pair.offer().await;
    pair.send_cancel.cancel();
    timeout(LIMIT, reply.closed()).await.expect("prompt is withdrawn");
    let (s, r) = pair.results().await;
    assert!(matches!(s, Err(Error::Cancelled)), "{s:?}");
    assert!(matches!(r, Err(Error::PeerCancelled)), "{r:?}");
}

#[tokio::test]
async fn dropping_the_reply_cancels() {
    let mut pair = Pair::start(Outgoing::Text { text: "x".into(), kind: TextKind::Text });
    let (_, reply) = pair.offer().await;
    drop(reply);
    let (s, r) = pair.results().await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(matches!(s, Err(Error::PeerCancelled)), "{s:?}");
}

/// An in-memory "network": hosted mediums are ports, joining one makes a pipe to its host.
#[derive(Default)]
struct Net {
    hosts: Mutex<HashMap<u16, mpsc::Sender<BoxStream>>>,
    next_port: AtomicU16,
    /// Bytes the hosts read from upgraded channels.
    upgraded_bytes: Arc<AtomicU64>,
}

#[derive(Default)]
struct TestUpgrader {
    net: Arc<Net>,
    hostable: Vec<Medium>,
    joinable: Vec<Medium>,
    fail_join: Vec<Medium>,
    hosted: Mutex<Vec<Medium>>,
    /// Set when a lease is dropped.
    released: Arc<AtomicBool>,
}

struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl Upgrader for TestUpgrader {
    fn local_links(&self) -> LocalLinks {
        LocalLinks { mediums: self.joinable.clone(), ..Default::default() }
    }

    fn hostable(&self, _peer: &PeerLinks) -> Vec<Medium> {
        self.hostable.clone()
    }

    fn host(&self, medium: Medium) -> BoxFuture<Result<Hosted, Error>> {
        self.hosted.lock().unwrap().push(medium);
        let port = 1000 + self.net.next_port.fetch_add(1, Ordering::SeqCst);
        let (tx, incoming) = mpsc::channel(4);
        self.net.hosts.lock().unwrap().insert(port, tx);
        let net =
            WifiNetwork { ssid: "DIRECT-test".into(), password: "pw".into(), port, gateway: None, frequency: None };
        let path = match medium {
            Medium::WifiLan => UpgradePath::WifiLan { addr: ([127, 0, 0, 1], port).into() },
            Medium::WifiHotspot => UpgradePath::WifiHotspot(net),
            Medium::WifiDirect => UpgradePath::WifiDirect(net),
            Medium::Bluetooth => unreachable!("not hostable"),
        };
        let lease = Box::new(Release(self.released.clone()));
        Box::pin(async move { Ok(Hosted { path, incoming, lease }) })
    }

    fn join(&self, path: UpgradePath) -> BoxFuture<Result<Joined, Error>> {
        let fail = self.fail_join.contains(&path.medium());
        let port = match &path {
            UpgradePath::WifiLan { addr } => addr.port(),
            UpgradePath::WifiHotspot(net) | UpgradePath::WifiDirect(net) => net.port,
        };
        let host = self.net.hosts.lock().unwrap().get(&port).cloned();
        let counter = self.net.upgraded_bytes.clone();
        let lease = Box::new(Release(self.released.clone()));
        Box::pin(async move {
            if fail {
                return Err(Error::Io(io::Error::other("cannot join")));
            }
            let host = host.ok_or_else(|| Error::Io(io::ErrorKind::ConnectionRefused.into()))?;
            let (a, b) = tokio::io::duplex(64 * 1024);
            host.send(Box::new(Counting { inner: b, counter })).await.map_err(|_| Error::Closed)?;
            Ok(Joined { stream: Box::new(a), lease })
        })
    }
}

/// Counts bytes read.
struct Counting {
    inner: DuplexStream,
    counter: Arc<AtomicU64>,
}

impl AsyncRead for Counting {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        ready!(Pin::new(&mut self.inner).poll_read(cx, buf))?;
        self.counter.fetch_add((buf.filled().len() - before) as u64, Ordering::SeqCst);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Counting {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A Bluetooth-speed pipe: at most 16 KiB per millisecond in each direction.
struct Slow {
    inner: DuplexStream,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

fn slow_pair() -> (Slow, Slow) {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let wrap = |inner| Slow { inner, sleep: Box::pin(tokio::time::sleep(Duration::ZERO)) };
    (wrap(a), wrap(b))
}

impl AsyncRead for Slow {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for Slow {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        ready!(self.sleep.as_mut().poll(cx));
        let len = buf.len().min(16 * 1024);
        let n = ready!(Pin::new(&mut self.inner).poll_write(cx, &buf[..len]))?;
        self.sleep.as_mut().reset(tokio::time::Instant::now() + Duration::from_millis(1));
        Poll::Ready(Ok(n))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

struct Upgrade {
    net: Arc<Net>,
    sender: Arc<TestUpgrader>,
    receiver: Arc<TestUpgrader>,
}

impl Upgrade {
    fn new(joinable: &[Medium], hostable: &[Medium], fail_join: &[Medium]) -> Self {
        let net = Arc::new(Net::default());
        let sender = Arc::new(TestUpgrader {
            net: net.clone(),
            joinable: joinable.to_vec(),
            fail_join: fail_join.to_vec(),
            ..Default::default()
        });
        let receiver = Arc::new(TestUpgrader { net: net.clone(), hostable: hostable.to_vec(), ..Default::default() });
        Upgrade { net, sender, receiver }
    }

    fn links(&self) -> (Link, Link) {
        let link = |u: &Arc<TestUpgrader>| Link { medium: Medium::Bluetooth, upgrader: Some(u.clone()) };
        (link(&self.sender), link(&self.receiver))
    }
}

/// Sends `contents` over a slow link with the given upgraders; returns the staging dir.
async fn transfer_over_bluetooth(contents: &[Vec<u8>], send_link: Link, recv_link: Link) {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).try_init();
    let src = tempfile::tempdir().unwrap();
    let staging = tempfile::tempdir().unwrap();
    let (a, b) = slow_pair();
    let mut pair = Pair::start_with(Outgoing::Files(write_files(src.path(), contents)), a, send_link, b, recv_link);
    let (_, reply) = pair.offer().await;
    reply.send(Answer::Accept { staging: staging.path().to_owned() }).unwrap();
    let (s, r) = pair.results().await;
    assert_eq!(s.unwrap(), SendOutcome::Completed);
    assert!(matches!(r.unwrap(), ReceiveOutcome::Completed(_)));
    for (i, data) in contents.iter().enumerate() {
        assert_eq!(&std::fs::read(staging.path().join(i.to_string())).unwrap(), data, "file {i}");
    }
}

#[tokio::test]
async fn upgrade_to_wifi_lan_mid_transfer() {
    let up = Upgrade::new(&[Medium::WifiLan], &[Medium::WifiLan], &[]);
    let (send_link, recv_link) = up.links();
    let contents = vec![random_bytes(6_000_000), b"small".to_vec()];
    transfer_over_bluetooth(&contents, send_link, recv_link).await;
    assert_eq!(*up.receiver.hosted.lock().unwrap(), [Medium::WifiLan]);
    // At 16 KiB/ms the old link alone would need 375 ms; the upgrade takes a few.
    assert!(up.net.upgraded_bytes.load(Ordering::SeqCst) > 3_000_000, "most data moves to the new channel");
    assert!(up.receiver.released.load(Ordering::SeqCst), "host lease released");
    assert!(up.sender.released.load(Ordering::SeqCst), "join lease released");
}

#[tokio::test]
async fn upgrade_to_hotspot_without_extra_encryption() {
    let up = Upgrade::new(&[Medium::WifiHotspot], &[Medium::WifiHotspot], &[]);
    let (send_link, recv_link) = up.links();
    let contents = vec![random_bytes(4_000_000)];
    transfer_over_bluetooth(&contents, send_link, recv_link).await;
    assert!(up.net.upgraded_bytes.load(Ordering::SeqCst) > 2_000_000);
}

#[tokio::test]
async fn failed_join_falls_back_to_the_next_medium() {
    let up = Upgrade::new(
        &[Medium::WifiLan, Medium::WifiHotspot],
        &[Medium::WifiLan, Medium::WifiHotspot],
        &[Medium::WifiLan],
    );
    let (send_link, recv_link) = up.links();
    transfer_over_bluetooth(&[random_bytes(4_000_000)], send_link, recv_link).await;
    assert_eq!(*up.receiver.hosted.lock().unwrap(), [Medium::WifiLan, Medium::WifiHotspot]);
    assert!(up.net.upgraded_bytes.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn sender_without_upgrader_stays_on_bluetooth() {
    let up = Upgrade::new(&[], &[Medium::WifiLan], &[]);
    let (_, recv_link) = up.links();
    transfer_over_bluetooth(&[random_bytes(1_500_000)], Link::fixed(Medium::Bluetooth), recv_link).await;
    assert_eq!(*up.receiver.hosted.lock().unwrap(), [Medium::WifiLan]);
    assert_eq!(up.net.upgraded_bytes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mediums_the_sender_cannot_join_are_not_offered() {
    let up = Upgrade::new(&[Medium::WifiDirect], &[Medium::WifiLan], &[]);
    let (send_link, recv_link) = up.links();
    transfer_over_bluetooth(&[random_bytes(1_500_000)], send_link, recv_link).await;
    assert!(up.receiver.hosted.lock().unwrap().is_empty());
}

#[tokio::test]
async fn small_transfers_stay_put() {
    let up = Upgrade::new(&[Medium::WifiLan], &[Medium::WifiLan], &[]);
    let (send_link, recv_link) = up.links();
    transfer_over_bluetooth(&[random_bytes(200_000)], send_link, recv_link).await;
    assert!(up.receiver.hosted.lock().unwrap().is_empty());
}
