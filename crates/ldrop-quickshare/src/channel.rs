// SPDX-License-Identifier: GPL-3.0-or-later
//! The connection below the sharing layer: plaintext handshake (SPEC §5.3 steps 2–5), then an
//! encrypted channel with a reader and a writer task.
//!
//! The writer task owns the write half and the [`Sealer`], so dropping any future in a
//! session never leaves a half-written frame. It also sends a keep-alive every 10 s
//! (§5.3 step 10). The reader task decrypts, answers keep-alives, reassembles bytes
//! payloads and forwards everything else.

use std::collections::HashMap;
use std::time::Duration;

use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, timeout};
use tracing::{debug, trace};

use crate::crypto::{ClientHandshake, HandshakeError, Opener, Sealer, ServerHandshake, Session};
use crate::endpoint::{EndpointId, EndpointInfo};
use crate::framing::{MAX_FRAME_LEN, read_frame, write_frame};
use crate::proto::location::nearby::connections::connection_response_frame::ResponseStatus;
use crate::proto::location::nearby::connections::payload_transfer_frame::PacketType;
use crate::proto::location::nearby::connections::payload_transfer_frame::control_message::EventType;
use crate::proto::location::nearby::connections::payload_transfer_frame::payload_header::PayloadType;
use crate::proto::location::nearby::connections::{OfflineFrame, V1Frame, v1_frame};
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

/// The other side, as its connection request describes it.
#[derive(Debug, Clone)]
pub struct RemoteEndpoint {
    pub id: EndpointId,
    pub info: EndpointInfo,
    /// `endpoint_name` from the request; hidden devices carry their name only here.
    pub endpoint_name: Option<String>,
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

async fn read_plain<S: AsyncRead + Unpin>(stream: &mut S) -> Result<Vec<u8>, Error> {
    timeout(HANDSHAKE_TIMEOUT, read_frame(stream)).await.map_err(|_| Error::Timeout)?
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

/// Receiving side, up to and including both plaintext connection responses.
pub(crate) async fn server_handshake<S>(stream: &mut S) -> Result<(RemoteEndpoint, Session), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let v1 = decode_offline(&read_plain(stream).await?, v1_frame::FrameType::ConnectionRequest)?;
    let request = v1.connection_request.ok_or_else(|| Error::protocol("connection request without body"))?;
    let id = EndpointId::parse(request.endpoint_id()).ok_or_else(|| Error::protocol("bad endpoint id"))?;
    let info = EndpointInfo::decode(request.endpoint_info())?;
    let remote = RemoteEndpoint { id, info, endpoint_name: request.endpoint_name.filter(|n| !n.is_empty()) };

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
pub(crate) async fn client_handshake<S>(stream: &mut S, local: &LocalEndpoint) -> Result<Session, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &frames::connection_request(local.id, &local.info).encode_to_vec()).await?;
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

/// The encrypted channel. Dropping it aborts both tasks; call [`Channel::close`] to end politely.
pub(crate) struct Channel {
    pub(crate) tx: mpsc::Sender<Cmd>,
    pub(crate) rx: mpsc::Receiver<Result<Incoming, Error>>,
    writer: Option<JoinHandle<Result<(), Error>>>,
    reader: JoinHandle<()>,
}

impl Channel {
    pub(crate) fn spawn<S>(stream: S, sealer: Sealer, opener: Opener) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (r, w) = tokio::io::split(stream);
        // Small queues: file chunks are up to 512 KiB each and backpressure must reach the file reader.
        let (tx, writer_rx) = mpsc::channel(4);
        let (reader_tx, rx) = mpsc::channel(4);
        let writer = tokio::spawn(writer_task(w, sealer, writer_rx));
        let reader = tokio::spawn(reader_task(r, opener, reader_tx, tx.clone()));
        Channel { tx, rx, writer: Some(writer), reader }
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
        self.rx.recv().await.unwrap_or(Err(Error::Closed))
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

    /// Sends `Disconnection`, waits for queued frames to go out, then drains the read side
    /// briefly so closing does not reset data the peer has not read yet.
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
        let _ = timeout(CLOSE_TIMEOUT, async { while self.rx.recv().await.is_some() {} }).await;
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

async fn writer_task<S: AsyncWrite>(
    mut w: WriteHalf<S>,
    mut sealer: Sealer,
    mut rx: mpsc::Receiver<Cmd>,
) -> Result<(), Error> {
    let mut keepalive = tokio::time::interval_at(tokio::time::Instant::now() + KEEPALIVE_INTERVAL, KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let frame = tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(Cmd::Frame(frame)) => *frame,
                Some(Cmd::Shutdown) | None => break,
            },
            _ = keepalive.tick() => frames::keep_alive(false),
        };
        let sealed = sealer.seal(&frame);
        timeout(WRITE_TIMEOUT, write_frame(&mut w, &sealed)).await.map_err(|_| Error::Timeout)??;
    }
    timeout(CLOSE_TIMEOUT, w.shutdown()).await.map_err(|_| Error::Timeout)??;
    Ok(())
}

async fn reader_task<S: AsyncRead>(
    mut r: ReadHalf<S>,
    mut opener: Opener,
    out: mpsc::Sender<Result<Incoming, Error>>,
    writer: mpsc::Sender<Cmd>,
) {
    let mut buffers = HashMap::new();
    let mut disconnected = false;
    loop {
        let result = match timeout(PEER_SILENCE, read_frame(&mut r)).await {
            Err(_) => Err(Error::Timeout),
            Ok(Err(e)) => Err(e),
            Ok(Ok(raw)) if disconnected => {
                trace!(len = raw.len(), "frame after disconnection ignored");
                continue;
            }
            Ok(Ok(raw)) => opener.open(&raw).and_then(|frame| demux(frame, &mut buffers, &writer)),
        };
        match result {
            Ok(None) => {}
            Ok(Some(incoming)) => {
                // Keep reading after a disconnection so the socket drains to EOF.
                disconnected = matches!(incoming, Incoming::Disconnected);
                if out.send(Ok(incoming)).await.is_err() {
                    return;
                }
            }
            Err(e) => {
                if !disconnected {
                    let _ = out.send(Err(e)).await;
                }
                return;
            }
        }
    }
}

fn demux(
    frame: OfflineFrame,
    buffers: &mut HashMap<i64, Vec<u8>>,
    writer: &mpsc::Sender<Cmd>,
) -> Result<Option<Incoming>, Error> {
    let v1 = frame.v1.ok_or_else(|| Error::protocol("offline frame without v1"))?;
    match v1.r#type() {
        v1_frame::FrameType::KeepAlive => {
            if !v1.keep_alive.and_then(|k| k.ack).unwrap_or(false) {
                // Never block the reader on a full writer queue: the data frames in it count as activity.
                let _ = writer.try_send(Cmd::Frame(Box::new(frames::keep_alive(true))));
            }
            Ok(None)
        }
        v1_frame::FrameType::Disconnection => Ok(Some(Incoming::Disconnected)),
        v1_frame::FrameType::PayloadTransfer => {
            let transfer = v1.payload_transfer.ok_or_else(|| Error::protocol("payload transfer without body"))?;
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
                PayloadType::File => Ok(Some(Incoming::FileChunk {
                    id,
                    offset: chunk.offset(),
                    body: chunk.body.unwrap_or_default(),
                    last,
                })),
                other => Err(Error::protocol(format!("unsupported payload type {other:?}"))),
            }
        }
        other => {
            debug!(frame = ?other, "ignoring offline frame");
            Ok(None)
        }
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
