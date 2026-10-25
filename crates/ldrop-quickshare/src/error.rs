// SPDX-License-Identifier: GPL-3.0-or-later

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("message failed authentication")]
    Integrity,
    #[error("peer stopped responding")]
    Timeout,
    #[error("connection closed by peer")]
    Closed,
    #[error("cancelled")]
    Cancelled,
    #[error("cancelled by peer")]
    PeerCancelled,
}

impl Error {
    pub fn protocol(message: impl Into<String>) -> Self {
        Error::Protocol(message.into())
    }
}
