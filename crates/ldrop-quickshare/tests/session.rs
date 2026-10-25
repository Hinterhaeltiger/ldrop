// SPDX-License-Identifier: GPL-3.0-or-later
//! Sender and receiver talking over an in-memory pipe.

use std::path::Path;
use std::time::Duration;

use ldrop_quickshare::receive::{self, Answer, InboundEvent, Offer, ReceiveOutcome};
use ldrop_quickshare::send::{self, OutboundEvent, Outgoing, OutgoingFile, SendOutcome};
use ldrop_quickshare::{DeviceType, EndpointId, EndpointInfo, Error, LocalEndpoint, RejectReason, TextKind};
use rand::Rng;
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
        let local =
            LocalEndpoint { id: EndpointId::random(), info: EndpointInfo::visible("Test laptop", DeviceType::Laptop) };
        let (out_tx, out_events) = mpsc::channel(64);
        let (in_tx, in_events) = mpsc::channel(64);
        let send_cancel = CancellationToken::new();
        let recv_cancel = CancellationToken::new();
        let sender = tokio::spawn({
            let cancel = send_cancel.clone();
            async move { send::send(a, &local, outgoing, out_tx, cancel).await }
        });
        let receiver = tokio::spawn(receive::receive(b, in_tx, recv_cancel.clone()));
        Pair { sender, receiver, out_events, in_events, send_cancel, recv_cancel }
    }

    async fn offer(&mut self) -> (Offer, oneshot::Sender<Answer>) {
        match timeout(LIMIT, self.in_events.recv()).await.unwrap().expect("receiver ended early") {
            InboundEvent::Offer(offer, reply) => (offer, reply),
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
