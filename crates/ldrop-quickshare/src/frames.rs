// SPDX-License-Identifier: GPL-3.0-or-later
//! Builders for the offline (Nearby Connections) and sharing (Nearby Share) frames we send.

use rand::Rng;

use crate::endpoint::{EndpointId, EndpointInfo};
use crate::link::{LocalLinks, Medium, UpgradePath};
use crate::proto::location::nearby::connections::bandwidth_upgrade_negotiation_frame::{
    ClientIntroduction, ClientIntroductionAck, EventType as BwuEvent, UpgradePathInfo,
};
use crate::proto::location::nearby::connections::connection_response_frame::ResponseStatus;
use crate::proto::location::nearby::connections::os_info::OsType;
use crate::proto::location::nearby::connections::payload_transfer_frame::payload_header::PayloadType;
use crate::proto::location::nearby::connections::payload_transfer_frame::{PacketType, PayloadChunk, PayloadHeader};
use crate::proto::location::nearby::connections::{
    BandwidthUpgradeNegotiationFrame, ConnectionRequestFrame, ConnectionResponseFrame, DisconnectionFrame,
    KeepAliveFrame, MediumMetadata, MediumRole, OfflineFrame, OsInfo, PayloadTransferFrame, V1Frame, offline_frame,
    v1_frame,
};
use crate::proto::sharing::nearby as sharing;

pub(crate) const LAST_CHUNK: i32 = 1;

fn offline(r#type: v1_frame::FrameType, fill: impl FnOnce(&mut V1Frame)) -> OfflineFrame {
    let mut v1 = V1Frame { r#type: Some(r#type as i32), ..Default::default() };
    fill(&mut v1);
    OfflineFrame { version: Some(offline_frame::Version::V1 as i32), v1: Some(v1) }
}

pub(crate) fn connection_request(id: EndpointId, info: &EndpointInfo, links: &LocalLinks) -> OfflineFrame {
    let has = |m: Medium| links.mediums.contains(&m);
    offline(v1_frame::FrameType::ConnectionRequest, |v1| {
        v1.connection_request = Some(ConnectionRequestFrame {
            endpoint_id: Some(id.as_str().to_owned()),
            endpoint_name: info.name.clone(),
            endpoint_info: Some(info.encode()),
            mediums: links.mediums.iter().map(|m| m.to_proto()).collect(),
            medium_metadata: Some(MediumMetadata {
                supports_5_ghz: Some(links.supports_5ghz),
                ip_address: links.ip.map(|ip| ip.octets().to_vec()),
                medium_role: Some(MediumRole {
                    support_wifi_hotspot_client: Some(has(Medium::WifiHotspot)),
                    support_wifi_direct_group_client: Some(has(Medium::WifiDirect)),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    })
}

pub(crate) fn connection_response() -> OfflineFrame {
    // `status` is deprecated in favour of `response`, but NearDrop and rquickshare still send 0.
    #[allow(deprecated)]
    offline(v1_frame::FrameType::ConnectionResponse, |v1| {
        v1.connection_response = Some(ConnectionResponseFrame {
            status: Some(0),
            response: Some(ResponseStatus::Accept as i32),
            os_info: Some(OsInfo { r#type: Some(OsType::Linux as i32) }),
            ..Default::default()
        });
    })
}

pub(crate) fn keep_alive(ack: bool) -> OfflineFrame {
    offline(v1_frame::FrameType::KeepAlive, |v1| {
        v1.keep_alive = Some(KeepAliveFrame { ack: Some(ack), seq_num: None });
    })
}

pub(crate) fn disconnection() -> OfflineFrame {
    offline(v1_frame::FrameType::Disconnection, |v1| v1.disconnection = Some(DisconnectionFrame::default()))
}

fn bwu(event: BwuEvent, fill: impl FnOnce(&mut BandwidthUpgradeNegotiationFrame)) -> OfflineFrame {
    offline(v1_frame::FrameType::BandwidthUpgradeNegotiation, |v1| {
        let mut frame = BandwidthUpgradeNegotiationFrame { event_type: Some(event as i32), ..Default::default() };
        fill(&mut frame);
        v1.bandwidth_upgrade_negotiation = Some(frame);
    })
}

pub(crate) fn upgrade_path_available(path: &UpgradePath) -> OfflineFrame {
    bwu(BwuEvent::UpgradePathAvailable, |f| f.upgrade_path_info = Some(path.to_proto()))
}

pub(crate) fn upgrade_failure(medium: i32) -> OfflineFrame {
    bwu(BwuEvent::UpgradeFailure, |f| {
        f.upgrade_path_info = Some(UpgradePathInfo { medium: Some(medium), ..Default::default() });
    })
}

pub(crate) fn client_introduction(id: EndpointId, disable_encryption: bool) -> OfflineFrame {
    bwu(BwuEvent::ClientIntroduction, |f| {
        f.client_introduction = Some(ClientIntroduction {
            endpoint_id: Some(id.as_str().to_owned()),
            supports_disabling_encryption: Some(disable_encryption),
            last_endpoint_id: None,
        });
    })
}

pub(crate) fn client_introduction_ack() -> OfflineFrame {
    bwu(BwuEvent::ClientIntroductionAck, |f| f.client_introduction_ack = Some(ClientIntroductionAck {}))
}

pub(crate) fn last_write_to_prior_channel() -> OfflineFrame {
    bwu(BwuEvent::LastWriteToPriorChannel, |_| {})
}

pub(crate) fn safe_to_close_prior_channel() -> OfflineFrame {
    bwu(BwuEvent::SafeToClosePriorChannel, |_| {})
}

pub(crate) fn payload_chunk(
    id: i64,
    r#type: PayloadType,
    total: i64,
    offset: i64,
    body: Vec<u8>,
    last: bool,
) -> OfflineFrame {
    offline(v1_frame::FrameType::PayloadTransfer, |v1| {
        v1.payload_transfer = Some(PayloadTransferFrame {
            packet_type: Some(PacketType::Data as i32),
            payload_header: Some(PayloadHeader {
                id: Some(id),
                r#type: Some(r#type as i32),
                total_size: Some(total),
                is_sensitive: Some(false),
                ..Default::default()
            }),
            payload_chunk: Some(PayloadChunk {
                flags: Some(if last { LAST_CHUNK } else { 0 }),
                offset: Some(offset),
                body: Some(body),
                index: None,
            }),
            ..Default::default()
        });
    })
}

/// A complete bytes payload as Android sends it: the whole body, then an empty last chunk [R43].
pub(crate) fn bytes_payload(id: i64, data: Vec<u8>) -> [OfflineFrame; 2] {
    let len = data.len() as i64;
    [
        payload_chunk(id, PayloadType::Bytes, len, 0, data, false),
        payload_chunk(id, PayloadType::Bytes, len, len, Vec::new(), true),
    ]
}

pub(crate) fn random_id() -> i64 {
    rand::rng().random()
}

fn sharing(r#type: sharing::v1_frame::FrameType, fill: impl FnOnce(&mut sharing::V1Frame)) -> sharing::Frame {
    let mut v1 = sharing::V1Frame { r#type: Some(r#type as i32), ..Default::default() };
    fill(&mut v1);
    sharing::Frame { version: Some(sharing::frame::Version::V1 as i32), v1: Some(v1) }
}

/// Random contents are accepted: real values need Google account certificates [R43].
pub(crate) fn paired_key_encryption() -> sharing::Frame {
    sharing(sharing::v1_frame::FrameType::PairedKeyEncryption, |v1| {
        let mut rng = rand::rng();
        v1.paired_key_encryption = Some(sharing::PairedKeyEncryptionFrame {
            secret_id_hash: Some(rng.random::<[u8; 6]>().to_vec()),
            signed_data: Some((0..72).map(|_| rng.random::<u8>()).collect()),
            ..Default::default()
        });
    })
}

pub(crate) fn paired_key_result() -> sharing::Frame {
    sharing(sharing::v1_frame::FrameType::PairedKeyResult, |v1| {
        v1.paired_key_result = Some(sharing::PairedKeyResultFrame {
            status: Some(sharing::paired_key_result_frame::Status::Unable as i32),
            ..Default::default()
        });
    })
}

pub(crate) fn introduction(intro: sharing::IntroductionFrame) -> sharing::Frame {
    sharing(sharing::v1_frame::FrameType::Introduction, |v1| v1.introduction = Some(intro))
}

pub(crate) fn response(status: sharing::connection_response_frame::Status) -> sharing::Frame {
    sharing(sharing::v1_frame::FrameType::Response, |v1| {
        v1.connection_response =
            Some(sharing::ConnectionResponseFrame { status: Some(status as i32), ..Default::default() });
    })
}

pub(crate) fn cancel() -> sharing::Frame {
    sharing(sharing::v1_frame::FrameType::Cancel, |_| {})
}
