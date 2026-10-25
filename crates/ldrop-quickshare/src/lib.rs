// SPDX-License-Identifier: GPL-3.0-or-later
//! Quick Share (Nearby Share) protocol (SPEC §5): endpoint identity, UKEY2 and SecureMessage,
//! and complete send/receive sessions over any byte stream.
//!
//! No sockets here: the daemon accepts or connects TCP and hands the stream in, and tests use
//! in-memory pipes.

pub mod bluetooth;
pub mod crypto;
pub mod endpoint;
pub mod framing;
pub mod link;
pub mod receive;
pub mod send;

mod channel;
mod error;
mod frames;

pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

use rand::Rng;

pub use channel::RemoteEndpoint;
pub use endpoint::{DeviceType, EndpointId, EndpointInfo};
pub use error::Error;
pub use link::{Link, Medium};
use proto::sharing::nearby::{connection_response_frame::Status, text_metadata};

/// 16-bit service UUID of the BLE wake-up advertisement (SPEC §5.2) [R43].
pub const BLE_WAKE_SERVICE_UUID: u16 = 0xFE2C;
const BLE_WAKE_PREFIX: [u8; 14] = [0xfc, 0x12, 0x8e, 0x01, 0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Service data for the wake-up advertisement: a fixed prefix and 10 random bytes [R43].
pub fn ble_wake_service_data() -> Vec<u8> {
    let mut data = BLE_WAKE_PREFIX.to_vec();
    data.extend(rand::rng().random::<[u8; 10]>());
    data
}

/// How this device presents itself in connection requests and mDNS.
#[derive(Debug, Clone)]
pub struct LocalEndpoint {
    pub id: EndpointId,
    pub info: EndpointInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextKind {
    Text,
    Url,
    Address,
    PhoneNumber,
}

impl TextKind {
    fn proto(self) -> text_metadata::Type {
        match self {
            TextKind::Text => text_metadata::Type::Text,
            TextKind::Url => text_metadata::Type::Url,
            TextKind::Address => text_metadata::Type::Address,
            TextKind::PhoneNumber => text_metadata::Type::PhoneNumber,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiSecurity {
    Unknown,
    Open,
    WpaPsk,
    Wep,
    Sae,
}

/// Why a receiver did not accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    Declined,
    NotEnoughSpace,
    Unsupported,
    TimedOut,
}

impl RejectReason {
    fn status(self) -> Status {
        match self {
            RejectReason::Declined => Status::Reject,
            RejectReason::NotEnoughSpace => Status::NotEnoughSpace,
            RejectReason::Unsupported => Status::UnsupportedAttachmentType,
            RejectReason::TimedOut => Status::TimedOut,
        }
    }
}

/// Ends the channel to match how the session ended.
async fn finish_session<T>(ch: &mut channel::Channel, result: Result<T, Error>) -> Result<T, Error> {
    match &result {
        // Successful paths close on their own.
        Ok(_) => {}
        Err(Error::PeerCancelled) => {
            let _ = ch.close().await;
        }
        // Nothing to say to a peer that is gone.
        Err(Error::Closed | Error::Timeout) => {}
        Err(_) => ch.abort().await,
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wake_data_fits_a_legacy_advertisement() {
        let data = ble_wake_service_data();
        assert_eq!(data.len(), 24);
        assert_eq!(&data[..5], &[0xfc, 0x12, 0x8e, 0x01, 0x42]);
        // Flags (3) + service data AD header (2) + UUID (2) + data must fit 31 bytes.
        assert!(3 + 2 + 2 + data.len() <= 31);
    }
}
