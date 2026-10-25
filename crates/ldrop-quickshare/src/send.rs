// SPDX-License-Identifier: GPL-3.0-or-later
//! Sending side of a Quick Share transfer (SPEC §5.3).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::fs::File;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::channel::{self, Channel, Cmd, Incoming, Role};
use crate::link::{BoxStream, Link, LocalLinks, Medium};
use crate::proto::location::nearby::connections::OfflineFrame;
use crate::proto::location::nearby::connections::payload_transfer_frame::payload_header::PayloadType;
use crate::proto::sharing::nearby::{
    self as sharing, FileMetadata, IntroductionFrame, TextMetadata, connection_response_frame::Status,
};
use crate::{Error, LocalEndpoint, RejectReason, TextKind, finish_session, frames};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone)]
pub struct OutgoingFile {
    pub path: PathBuf,
    /// Name shown to and saved by the receiver.
    pub name: String,
    pub mime: String,
}

#[derive(Debug, Clone)]
pub enum Outgoing {
    Files(Vec<OutgoingFile>),
    Text { text: String, kind: TextKind },
}

#[derive(Debug)]
pub enum OutboundEvent {
    /// Key exchange done; show this PIN next to the receiver's.
    Pin(String),
    Accepted,
    Progress {
        bytes_done: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum SendOutcome {
    Completed,
    Rejected(RejectReason),
}

/// Runs a sending session on a connected `stream`.
pub async fn send<S>(
    stream: S,
    local: &LocalEndpoint,
    link: &Link,
    outgoing: Outgoing,
    events: mpsc::Sender<OutboundEvent>,
    cancel: CancellationToken,
) -> Result<SendOutcome, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut stream: BoxStream = Box::new(stream);
    // Without an upgrader, list Wi-Fi LAN as M2 did; Nearby never upgrades LAN to LAN.
    let links = match &link.upgrader {
        Some(upgrader) => upgrader.local_links(),
        None => LocalLinks { mediums: vec![Medium::WifiLan], ..Default::default() },
    };
    let session = tokio::select! {
        result = channel::client_handshake(&mut stream, local, &links) => result?,
        () = cancel.cancelled() => return Err(Error::Cancelled),
    };
    let _ = events.send(OutboundEvent::Pin(session.pin())).await;
    let mut ch = Channel::spawn(stream, session, Role::Client { local_id: local.id }, link);
    let result = tokio::select! {
        result = run(&mut ch, outgoing, &events) => result,
        () = cancel.cancelled() => Err(Error::Cancelled),
    };
    finish_session(&mut ch, result).await
}

/// What goes out after acceptance, in introduction order.
enum Payload {
    File { id: i64, path: PathBuf, size: u64 },
    Bytes { id: i64, data: Vec<u8> },
}

async fn introduce(outgoing: Outgoing) -> Result<(IntroductionFrame, Vec<Payload>), Error> {
    let mut intro = IntroductionFrame::default();
    let mut payloads = Vec::new();
    match outgoing {
        Outgoing::Files(files) => {
            if files.is_empty() {
                return Err(Error::protocol("nothing to send"));
            }
            for f in files {
                let meta = tokio::fs::metadata(&f.path).await?;
                if !meta.is_file() {
                    return Err(Error::protocol(format!("{} is not a regular file", f.path.display())));
                }
                let id = frames::random_id();
                intro.file_metadata.push(FileMetadata {
                    name: Some(f.name),
                    r#type: Some(file_type(&f.mime, &f.path) as i32),
                    payload_id: Some(id),
                    size: Some(meta.len() as i64),
                    mime_type: Some(f.mime),
                    id: Some(frames::random_id()),
                    ..Default::default()
                });
                payloads.push(Payload::File { id, path: f.path, size: meta.len() });
            }
        }
        Outgoing::Text { text, kind } => {
            let id = frames::random_id();
            intro.text_metadata.push(TextMetadata {
                text_title: Some(text_title(&text, kind)),
                r#type: Some(kind.proto() as i32),
                payload_id: Some(id),
                size: Some(text.len() as i64),
                id: Some(frames::random_id()),
                ..Default::default()
            });
            payloads.push(Payload::Bytes { id, data: text.into_bytes() });
        }
    }
    Ok((intro, payloads))
}

fn file_type(mime: &str, path: &std::path::Path) -> sharing::file_metadata::Type {
    use sharing::file_metadata::Type;
    match mime.split('/').next() {
        Some("image") => Type::Image,
        Some("video") => Type::Video,
        Some("audio") => Type::Audio,
        _ if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("apk")) => Type::AndroidApp,
        _ => Type::Unknown,
    }
}

/// Shown in the receiver's prompt: the host of a URL, or the start of the text.
fn text_title(text: &str, kind: TextKind) -> String {
    if kind == TextKind::Url
        && let Some((_, rest)) = text.split_once("://")
    {
        let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
        if !host.is_empty() {
            return host.to_owned();
        }
    }
    let first = text.lines().next().unwrap_or("").trim();
    match first.char_indices().nth(64) {
        Some((cut, _)) => format!("{}…", &first[..cut]),
        None => first.to_owned(),
    }
}

async fn run(ch: &mut Channel, outgoing: Outgoing, events: &mpsc::Sender<OutboundEvent>) -> Result<SendOutcome, Error> {
    let (intro, payloads) = introduce(outgoing).await?;
    channel::exchange_paired_keys(ch).await?;
    ch.send_sharing(&frames::introduction(intro)).await?;

    // No deadline of our own: the receiver's prompt times out on its side and says TIMED_OUT.
    let status = loop {
        let v1 = channel::recv_sharing(ch).await?;
        if v1.r#type() == sharing::v1_frame::FrameType::Response {
            break v1.connection_response.unwrap_or_default().status();
        }
        debug!(frame = ?v1.r#type(), "ignoring sharing frame while waiting for the answer");
    };
    let reason = match status {
        Status::Accept => None,
        Status::Reject | Status::Unknown => Some(RejectReason::Declined),
        Status::NotEnoughSpace => Some(RejectReason::NotEnoughSpace),
        Status::UnsupportedAttachmentType => Some(RejectReason::Unsupported),
        Status::TimedOut => Some(RejectReason::TimedOut),
    };
    if let Some(reason) = reason {
        let _ = ch.close().await;
        return Ok(SendOutcome::Rejected(reason));
    }
    let _ = events.send(OutboundEvent::Accepted).await;

    let mut done = 0u64;
    let mut last_report = Instant::now();
    let mut report = |done: u64, force: bool| {
        if force || last_report.elapsed() >= PROGRESS_INTERVAL {
            last_report = Instant::now();
            let _ = events.try_send(OutboundEvent::Progress { bytes_done: done });
        }
    };
    for payload in payloads {
        match payload {
            Payload::Bytes { id, data } => {
                let len = data.len() as u64;
                for frame in frames::bytes_payload(id, data) {
                    send_watching(ch, frame).await?;
                }
                done += len;
                report(done, false);
            }
            Payload::File { id, path, size } => {
                let mut file = File::open(&path).await?;
                let total = size as i64;
                let mut offset = 0u64;
                while offset < size {
                    let want = ch.chunk_len().min((size - offset) as usize);
                    let mut body = vec![0u8; want];
                    let n = file.read(&mut body).await?;
                    if n == 0 {
                        return Err(Error::protocol(format!("{} shrank while sending", path.display())));
                    }
                    body.truncate(n);
                    send_watching(ch, frames::payload_chunk(id, PayloadType::File, total, offset as i64, body, false))
                        .await?;
                    offset += n as u64;
                    done += n as u64;
                    report(done, false);
                }
                send_watching(ch, frames::payload_chunk(id, PayloadType::File, total, total, Vec::new(), true)).await?;
            }
        }
    }
    report(done, true);
    ch.close().await?;
    Ok(SendOutcome::Completed)
}

/// Queues `frame` while watching for the receiver cancelling.
async fn send_watching(ch: &mut Channel, frame: OfflineFrame) -> Result<(), Error> {
    loop {
        tokio::select! {
            biased;
            incoming = ch.rx.recv() => match incoming.unwrap_or(Err(Error::Closed))? {
                Incoming::Bytes { data, .. } => {
                    let v1 = channel::decode_sharing(&data)?;
                    if v1.r#type() == sharing::v1_frame::FrameType::Cancel {
                        return Err(Error::PeerCancelled);
                    }
                    debug!(frame = ?v1.r#type(), "ignoring sharing frame during transfer");
                }
                Incoming::Disconnected | Incoming::PayloadCancelled => return Err(Error::PeerCancelled),
                Incoming::FileChunk { .. } => return Err(Error::protocol("receiver sent file data")),
            },
            permit = ch.tx.reserve() => match permit {
                Ok(permit) => {
                    permit.send(Cmd::Frame(Box::new(frame)));
                    return Ok(());
                }
                Err(_) => break,
            },
        }
    }
    Err(ch.writer_error().await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(text_title("https://example.org/a?b", TextKind::Url), "example.org");
        assert_eq!(text_title("not a url", TextKind::Url), "not a url");
        assert_eq!(text_title("  first line \nsecond", TextKind::Text), "first line");
        let long = "ä".repeat(100);
        assert_eq!(text_title(&long, TextKind::Text), format!("{}…", "ä".repeat(64)));
    }

    #[test]
    fn file_types() {
        use sharing::file_metadata::Type;
        assert_eq!(file_type("image/png", "a.png".as_ref()), Type::Image);
        assert_eq!(file_type("application/octet-stream", "x.APK".as_ref()), Type::AndroidApp);
        assert_eq!(file_type("application/pdf", "x.pdf".as_ref()), Type::Unknown);
    }
}
