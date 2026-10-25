// SPDX-License-Identifier: GPL-3.0-or-later
//! The connection below the sharing layer: plaintext handshake (SPEC §5.3 steps 2–5), then an
//! encrypted channel that can move to a faster medium mid-session (bandwidth upgrade, §5.4).
//!
//! The writer task owns the write half and the [`Sealer`], so dropping any future in a
//! session never leaves a half-written frame. It also sends a keep-alive every 10 s
//! (§5.3 step 10). The reader task owns the [`Opener`]: it decrypts, answers keep-alives,
//! reassembles bytes payloads, forwards everything else and runs the upgrade negotiation.
//! A small task per stream only reads raw frames, so the reader can wait on several things
//! at once without losing data.
//!
//! The upgrade follows Nearby Connections' `BwuManager` [R44] (docs/research/quickshare-upgrade.md):
//! 1. The host writes `UPGRADE_PATH_AVAILABLE` on the old channel.
//! 2. The other side connects and introduces itself on the new channel in plaintext; the
//!    host acknowledges.
//! 3. Each side writes `LAST_WRITE_TO_PRIOR_CHANNEL` on the old channel and holds further
//!    data. The new channel continues the old one's encryption (sequence numbers carry on),
//!    or runs in plaintext if both sides agreed to drop it.
//! 4. On reading the peer's `LAST_WRITE`, a side answers `SAFE_TO_CLOSE_PRIOR_CHANNEL`.
//! 5. On reading `SAFE_TO_CLOSE`, a side writes a plaintext disconnection on the old channel,
//!    reads it once more, closes it and continues on the new one.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior, sleep_until, timeout};
use tracing::{debug, info, trace, warn};

use crate::crypto::{ClientHandshake, HandshakeError, Opener, Sealer, ServerHandshake, Session};
use crate::endpoint::{EndpointId, EndpointInfo};
use crate::framing::{MAX_FRAME_LEN, read_frame, write_frame};
use crate::link::{BoxStream, Hosted, Joined, Lease, Link, LocalLinks, Medium, PeerLinks, UpgradePath, Upgrader};
use crate::proto::location::nearby::connections::bandwidth_upgrade_negotiation_frame::{
    EventType as BwuEvent, UpgradePathInfo,
};
use crate::proto::location::nearby::connections::connection_response_frame::ResponseStatus;
use crate::proto::location::nearby::connections::payload_transfer_frame::PacketType;
use crate::proto::location::nearby::connections::payload_transfer_frame::control_message::EventType;
use crate::proto::location::nearby::connections::payload_transfer_frame::payload_header::PayloadType;
use crate::proto::location::nearby::connections::{
    BandwidthUpgradeNegotiationFrame, ConnectionRequestFrame, OfflineFrame, V1Frame, v1_frame,
};
use crate::proto::sharing::nearby as sharing;
use crate::{Error, LocalEndpoint, frames};

/// Per-message limit while the plaintext handshake runs.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// Android sends keep-alives every few seconds and acks ours; silence this long means the peer is gone.
const PEER_SILENCE: Duration = Duration::from_secs(45);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PENDING_BYTES_PAYLOADS: usize = 8;
/// Setting up a medium to host (a hotspot needs a few seconds).
const HOST_TIMEOUT: Duration = Duration::from_secs(30);
/// From offering a medium to the peer introducing itself on it.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(60);
/// Joining the peer's medium: scanning, association and DHCP.
const JOIN_TIMEOUT: Duration = Duration::from_secs(45);
/// From our `LAST_WRITE` to the peer's `SAFE_TO_CLOSE`.
const SWITCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Waiting for the old channel's final plaintext disconnection.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

/// The other side, as its connection request describes it.
#[derive(Debug, Clone)]
pub struct RemoteEndpoint {
    pub id: EndpointId,
    pub info: EndpointInfo,
    /// `endpoint_name` from the request; hidden devices carry their name only here.
    pub endpoint_name: Option<String>,
    pub links: PeerLinks,
}

impl RemoteEndpoint {
    pub fn display_name(&self) -> String {
        self.info.name.clone().or_else(|| self.endpoint_name.clone()).unwrap_or_else(|| self.id.to_string())
    }
}

/// What the reader task forwards.
#[derive(Debug)]
pub(crate) enum Incoming {
    /// A complete bytes payload: a sharing frame, or text/Wi-Fi data the session asked for.
    Bytes {
        id: i64,
        data: Vec<u8>,
    },
    FileChunk {
        id: i64,
        offset: i64,
        body: Vec<u8>,
        last: bool,
    },
    /// The peer sent PAYLOAD_CANCELED or PAYLOAD_ERROR for a payload.
    PayloadCancelled,
    Disconnected,
}

pub(crate) enum Cmd {
    Frame(Box<OfflineFrame>),
    Shutdown,
}

/// Which side of the connection we are; it decides our part in an upgrade.
pub(crate) enum Role {
    /// We connected, so we may join a medium the peer hosts. Our ID goes into the introduction.
    Client { local_id: EndpointId },
    /// The peer connected, so we may host. Its ID must come back in the introduction.
    Server { remote_id: EndpointId, peer: PeerLinks },
}

async fn read_plain<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, Error> {
    timeout(HANDSHAKE_TIMEOUT, read_frame(stream)).await.map_err(|_| Error::Timeout)?
}

async fn write_plain<S: AsyncWrite + Unpin>(stream: &mut S, frame: &OfflineFrame) -> Result<(), Error> {
    timeout(HANDSHAKE_TIMEOUT, write_frame(stream, &frame.encode_to_vec())).await.map_err(|_| Error::Timeout)?
}

fn decode_offline(raw: &[u8], expected: v1_frame::FrameType) -> Result<V1Frame, Error> {
    let frame = OfflineFrame::decode(raw).map_err(|_| Error::protocol("offline frame does not parse"))?;
    let v1 = frame.v1.ok_or_else(|| Error::protocol("offline frame without v1"))?;
    if v1.r#type() != expected {
        return Err(Error::protocol(format!("expected {expected:?}, got {:?}", v1.r#type())));
    }
    Ok(v1)
}

async fn fail_handshake<S: AsyncWrite + Unpin>(stream: &mut S, e: HandshakeError) -> Error {
    if let Some(alert) = e.alert_message() {
        let _ = timeout(HANDSHAKE_TIMEOUT, write_frame(stream, &alert)).await;
    }
    e.into()
}

fn check_connection_response(raw: &[u8]) -> Result<(), Error> {
    let v1 = decode_offline(raw, v1_frame::FrameType::ConnectionResponse)?;
    let response = v1.connection_response.unwrap_or_default();
    if response.response() != ResponseStatus::Accept {
        return Err(Error::protocol("peer refused the connection"));
    }
    Ok(())
}

fn peer_links(request: &ConnectionRequestFrame) -> PeerLinks {
    let meta = request.medium_metadata.clone().unwrap_or_default();
    PeerLinks {
        mediums: request.mediums.iter().filter_map(|&m| Medium::from_proto(m)).collect(),
        ip: <[u8; 4]>::try_from(meta.ip_address()).ok().map(Into::into),
        supports_5ghz: meta.supports_5_ghz(),
    }
}

/// Receiving side, up to and including both plaintext connection responses.
pub(crate) async fn server_handshake<S>(stream: &mut S) -> Result<(RemoteEndpoint, Session), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let v1 = decode_offline(&read_plain(stream).await?, v1_frame::FrameType::ConnectionRequest)?;
    let request = v1.connection_request.ok_or_else(|| Error::protocol("connection request without body"))?;
    let id = EndpointId::parse(request.endpoint_id()).ok_or_else(|| Error::protocol("bad endpoint id"))?;
    let info = EndpointInfo::decode(request.endpoint_info())?;
    let links = peer_links(&request);
    let remote = RemoteEndpoint { id, info, endpoint_name: request.endpoint_name.filter(|n| !n.is_empty()), links };

    let server = match ServerHandshake::respond(&read_plain(stream).await?) {
        Ok(server) => server,
        Err(e) => return Err(fail_handshake(stream, e).await),
    };
    write_frame(stream, server.server_init()).await?;
    let session = match server.finish(&read_plain(stream).await?) {
        Ok(session) => session,
        Err(e) => return Err(fail_handshake(stream, e).await),
    };
    check_connection_response(&read_plain(stream).await?)?;
    write_frame(stream, &frames::connection_response().encode_to_vec()).await?;
    Ok((remote, session))
}

/// Sending side, up to and including both plaintext connection responses.
pub(crate) async fn client_handshake<S>(
    stream: &mut S,
    local: &LocalEndpoint,
    links: &LocalLinks,
) -> Result<Session, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &frames::connection_request(local.id, &local.info, links).encode_to_vec()).await?;
    let client = ClientHandshake::new();
    write_frame(stream, client.client_init()).await?;
    let (client_finish, session) = match client.finish(&read_plain(stream).await?) {
        Ok(done) => done,
        Err(e) => return Err(fail_handshake(stream, e).await),
    };
    write_frame(stream, &client_finish).await?;
    write_frame(stream, &frames::connection_response().encode_to_vec()).await?;
    check_connection_response(&read_plain(stream).await?)?;
    Ok(session)
}

/// Aborts the task when dropped.
struct Task(JoinHandle<()>);

impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}

type RawFrames = mpsc::Receiver<Result<Vec<u8>, Error>>;

/// Reads length-prefixed frames off one stream until it fails.
fn spawn_raw_reader(mut r: ReadHalf<BoxStream>) -> (RawFrames, Task) {
    let (tx, rx) = mpsc::channel(4);
    let task = tokio::spawn(async move {
        loop {
            let result = timeout(PEER_SILENCE, read_frame(&mut r)).await.unwrap_or(Err(Error::Timeout));
            let end = result.is_err();
            if tx.send(result).await.is_err() || end {
                return;
            }
        }
    });
    (rx, Task(task))
}

/// The encrypted channel. Dropping it aborts both tasks; call [`Channel::close`] to end politely.
pub(crate) struct Channel {
    pub(crate) tx: mpsc::Sender<Cmd>,
    pub(crate) rx: mpsc::Receiver<Result<Incoming, Error>>,
    upgrade: mpsc::UnboundedSender<()>,
    chunk_len: Arc<AtomicUsize>,
    /// The peer's disconnection has been read.
    peer_gone: bool,
    writer: Option<JoinHandle<Result<(), Error>>>,
    reader: JoinHandle<()>,
}

impl Channel {
    pub(crate) fn spawn(stream: BoxStream, session: Session, role: Role, link: &Link) -> Self {
        let (r, w) = tokio::io::split(stream);
        // Small queues: file chunks are up to 512 KiB each and backpressure must reach the file reader.
        let (tx, data) = mpsc::channel(4);
        let (out, rx) = mpsc::channel(4);
        let (ctl, ctl_rx) = mpsc::unbounded_channel();
        let (upgrade, requests) = mpsc::unbounded_channel();
        let (events_tx, events) = mpsc::unbounded_channel();
        let chunk_len = Arc::new(AtomicUsize::new(link.medium.chunk_len()));
        let writer = Writer { w, encrypt: true, medium: link.medium, sealer: session.sealer, next: None };
        let writer = tokio::spawn(writer.run(data, ctl_rx));
        let (frames, raw) = spawn_raw_reader(r);
        let reader = Reader {
            opener: session.opener,
            decrypt: true,
            frames,
            _raw: raw,
            out,
            pending: None,
            finished: false,
            data: tx.clone(),
            ctl,
            requests,
            requests_open: true,
            buffers: HashMap::new(),
            disconnected: false,
            medium: link.medium,
            chunk_len: chunk_len.clone(),
            role,
            upgrader: link.upgrader.clone(),
            events_tx,
            events,
            bwu: Bwu::Idle,
            peer_last_write: false,
            leases: Vec::new(),
        };
        let reader = tokio::spawn(reader.run());
        Channel { tx, rx, upgrade, chunk_len, peer_gone: false, writer: Some(writer), reader }
    }

    /// Payload chunk size for the medium in use.
    pub(crate) fn chunk_len(&self) -> usize {
        self.chunk_len.load(Ordering::Relaxed)
    }

    /// Asks to move to a faster medium, if we are the host side and one is available. The
    /// session carries on meanwhile.
    pub(crate) fn request_upgrade(&self) {
        let _ = self.upgrade.send(());
    }

    pub(crate) async fn send(&mut self, frame: OfflineFrame) -> Result<(), Error> {
        if self.tx.send(Cmd::Frame(Box::new(frame))).await.is_ok() {
            return Ok(());
        }
        Err(self.writer_error().await)
    }

    /// Sends a sharing frame as a bytes payload with a fresh random ID.
    pub(crate) async fn send_sharing(&mut self, frame: &sharing::Frame) -> Result<(), Error> {
        self.send_bytes(frames::random_id(), frame.encode_to_vec()).await
    }

    pub(crate) async fn send_bytes(&mut self, id: i64, data: Vec<u8>) -> Result<(), Error> {
        for frame in frames::bytes_payload(id, data) {
            self.send(frame).await?;
        }
        Ok(())
    }

    pub(crate) async fn recv(&mut self) -> Result<Incoming, Error> {
        let incoming = self.rx.recv().await.unwrap_or(Err(Error::Closed));
        self.peer_gone |= matches!(incoming, Ok(Incoming::Disconnected));
        incoming
    }

    /// Why the writer stopped.
    pub(crate) async fn writer_error(&mut self) -> Error {
        match self.writer.take() {
            Some(handle) => match handle.await {
                Ok(Err(e)) => e,
                _ => Error::Closed,
            },
            None => Error::Closed,
        }
    }

    /// Sends `Disconnection`, waits for queued frames to go out, then reads until the peer
    /// says goodbye or hangs up, so closing does not reset data it has not read yet.
    pub(crate) async fn close(&mut self) -> Result<(), Error> {
        let _ = timeout(CLOSE_TIMEOUT, async {
            let _ = self.tx.send(Cmd::Frame(Box::new(frames::disconnection()))).await;
            let _ = self.tx.send(Cmd::Shutdown).await;
        })
        .await;
        let result = match self.writer.take() {
            Some(handle) => match timeout(CLOSE_TIMEOUT, handle).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(Error::Closed),
                Err(_) => Err(Error::Timeout),
            },
            None => Err(Error::Closed),
        };
        let _ = timeout(CLOSE_TIMEOUT, async {
            while !self.peer_gone
                && self.rx.recv().await.is_some_and(|incoming| {
                    self.peer_gone = matches!(incoming, Ok(Incoming::Disconnected));
                    true
                })
            {}
        })
        .await;
        result
    }

    /// Best effort: tell the peer we stopped, then close.
    pub(crate) async fn abort(&mut self) {
        let _ = timeout(CLOSE_TIMEOUT, self.send_sharing(&frames::cancel())).await;
        let _ = self.close().await;
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if let Some(writer) = &self.writer {
            writer.abort();
        }
        self.reader.abort();
    }
}

/// Upgrade steps the reader asks the writer for.
enum Ctl {
    /// A negotiation frame for the current stream; goes out even while data is held.
    Frame(Box<OfflineFrame>),
    /// Write `LAST_WRITE_TO_PRIOR_CHANNEL`, then hold data until [`Ctl::ClosePrior`].
    LastWrite { next: WriteHalf<BoxStream>, encrypt: bool, medium: Medium },
    /// Write a plaintext disconnection on the old stream, close it and continue on the new one.
    ClosePrior,
}

struct Writer {
    w: WriteHalf<BoxStream>,
    encrypt: bool,
    medium: Medium,
    sealer: Sealer,
    /// Set between our `LAST_WRITE` and the switch; data is held meanwhile.
    next: Option<(WriteHalf<BoxStream>, bool, Medium)>,
}

impl Writer {
    async fn write(&mut self, frame: &OfflineFrame) -> Result<(), Error> {
        let bytes = if self.encrypt { self.sealer.seal(frame) } else { frame.encode_to_vec() };
        timeout(WRITE_TIMEOUT, write_frame(&mut self.w, &bytes)).await.map_err(|_| Error::Timeout)?
    }

    /// Half-closes TCP so the peer sees the end. RFCOMM has no half-close: shutting it down
    /// can drop data still queued, so it is closed only once the peer hangs up.
    async fn shutdown(w: &mut WriteHalf<BoxStream>, medium: Medium) -> Result<(), Error> {
        if medium == Medium::Bluetooth {
            return Ok(());
        }
        timeout(CLOSE_TIMEOUT, w.shutdown()).await.map_err(|_| Error::Timeout)??;
        Ok(())
    }

    async fn run(mut self, mut data: mpsc::Receiver<Cmd>, mut ctl: mpsc::UnboundedReceiver<Ctl>) -> Result<(), Error> {
        let mut keepalive = tokio::time::interval_at(Instant::now() + KEEPALIVE_INTERVAL, KEEPALIVE_INTERVAL);
        keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut ctl_open = true;
        loop {
            let held = self.next.is_some();
            tokio::select! {
                biased;
                c = ctl.recv(), if ctl_open => match c {
                    None => ctl_open = false,
                    Some(Ctl::Frame(frame)) => self.write(&frame).await?,
                    Some(Ctl::LastWrite { next, encrypt, medium }) => {
                        self.write(&frames::last_write_to_prior_channel()).await?;
                        self.next = Some((next, encrypt, medium));
                    }
                    Some(Ctl::ClosePrior) => {
                        let Some((next, encrypt, medium)) = self.next.take() else { continue };
                        self.encrypt = false;
                        // Best effort: the old stream is finished either way.
                        let _ = self.write(&frames::disconnection()).await;
                        let mut prior = std::mem::replace(&mut self.w, next);
                        let _ = Self::shutdown(&mut prior, self.medium).await;
                        (self.encrypt, self.medium) = (encrypt, medium);
                        keepalive.reset();
                    }
                },
                cmd = data.recv(), if !held => match cmd {
                    Some(Cmd::Frame(frame)) => self.write(&frame).await?,
                    Some(Cmd::Shutdown) | None => break,
                },
                _ = keepalive.tick(), if !held => self.write(&frames::keep_alive(false)).await?,
            }
        }
        Self::shutdown(&mut self.w, self.medium).await
    }
}

/// The stream we are moving to.
struct NextChannel {
    frames: RawFrames,
    raw: Task,
    decrypt: bool,
    medium: Medium,
}

/// Upgrade negotiation state.
enum Bwu {
    Idle,
    /// We host `medium` and wait for the peer on it; `rest` are the fallbacks.
    Hosting {
        medium: Medium,
        rest: Vec<Medium>,
        _task: Task,
    },
    /// We join the medium the peer hosts.
    Joining {
        medium: Medium,
        _task: Task,
    },
    /// Our `LAST_WRITE` is out; waiting for the peer's `SAFE_TO_CLOSE`.
    Switching {
        next: NextChannel,
        deadline: Instant,
    },
    /// Reading the old stream's final frame.
    Draining {
        next: NextChannel,
        deadline: Instant,
    },
    /// Upgraded, or nothing left to try.
    Done,
}

impl Bwu {
    fn deadline(&self) -> Option<Instant> {
        match self {
            Bwu::Switching { deadline, .. } | Bwu::Draining { deadline, .. } => Some(*deadline),
            _ => None,
        }
    }
}

enum Wake {
    Event(Event),
    /// `false` once the channel handle is gone.
    Request(bool),
    Raw(Result<Vec<u8>, Error>),
    Deadline,
}

/// Results of the hosting and joining tasks.
enum Event {
    Ready { medium: Medium, stream: BoxStream, encrypt: bool, lease: Lease },
    Failed { medium: Medium, why: Error },
}

struct Reader {
    opener: Opener,
    decrypt: bool,
    frames: RawFrames,
    _raw: Task,
    out: mpsc::Sender<Result<Incoming, Error>>,
    /// Waiting for room in `out`; nothing more is read meanwhile.
    pending: Option<Result<Incoming, Error>>,
    finished: bool,
    /// For keep-alive acks.
    data: mpsc::Sender<Cmd>,
    ctl: mpsc::UnboundedSender<Ctl>,
    requests: mpsc::UnboundedReceiver<()>,
    requests_open: bool,
    buffers: HashMap<i64, Vec<u8>>,
    disconnected: bool,
    medium: Medium,
    chunk_len: Arc<AtomicUsize>,
    role: Role,
    upgrader: Option<Arc<dyn Upgrader>>,
    events_tx: mpsc::UnboundedSender<Event>,
    events: mpsc::UnboundedReceiver<Event>,
    bwu: Bwu,
    /// The peer's `LAST_WRITE` arrived before our side of the switch was ready.
    peer_last_write: bool,
    /// Mediums in use; dropped with the channel.
    leases: Vec<Lease>,
}

impl Reader {
    async fn run(mut self) {
        loop {
            if self.finished && self.pending.is_none() {
                return;
            }
            let deadline = self.bwu.deadline();
            let wake = tokio::select! {
                biased;
                Some(event) = self.events.recv() => Wake::Event(event),
                request = self.requests.recv(), if self.requests_open => Wake::Request(request.is_some()),
                permit = self.out.reserve(), if self.pending.is_some() => match permit {
                    Ok(permit) => {
                        permit.send(self.pending.take().expect("checked above"));
                        continue;
                    }
                    Err(_) => return,
                },
                raw = self.frames.recv(), if self.pending.is_none() && !self.finished => {
                    Wake::Raw(raw.unwrap_or(Err(Error::Closed)))
                }
                () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => Wake::Deadline,
            };
            match wake {
                Wake::Event(event) => self.on_event(event),
                Wake::Request(true) => self.start_upgrade(),
                Wake::Request(false) => self.requests_open = false,
                Wake::Raw(raw) => {
                    if let Err(e) = self.on_raw(raw) {
                        self.fail(e);
                    }
                }
                Wake::Deadline => self.on_deadline(),
            }
        }
    }

    fn deliver(&mut self, incoming: Incoming) {
        // Keep reading after a disconnection so the stream drains to EOF.
        self.disconnected |= matches!(incoming, Incoming::Disconnected);
        self.pending = Some(Ok(incoming));
    }

    fn fail(&mut self, e: Error) {
        if !self.disconnected {
            self.pending = Some(Err(e));
        }
        self.finished = true;
    }

    fn on_raw(&mut self, raw: Result<Vec<u8>, Error>) -> Result<(), Error> {
        if matches!(self.bwu, Bwu::Draining { .. }) {
            // The old stream's plaintext disconnection, its end, or whatever else: it is done.
            self.finish_switch();
            return Ok(());
        }
        let raw = raw?;
        if self.disconnected {
            trace!(len = raw.len(), "frame after disconnection ignored");
            return Ok(());
        }
        let frame = if self.decrypt {
            self.opener.open(&raw)?
        } else {
            OfflineFrame::decode(raw.as_slice()).map_err(|_| Error::protocol("offline frame does not parse"))?
        };
        self.demux(frame)
    }

    fn demux(&mut self, frame: OfflineFrame) -> Result<(), Error> {
        let v1 = frame.v1.ok_or_else(|| Error::protocol("offline frame without v1"))?;
        match v1.r#type() {
            v1_frame::FrameType::KeepAlive => {
                if !v1.keep_alive.and_then(|k| k.ack).unwrap_or(false) {
                    // Never block the reader on a full writer queue: the data frames in it count as activity.
                    let _ = self.data.try_send(Cmd::Frame(Box::new(frames::keep_alive(true))));
                }
            }
            v1_frame::FrameType::Disconnection => self.deliver(Incoming::Disconnected),
            v1_frame::FrameType::PayloadTransfer => {
                let transfer = v1.payload_transfer.ok_or_else(|| Error::protocol("payload transfer without body"))?;
                if let Some(incoming) = payload(transfer, &mut self.buffers)? {
                    self.deliver(incoming);
                }
            }
            v1_frame::FrameType::BandwidthUpgradeNegotiation => {
                self.on_bwu_frame(v1.bandwidth_upgrade_negotiation.unwrap_or_default());
            }
            other => debug!(frame = ?other, "ignoring offline frame"),
        }
        Ok(())
    }

    fn control(&self, frame: OfflineFrame) {
        let _ = self.ctl.send(Ctl::Frame(Box::new(frame)));
    }

    fn on_bwu_frame(&mut self, frame: BandwidthUpgradeNegotiationFrame) {
        match frame.event_type() {
            BwuEvent::UpgradePathAvailable => self.on_path_available(frame.upgrade_path_info.unwrap_or_default()),
            BwuEvent::UpgradeFailure => match std::mem::replace(&mut self.bwu, Bwu::Idle) {
                Bwu::Hosting { medium, rest, .. } => {
                    info!(?medium, "peer could not join the medium we offered");
                    self.host_next(rest);
                }
                other => self.bwu = other,
            },
            BwuEvent::LastWriteToPriorChannel => match self.bwu {
                Bwu::Switching { .. } => self.control(frames::safe_to_close_prior_channel()),
                Bwu::Draining { .. } | Bwu::Done => debug!("unexpected LAST_WRITE_TO_PRIOR_CHANNEL"),
                _ => self.peer_last_write = true,
            },
            BwuEvent::SafeToClosePriorChannel => match std::mem::replace(&mut self.bwu, Bwu::Idle) {
                Bwu::Switching { next, .. } => {
                    let _ = self.ctl.send(Ctl::ClosePrior);
                    self.bwu = Bwu::Draining { next, deadline: Instant::now() + DRAIN_TIMEOUT };
                }
                other => {
                    debug!("SAFE_TO_CLOSE_PRIOR_CHANNEL without an upgrade in progress");
                    self.bwu = other;
                }
            },
            other => debug!(event = ?other, "ignoring upgrade frame"),
        }
    }

    /// The peer hosts a medium for us.
    fn on_path_available(&mut self, info: UpgradePathInfo) {
        let refuse = |me: &Self, why: &str| {
            info!(medium = ?info.medium(), why, "declining upgrade");
            me.control(frames::upgrade_failure(info.medium.unwrap_or_default()));
        };
        let (Role::Client { local_id }, Some(upgrader), Bwu::Idle) = (&self.role, &self.upgrader, &self.bwu) else {
            return refuse(self, "not possible now");
        };
        let Some(path) = UpgradePath::from_proto(&info) else {
            return refuse(self, "unsupported medium or credentials");
        };
        let medium = path.medium();
        if medium == self.medium {
            return refuse(self, "already on that medium");
        }
        let disable = info.supports_disabling_encryption() && medium.may_disable_encryption();
        info!(?medium, "peer offers a faster medium");
        let task = tokio::spawn(join(
            upgrader.clone(),
            path,
            info.supports_client_introduction_ack(),
            disable,
            *local_id,
            self.events_tx.clone(),
        ));
        self.bwu = Bwu::Joining { medium, _task: Task(task) };
    }

    /// The session asked to upgrade: offer what we can host, best first.
    fn start_upgrade(&mut self) {
        let (Role::Server { peer, .. }, Some(upgrader), Bwu::Idle) = (&self.role, &self.upgrader, &self.bwu) else {
            return;
        };
        let mediums: Vec<Medium> =
            upgrader.hostable(peer).into_iter().filter(|m| *m != self.medium && peer.mediums.contains(m)).collect();
        debug!(?mediums, peer = ?peer.mediums, "upgrade requested");
        self.host_next(mediums);
    }

    fn host_next(&mut self, mut mediums: Vec<Medium>) {
        let (Role::Server { remote_id, .. }, Some(upgrader)) = (&self.role, &self.upgrader) else { return };
        if mediums.is_empty() {
            info!(medium = ?self.medium, "no faster medium available, staying");
            self.bwu = Bwu::Done;
            return;
        }
        let medium = mediums.remove(0);
        info!(?medium, "offering a faster medium");
        let task = tokio::spawn(host(upgrader.clone(), medium, *remote_id, self.ctl.clone(), self.events_tx.clone()));
        self.bwu = Bwu::Hosting { medium, rest: mediums, _task: Task(task) };
    }

    fn on_event(&mut self, event: Event) {
        match (event, std::mem::replace(&mut self.bwu, Bwu::Idle)) {
            (Event::Failed { medium, why }, Bwu::Hosting { medium: m, rest, .. }) if m == medium => {
                warn!(?medium, "hosting failed: {why}");
                self.host_next(rest);
            }
            (Event::Failed { medium, why }, Bwu::Joining { medium: m, .. }) if m == medium => {
                warn!(?medium, "joining failed: {why}");
                self.control(frames::upgrade_failure(medium.to_proto()));
            }
            (
                Event::Ready { medium, stream, encrypt, lease },
                Bwu::Hosting { medium: m, .. } | Bwu::Joining { medium: m, .. },
            ) if m == medium => {
                self.begin_switch(medium, stream, encrypt, lease);
            }
            // A task that finished after we moved on.
            (_, state) => self.bwu = state,
        }
    }

    fn begin_switch(&mut self, medium: Medium, stream: BoxStream, encrypt: bool, lease: Lease) {
        debug!(?medium, encrypt, "new channel ready, switching");
        let (r, w) = tokio::io::split(stream);
        let (frames, raw) = spawn_raw_reader(r);
        self.leases.push(lease);
        let _ = self.ctl.send(Ctl::LastWrite { next: w, encrypt, medium });
        let next = NextChannel { frames, raw, decrypt: encrypt, medium };
        self.bwu = Bwu::Switching { next, deadline: Instant::now() + SWITCH_TIMEOUT };
        if std::mem::take(&mut self.peer_last_write) {
            self.control(frames::safe_to_close_prior_channel());
        }
    }

    fn finish_switch(&mut self) {
        let Bwu::Draining { next, .. } = std::mem::replace(&mut self.bwu, Bwu::Done) else { return };
        // Replacing the raw reader aborts the old one, which closes the old stream.
        (self.frames, self._raw) = (next.frames, next.raw);
        self.decrypt = next.decrypt;
        self.medium = next.medium;
        self.chunk_len.store(next.medium.chunk_len(), Ordering::Relaxed);
        info!(medium = ?next.medium, encrypted = next.decrypt, "connection upgraded");
    }

    fn on_deadline(&mut self) {
        match self.bwu {
            Bwu::Switching { .. } => self.fail(Error::Timeout),
            Bwu::Draining { .. } => self.finish_switch(),
            _ => {}
        }
    }
}

/// Reads one plaintext negotiation frame from a new channel.
async fn read_bwu<S: AsyncRead + Unpin>(stream: &mut S) -> Result<BandwidthUpgradeNegotiationFrame, Error> {
    let v1 = decode_offline(&read_plain(stream).await?, v1_frame::FrameType::BandwidthUpgradeNegotiation)?;
    v1.bandwidth_upgrade_negotiation.ok_or_else(|| Error::protocol("upgrade frame without body"))
}

/// Host side: set up `medium`, offer it, and wait for the peer to introduce itself on it.
async fn host(
    upgrader: Arc<dyn Upgrader>,
    medium: Medium,
    remote_id: EndpointId,
    ctl: mpsc::UnboundedSender<Ctl>,
    events: mpsc::UnboundedSender<Event>,
) {
    let result = async {
        let Hosted { path, mut incoming, lease } =
            timeout(HOST_TIMEOUT, upgrader.host(medium)).await.map_err(|_| Error::Timeout)??;
        ctl.send(Ctl::Frame(Box::new(frames::upgrade_path_available(&path)))).map_err(|_| Error::Closed)?;
        let (stream, disable) = timeout(ACCEPT_TIMEOUT, async {
            loop {
                let mut stream = incoming.recv().await.ok_or(Error::Closed)?;
                match introduction(&mut stream, remote_id).await {
                    Ok(disable) => return Ok::<_, Error>((stream, disable)),
                    Err(e) => debug!("dropping connection on the new medium: {e}"),
                }
            }
        })
        .await
        .map_err(|_| Error::Timeout)??;
        // We offered to drop encryption only where the medium allows it.
        let encrypt = !(disable && medium.may_disable_encryption());
        Ok::<_, Error>(Event::Ready { medium, stream, encrypt, lease })
    }
    .await;
    let _ = events.send(result.unwrap_or_else(|why| Event::Failed { medium, why }));
}

/// Reads the client introduction, checks it is our peer and acknowledges. Returns whether
/// the peer wants to drop encryption.
async fn introduction(stream: &mut BoxStream, remote_id: EndpointId) -> Result<bool, Error> {
    let frame = read_bwu(stream).await?;
    let intro = match (frame.event_type(), frame.client_introduction) {
        (BwuEvent::ClientIntroduction, Some(intro)) => intro,
        (event, _) => return Err(Error::protocol(format!("expected a client introduction, got {event:?}"))),
    };
    if intro.endpoint_id() != remote_id.as_str() {
        return Err(Error::protocol(format!("introduced as {:?}, expected {remote_id}", intro.endpoint_id())));
    }
    write_plain(stream, &frames::client_introduction_ack()).await?;
    Ok(intro.supports_disabling_encryption())
}

/// Joining side: connect to what the peer hosts and introduce ourselves.
async fn join(
    upgrader: Arc<dyn Upgrader>,
    path: UpgradePath,
    wants_ack: bool,
    disable: bool,
    local_id: EndpointId,
    events: mpsc::UnboundedSender<Event>,
) {
    let medium = path.medium();
    let result = async {
        let Joined { mut stream, lease } =
            timeout(JOIN_TIMEOUT, upgrader.join(path)).await.map_err(|_| Error::Timeout)??;
        write_plain(&mut stream, &frames::client_introduction(local_id, disable)).await?;
        if wants_ack {
            let frame = read_bwu(&mut stream).await?;
            if frame.event_type() != BwuEvent::ClientIntroductionAck {
                return Err(Error::protocol(format!("expected an introduction ack, got {:?}", frame.event_type())));
            }
        }
        Ok(Event::Ready { medium, stream, encrypt: !disable, lease })
    }
    .await;
    let _ = events.send(result.unwrap_or_else(|why| Event::Failed { medium, why }));
}

fn payload(
    transfer: crate::proto::location::nearby::connections::PayloadTransferFrame,
    buffers: &mut HashMap<i64, Vec<u8>>,
) -> Result<Option<Incoming>, Error> {
    match transfer.packet_type() {
        PacketType::Control => {
            let event = transfer.control_message.unwrap_or_default().event();
            return Ok(matches!(event, EventType::PayloadCanceled | EventType::PayloadError)
                .then_some(Incoming::PayloadCancelled));
        }
        PacketType::PayloadAck => return Ok(None),
        PacketType::Data | PacketType::UnknownPacketType => {}
    }
    let header = transfer.payload_header.ok_or_else(|| Error::protocol("payload without header"))?;
    let chunk = transfer.payload_chunk.ok_or_else(|| Error::protocol("payload without chunk"))?;
    let id = header.id();
    let last = chunk.flags() & frames::LAST_CHUNK != 0;
    match header.r#type() {
        PayloadType::Bytes => {
            let total = header.total_size();
            if !(0..=MAX_FRAME_LEN as i64).contains(&total) {
                return Err(Error::protocol(format!("bytes payload of {total} bytes")));
            }
            if !buffers.contains_key(&id) && buffers.len() >= MAX_PENDING_BYTES_PAYLOADS {
                return Err(Error::protocol("too many concurrent bytes payloads"));
            }
            let buffer = buffers.entry(id).or_default();
            if chunk.offset() != buffer.len() as i64 {
                return Err(Error::protocol("bytes payload chunk out of order"));
            }
            let body = chunk.body.unwrap_or_default();
            if (buffer.len() + body.len()) as i64 > total {
                return Err(Error::protocol("bytes payload exceeds its announced size"));
            }
            buffer.extend_from_slice(&body);
            Ok(last.then(|| Incoming::Bytes { id, data: buffers.remove(&id).unwrap_or_default() }))
        }
        PayloadType::File => {
            Ok(Some(Incoming::FileChunk { id, offset: chunk.offset(), body: chunk.body.unwrap_or_default(), last }))
        }
        other => Err(Error::protocol(format!("unsupported payload type {other:?}"))),
    }
}

/// Decodes a bytes payload as a sharing frame and returns its V1 body.
pub(crate) fn decode_sharing(data: &[u8]) -> Result<sharing::V1Frame, Error> {
    let frame = sharing::Frame::decode(data).map_err(|_| Error::protocol("sharing frame does not parse"))?;
    frame.v1.ok_or_else(|| Error::protocol("sharing frame without v1"))
}

/// Next sharing frame during negotiation. Peer cancel or disconnect end the session.
pub(crate) async fn recv_sharing(ch: &mut Channel) -> Result<sharing::V1Frame, Error> {
    match ch.recv().await? {
        Incoming::Bytes { data, .. } => {
            let v1 = decode_sharing(&data)?;
            if v1.r#type() == sharing::v1_frame::FrameType::Cancel {
                return Err(Error::PeerCancelled);
            }
            Ok(v1)
        }
        Incoming::Disconnected | Incoming::PayloadCancelled => Err(Error::PeerCancelled),
        Incoming::FileChunk { .. } => Err(Error::protocol("file data before acceptance")),
    }
}

/// `PairedKeyEncryption` and `PairedKeyResult(UNABLE)` in both directions (SPEC §5.3 step 8).
pub(crate) async fn exchange_paired_keys(ch: &mut Channel) -> Result<(), Error> {
    use sharing::v1_frame::FrameType;
    ch.send_sharing(&frames::paired_key_encryption()).await?;
    let v1 = recv_sharing(ch).await?;
    if v1.r#type() != FrameType::PairedKeyEncryption {
        return Err(Error::protocol(format!("expected PairedKeyEncryption, got {:?}", v1.r#type())));
    }
    ch.send_sharing(&frames::paired_key_result()).await?;
    let v1 = recv_sharing(ch).await?;
    if v1.r#type() != FrameType::PairedKeyResult {
        return Err(Error::protocol(format!("expected PairedKeyResult, got {:?}", v1.r#type())));
    }
    Ok(())
}
