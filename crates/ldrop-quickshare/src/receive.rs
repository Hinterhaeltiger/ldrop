// SPDX-License-Identifier: GPL-3.0-or-later
//! Receiving side of a Quick Share transfer (SPEC §5.3, §9, §14).
//!
//! [`receive`] runs the whole session on an accepted connection. It reports the offer
//! through an [`InboundEvent::Offer`] and writes nothing before the answer is
//! [`Answer::Accept`]. File `i` of [`Offer::files`] is written to `<staging>/<i>`; naming,
//! moving and cleanup of the staging directory belong to the caller.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use prost::Message;
use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::channel::{self, Channel, HANDSHAKE_TIMEOUT, Incoming, RemoteEndpoint, Role};
use crate::link::{BoxStream, Link};
use crate::proto::sharing::nearby::{self as sharing, IntroductionFrame, connection_response_frame::Status};
use crate::{Error, RejectReason, TextKind, WifiSecurity, finish_session, frames};

/// Upper bound on items in one introduction (SPEC §14: cap counts).
const MAX_ITEMS: usize = 10_000;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Transfers at least this big are moved to a faster medium if there is one, as Windows
/// Quick Share does (`kAttachmentsSizeThresholdOverHighQualityMedium`) [R44].
const UPGRADE_THRESHOLD: u64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct Offer {
    pub peer: RemoteEndpoint,
    pub pin: String,
    pub files: Vec<OfferedFile>,
    pub texts: Vec<OfferedText>,
    pub wifi: Vec<OfferedWifi>,
}

impl Offer {
    /// Bytes that will arrive: files plus text payloads.
    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).chain(self.texts.iter().map(|t| t.size)).sum()
    }
}

#[derive(Debug, Clone)]
pub struct OfferedFile {
    /// As sent by the peer: not sanitized.
    pub name: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct OfferedText {
    pub title: String,
    pub kind: TextKind,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct OfferedWifi {
    pub ssid: String,
    pub security: WifiSecurity,
}

#[derive(Debug)]
pub enum Answer {
    Accept { staging: PathBuf },
    Reject(RejectReason),
}

#[derive(Debug)]
pub enum InboundEvent {
    /// Reply on the sender. Dropping it cancels the transfer.
    Offer(Box<Offer>, oneshot::Sender<Answer>),
    Progress {
        bytes_done: u64,
    },
}

/// Payloads that do not land in files, indexed like [`Offer::texts`] and [`Offer::wifi`].
#[derive(Debug, Default)]
pub struct Received {
    pub texts: Vec<String>,
    pub wifi: Vec<WifiCredentials>,
}

#[derive(Debug, Clone)]
pub struct WifiCredentials {
    pub password: String,
    pub hidden: bool,
}

#[derive(Debug)]
pub enum ReceiveOutcome {
    Completed(Received),
    Rejected(RejectReason),
}

/// Runs a receiving session on `stream`, from the connection request to the disconnection.
pub async fn receive<S>(
    stream: S,
    link: &Link,
    events: mpsc::Sender<InboundEvent>,
    cancel: CancellationToken,
) -> Result<ReceiveOutcome, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut stream: BoxStream = Box::new(stream);
    let (peer, session) = tokio::select! {
        result = channel::server_handshake(&mut stream) => result?,
        () = cancel.cancelled() => return Err(Error::Cancelled),
    };
    let pin = session.pin();
    let role = Role::Server { remote_id: peer.id, peer: peer.links.clone() };
    let mut ch = Channel::spawn(stream, session, role, link);
    let result = tokio::select! {
        result = run(&mut ch, peer, pin, &events) => result,
        () = cancel.cancelled() => Err(Error::Cancelled),
    };
    finish_session(&mut ch, result).await
}

/// Where each payload ID goes.
struct Plan {
    offer_files: Vec<OfferedFile>,
    file_ids: Vec<i64>,
    texts: Vec<(i64, OfferedText)>,
    wifi: Vec<(i64, OfferedWifi)>,
}

fn plan(intro: IntroductionFrame) -> Result<Plan, &'static str> {
    use sharing::text_metadata::Type as T;
    use sharing::wifi_credentials_metadata::SecurityType as W;

    let count = intro.file_metadata.len() + intro.text_metadata.len() + intro.wifi_credentials_metadata.len();
    if count == 0 {
        return Err("nothing offered");
    }
    if count > MAX_ITEMS {
        return Err("too many items");
    }
    let mut ids = HashSet::new();
    let mut unique = |id: i64| if ids.insert(id) { Ok(()) } else { Err("duplicate payload id") };
    let mut plan = Plan { offer_files: Vec::new(), file_ids: Vec::new(), texts: Vec::new(), wifi: Vec::new() };
    for f in intro.file_metadata {
        unique(f.payload_id())?;
        let size = u64::try_from(f.size()).map_err(|_| "negative file size")?;
        plan.file_ids.push(f.payload_id());
        plan.offer_files.push(OfferedFile { name: f.name().to_owned(), mime: f.mime_type().to_owned(), size });
    }
    for t in intro.text_metadata {
        unique(t.payload_id())?;
        let kind = match t.r#type() {
            T::Text => TextKind::Text,
            T::Url => TextKind::Url,
            T::Address => TextKind::Address,
            T::PhoneNumber => TextKind::PhoneNumber,
            T::Unknown => return Err("unknown text type"),
        };
        let size = u64::try_from(t.size()).map_err(|_| "negative text size")?;
        plan.texts.push((t.payload_id(), OfferedText { title: t.text_title().to_owned(), kind, size }));
    }
    for w in intro.wifi_credentials_metadata {
        unique(w.payload_id())?;
        let security = match w.security_type() {
            W::Open => WifiSecurity::Open,
            W::WpaPsk => WifiSecurity::WpaPsk,
            W::Wep => WifiSecurity::Wep,
            W::Sae => WifiSecurity::Sae,
            W::UnknownSecurityType => WifiSecurity::Unknown,
        };
        plan.wifi.push((w.payload_id(), OfferedWifi { ssid: w.ssid().to_owned(), security }));
    }
    Ok(plan)
}

async fn run(
    ch: &mut Channel,
    peer: RemoteEndpoint,
    pin: String,
    events: &mpsc::Sender<InboundEvent>,
) -> Result<ReceiveOutcome, Error> {
    channel::exchange_paired_keys(ch).await?;
    let v1 = timeout(HANDSHAKE_TIMEOUT, channel::recv_sharing(ch)).await.map_err(|_| Error::Timeout)??;
    if v1.r#type() != sharing::v1_frame::FrameType::Introduction {
        return Err(Error::protocol(format!("expected Introduction, got {:?}", v1.r#type())));
    }
    let plan = match plan(v1.introduction.unwrap_or_default()) {
        Ok(plan) => plan,
        Err(why) => {
            ch.send_sharing(&frames::response(Status::UnsupportedAttachmentType)).await?;
            let _ = ch.close().await;
            return Err(Error::protocol(format!("introduction rejected: {why}")));
        }
    };
    let offer = Offer {
        peer,
        pin,
        files: plan.offer_files.clone(),
        texts: plan.texts.iter().map(|(_, t)| t.clone()).collect(),
        wifi: plan.wifi.iter().map(|(_, w)| w.clone()).collect(),
    };

    let total = offer.total_size();
    let (reply, mut answer) = oneshot::channel();
    events.send(InboundEvent::Offer(Box::new(offer), reply)).await.map_err(|_| Error::Cancelled)?;
    let answer = loop {
        tokio::select! {
            answer = &mut answer => break answer.map_err(|_| Error::Cancelled)?,
            incoming = ch.recv() => match incoming? {
                Incoming::Bytes { data, .. } => {
                    let v1 = channel::decode_sharing(&data)?;
                    if v1.r#type() == sharing::v1_frame::FrameType::Cancel {
                        return Err(Error::PeerCancelled);
                    }
                    debug!(frame = ?v1.r#type(), "ignoring sharing frame while waiting for consent");
                }
                Incoming::Disconnected | Incoming::PayloadCancelled => return Err(Error::PeerCancelled),
                Incoming::FileChunk { .. } => return Err(Error::protocol("file data before acceptance")),
            },
        }
    };
    let staging = match answer {
        Answer::Accept { staging } => staging,
        Answer::Reject(reason) => {
            ch.send_sharing(&frames::response(reason.status())).await?;
            let _ = ch.close().await;
            return Ok(ReceiveOutcome::Rejected(reason));
        }
    };
    ch.send_sharing(&frames::response(Status::Accept)).await?;
    if total >= UPGRADE_THRESHOLD {
        ch.request_upgrade();
    }
    let received = transfer(ch, plan, &staging, events).await?;
    let _ = ch.close().await;
    Ok(ReceiveOutcome::Completed(received))
}

struct Slot {
    path: PathBuf,
    size: u64,
    written: u64,
    file: Option<File>,
}

async fn transfer(
    ch: &mut Channel,
    plan: Plan,
    staging: &Path,
    events: &mpsc::Sender<InboundEvent>,
) -> Result<Received, Error> {
    let mut files: HashMap<i64, Slot> = plan
        .file_ids
        .iter()
        .zip(&plan.offer_files)
        .enumerate()
        .map(|(i, (&id, f))| (id, Slot { path: staging.join(i.to_string()), size: f.size, written: 0, file: None }))
        .collect();
    let mut texts: HashMap<i64, usize> = plan.texts.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    let mut wifi: HashMap<i64, usize> = plan.wifi.iter().enumerate().map(|(i, (id, _))| (*id, i)).collect();
    let mut received = Received {
        texts: vec![String::new(); plan.texts.len()],
        wifi: vec![WifiCredentials { password: String::new(), hidden: false }; plan.wifi.len()],
    };
    let mut remaining = files.len() + texts.len() + wifi.len();
    let mut progress = Progress::new(events);

    while remaining > 0 {
        match ch.recv().await? {
            Incoming::FileChunk { id, offset, body, last } => {
                let slot = files.get_mut(&id).ok_or_else(|| Error::protocol("unknown or finished file payload"))?;
                if u64::try_from(offset).ok() != Some(slot.written) {
                    return Err(Error::protocol("file chunk out of order"));
                }
                let len = body.len() as u64;
                if slot.written + len > slot.size {
                    return Err(Error::protocol("file exceeds its announced size"));
                }
                if slot.file.is_none() {
                    slot.file = Some(File::options().write(true).create_new(true).open(&slot.path).await?);
                }
                let file = slot.file.as_mut().expect("opened above");
                if len > 0 {
                    file.write_all(&body).await?;
                    slot.written += len;
                    progress.add(len);
                }
                if last {
                    if slot.written != slot.size {
                        return Err(Error::protocol("file ended before its announced size"));
                    }
                    file.sync_all().await?;
                    files.remove(&id);
                    remaining -= 1;
                }
            }
            Incoming::Bytes { id, data } => {
                if let Some(i) = texts.remove(&id) {
                    progress.add(data.len() as u64);
                    received.texts[i] = String::from_utf8_lossy(&data).into_owned();
                    remaining -= 1;
                } else if let Some(i) = wifi.remove(&id) {
                    let creds = sharing::WifiCredentials::decode(data.as_slice())
                        .map_err(|_| Error::protocol("Wi-Fi credentials do not parse"))?;
                    received.wifi[i] =
                        WifiCredentials { password: creds.password().to_owned(), hidden: creds.hidden_ssid() };
                    remaining -= 1;
                } else {
                    let v1 = channel::decode_sharing(&data)?;
                    if v1.r#type() == sharing::v1_frame::FrameType::Cancel {
                        return Err(Error::PeerCancelled);
                    }
                    debug!(frame = ?v1.r#type(), "ignoring sharing frame during transfer");
                }
            }
            Incoming::Disconnected | Incoming::PayloadCancelled => return Err(Error::PeerCancelled),
        }
    }
    progress.flush();
    Ok(received)
}

/// Throttled progress reports; dropped when the consumer lags, since each carries the total.
struct Progress<'a> {
    events: &'a mpsc::Sender<InboundEvent>,
    done: u64,
    last: Instant,
}

impl<'a> Progress<'a> {
    fn new(events: &'a mpsc::Sender<InboundEvent>) -> Self {
        Progress { events, done: 0, last: Instant::now() }
    }

    fn add(&mut self, bytes: u64) {
        self.done += bytes;
        if self.last.elapsed() >= PROGRESS_INTERVAL {
            self.flush();
        }
    }

    fn flush(&mut self) {
        self.last = Instant::now();
        let _ = self.events.try_send(InboundEvent::Progress { bytes_done: self.done });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::sharing::nearby::{FileMetadata, TextMetadata};

    fn file(id: i64, size: i64) -> FileMetadata {
        FileMetadata { payload_id: Some(id), size: Some(size), name: Some("a.txt".into()), ..Default::default() }
    }

    #[test]
    fn plan_validates_introductions() {
        assert!(plan(IntroductionFrame::default()).is_err());
        let dup = IntroductionFrame { file_metadata: vec![file(1, 1), file(1, 2)], ..Default::default() };
        assert_eq!(plan(dup).err(), Some("duplicate payload id"));
        let neg = IntroductionFrame { file_metadata: vec![file(1, -1)], ..Default::default() };
        assert_eq!(plan(neg).err(), Some("negative file size"));
        let unknown_text = IntroductionFrame {
            text_metadata: vec![TextMetadata { payload_id: Some(2), ..Default::default() }],
            ..Default::default()
        };
        assert_eq!(plan(unknown_text).err(), Some("unknown text type"));
        let many = IntroductionFrame {
            file_metadata: (0..=MAX_ITEMS as i64).map(|i| file(i, 0)).collect(),
            ..Default::default()
        };
        assert_eq!(plan(many).err(), Some("too many items"));

        let ok = IntroductionFrame {
            file_metadata: vec![file(1, 10)],
            text_metadata: vec![TextMetadata {
                payload_id: Some(2),
                r#type: Some(sharing::text_metadata::Type::Url as i32),
                size: Some(5),
                ..Default::default()
            }],
            ..Default::default()
        };
        let plan = plan(ok).unwrap();
        assert_eq!(plan.offer_files[0].mime, "application/octet-stream");
        assert_eq!(plan.texts[0].1.kind, TextKind::Url);
    }
}
