// SPDX-License-Identifier: GPL-3.0-or-later
//! 4-byte big-endian length prefix around every protobuf on the wire (SPEC §5.3 step 1).

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::Error;

/// Largest frame accepted, as in NearDrop and rquickshare [R42][R43]. Sender chunks are 512 KiB.
pub const MAX_FRAME_LEN: usize = 5 * 1024 * 1024;

/// Reads one frame. A clean EOF before the length prefix is [`Error::Closed`].
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>, Error> {
    let mut prefix = [0u8; 4];
    match r.read_exact(&mut prefix).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(prefix) as usize;
    if len > MAX_FRAME_LEN {
        return Err(Error::protocol(format!("frame of {len} bytes exceeds the limit")));
    }
    let mut frame = vec![0u8; len];
    r.read_exact(&mut frame).await?;
    Ok(frame)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &[u8]) -> Result<(), Error> {
    let len = u32::try_from(frame.len()).ok().filter(|&n| n as usize <= MAX_FRAME_LEN);
    let len = len.ok_or_else(|| Error::protocol("outgoing frame too large"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(frame).await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn round_trip_and_limits() {
        let (mut a, mut b) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            write_frame(&mut a, b"hello").await.unwrap();
            write_frame(&mut a, b"").await.unwrap();
            a.write_all(&(MAX_FRAME_LEN as u32 + 1).to_be_bytes()).await.unwrap();
        });
        assert_eq!(read_frame(&mut b).await.unwrap(), b"hello");
        assert_eq!(read_frame(&mut b).await.unwrap(), b"");
        assert!(matches!(read_frame(&mut b).await, Err(Error::Protocol(_))));
        writer.await.unwrap();
        assert!(matches!(read_frame(&mut b).await, Err(Error::Closed)));
    }

    #[tokio::test]
    async fn truncated_frame_is_an_io_error() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[0, 0, 0, 10, 1, 2]).await.unwrap();
        drop(a);
        assert!(matches!(read_frame(&mut b).await, Err(Error::Io(_))));
    }
}
