//! OfflineFrame builders and the payload layer.

use std::collections::HashMap;

use prost::Message;

use crate::proto::connections as nc;
use crate::proto::sharing as sh;

pub fn offline(v1: nc::V1Frame) -> nc::OfflineFrame {
    nc::OfflineFrame { version: Some(nc::offline_frame::Version::V1 as i32), v1: Some(v1) }
}

fn v1(t: nc::v1_frame::FrameType) -> nc::V1Frame {
    nc::V1Frame { r#type: Some(t as i32), ..Default::default() }
}

pub fn frame_type(f: &nc::OfflineFrame) -> nc::v1_frame::FrameType {
    f.v1
        .as_ref()
        .and_then(|v| v.r#type)
        .and_then(|t| nc::v1_frame::FrameType::try_from(t).ok())
        .unwrap_or(nc::v1_frame::FrameType::UnknownFrameType)
}

/// `wifi_direct_device_name`: our Wi-Fi Direct (P2P) device name. When set, an
/// Android GO admits us by device name and sends back its name + PIN.
pub fn connection_response_accept(os: nc::os_info::OsType, wifi_direct_device_name: Option<&str>) -> nc::OfflineFrame {
    let mut f = v1(nc::v1_frame::FrameType::ConnectionResponse);
    f.connection_response = Some(nc::ConnectionResponseFrame {
        response: Some(nc::connection_response_frame::ResponseStatus::Accept as i32),
        os_info: Some(nc::OsInfo { r#type: Some(os as i32) }),
        wifi_direct_device_name: wifi_direct_device_name.map(str::to_owned),
        ..Default::default()
    });
    offline(f)
}

/// Role switch: ask the peer to *host* one of `mediums` for us (we join as client).
/// `client_channels`: frequencies (MHz) we can use as a Wi-Fi Direct client; a hint for the
/// host's GO channel choice. Empty = don't send.
pub fn bwu_path_request(
    mediums: &[nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium],
    client_channels: &[i32],
) -> nc::OfflineFrame {
    bwu_path_request_from(mediums, client_channels, None)
}

/// As `bwu_path_request`, with our IPv4 address (the host picks the interface that reaches it).
pub fn bwu_path_request_from(
    mediums: &[nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info::Medium],
    client_channels: &[i32],
    ip: Option<[u8; 4]>,
) -> nc::OfflineFrame {
    use nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info as upi;
    let role = nc::MediumRole {
        support_wifi_direct_group_owner: Some(false),
        support_wifi_direct_group_client: Some(true),
        support_wifi_hotspot_host: Some(false),
        support_wifi_hotspot_client: Some(false),
        ..Default::default()
    };
    bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(BwuEvent::UpgradePathRequest as i32),
        upgrade_path_info: Some(Upi {
            medium: mediums.first().map(|m| *m as i32),
            upgrade_path_request: Some(upi::UpgradePathRequest {
                mediums: mediums.iter().map(|m| *m as i32).collect(),
                medium_meta_data: Some(nc::MediumMetadata {
                    supports_5_ghz: Some(true),
                    medium_role: Some(role),
                    ip_address: ip.map(|b| b.to_vec()),
                    available_channels: (!client_channels.is_empty())
                        .then(|| nc::AvailableChannels { channels: client_channels.to_vec() }),
                    wifi_direct_cli_usable_channels: (!client_channels.is_empty())
                        .then(|| nc::WifiDirectCliUsableChannels { channels: client_channels.to_vec() }),
                    supported_wifi_direct_auth_types: vec![
                        nc::medium_metadata::WifiDirectAuthType::WifiDirectWithDeviceName as i32,
                    ],
                    ..Default::default()
                }),
            }),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// First (plaintext) frame a BWU client sends on the new channel.
pub fn bwu_client_introduction(endpoint_id: &str) -> nc::OfflineFrame {
    bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(BwuEvent::ClientIntroduction as i32),
        client_introduction: Some(nc::bandwidth_upgrade_negotiation_frame::ClientIntroduction {
            endpoint_id: Some(endpoint_id.to_owned()),
            supports_disabling_encryption: Some(false),
            last_endpoint_id: None,
        }),
        ..Default::default()
    })
}

pub fn keep_alive(ack: bool, seq: u32) -> nc::OfflineFrame {
    let mut f = v1(nc::v1_frame::FrameType::KeepAlive);
    f.keep_alive = Some(nc::KeepAliveFrame { ack: Some(ack), seq_num: Some(seq) });
    offline(f)
}

pub fn disconnection() -> nc::OfflineFrame {
    let mut f = v1(nc::v1_frame::FrameType::Disconnection);
    f.disconnection = Some(nc::DisconnectionFrame::default());
    offline(f)
}

pub fn bwu(frame: nc::BandwidthUpgradeNegotiationFrame) -> nc::OfflineFrame {
    let mut f = v1(nc::v1_frame::FrameType::BandwidthUpgradeNegotiation);
    f.bandwidth_upgrade_negotiation = Some(frame);
    offline(f)
}

pub type BwuEvent = nc::bandwidth_upgrade_negotiation_frame::EventType;
type Upi = nc::bandwidth_upgrade_negotiation_frame::UpgradePathInfo;

/// A bare BWU event frame (LAST_WRITE, SAFE_TO_CLOSE, CLIENT_INTRODUCTION_ACK).
pub fn bwu_event(event: BwuEvent) -> nc::OfflineFrame {
    bwu(nc::BandwidthUpgradeNegotiationFrame { event_type: Some(event as i32), ..Default::default() })
}

fn upgrade_path(info: Upi) -> nc::OfflineFrame {
    bwu(nc::BandwidthUpgradeNegotiationFrame {
        event_type: Some(BwuEvent::UpgradePathAvailable as i32),
        upgrade_path_info: Some(Upi {
            supports_disabling_encryption: Some(false),
            supports_client_introduction_ack: Some(true),
            ..info
        }),
        ..Default::default()
    })
}

/// Ask the peer to move to a TCP socket on the LAN (`ip` is IPv4, network order).
pub fn bwu_wifi_lan(ip: [u8; 4], port: u16) -> nc::OfflineFrame {
    use nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info as upi;
    upgrade_path(Upi {
        medium: Some(upi::Medium::WifiLan as i32),
        wifi_lan_socket: Some(upi::WifiLanSocket {
            ip_address: Some(ip.to_vec()),
            wifi_port: Some(port as i32),
            address_candidates: vec![],
        }),
        ..Default::default()
    })
}

/// Ask the peer to join a soft-AP / Wi-Fi Direct group we host, then connect
/// to `gateway:port`. `as_direct` picks the WIFI_DIRECT medium, else WIFI_HOTSPOT.
pub fn bwu_hosted_network(
    as_direct: bool,
    ssid: &str,
    password: &str,
    gateway: &str,
    port: u16,
    frequency: i32,
    device_name: Option<&str>,
) -> nc::OfflineFrame {
    use nc::bandwidth_upgrade_negotiation_frame::upgrade_path_info as upi;
    if as_direct {
        upgrade_path(Upi {
            medium: Some(upi::Medium::WifiDirect as i32),
            wifi_direct_credentials: Some(upi::WifiDirectCredentials {
                ssid: Some(ssid.into()),
                password: Some(password.into()),
                port: Some(port as i32),
                frequency: Some(frequency),
                gateway: Some(gateway.into()),
                // the host's P2P device name: the joiner finds (and later reconnects to) it by name
                device_name: device_name.map(Into::into),
                ..Default::default()
            }),
            ..Default::default()
        })
    } else {
        upgrade_path(Upi {
            medium: Some(upi::Medium::WifiHotspot as i32),
            wifi_hotspot_credentials: Some(upi::WifiHotspotCredentials {
                ssid: Some(ssid.into()),
                password: Some(password.into()),
                port: Some(port as i32),
                gateway: Some(gateway.into()),
                frequency: Some(frequency),
                address_candidates: vec![],
            }),
            ..Default::default()
        })
    }
}

/// One chunk of a FILE payload. The last chunk (`last`) may be empty.
pub fn file_chunk(id: i64, total_size: i64, file_name: &str, offset: i64, body: Vec<u8>, last: bool) -> nc::OfflineFrame {
    use nc::payload_transfer_frame as ptf;
    let mut f = v1(nc::v1_frame::FrameType::PayloadTransfer);
    f.payload_transfer = Some(nc::PayloadTransferFrame {
        packet_type: Some(ptf::PacketType::Data as i32),
        payload_header: Some(ptf::PayloadHeader {
            id: Some(id),
            r#type: Some(ptf::payload_header::PayloadType::File as i32),
            total_size: Some(total_size),
            is_sensitive: Some(false),
            file_name: Some(file_name.to_owned()),
            ..Default::default()
        }),
        payload_chunk: Some(ptf::PayloadChunk {
            flags: Some(if last { ptf::payload_chunk::Flags::LastChunk as i32 } else { 0 }),
            offset: Some(offset),
            body: Some(body),
            index: None,
        }),
        control_message: None,
    });
    offline(f)
}

/// Client side of connection setup: our ConnectionRequest.
pub fn connection_request(
    endpoint_id: &str,
    endpoint_info: &[u8],
    mediums: &[nc::connection_request_frame::Medium],
    medium_metadata: nc::MediumMetadata,
) -> nc::OfflineFrame {
    use rand::Rng;
    let mut f = v1(nc::v1_frame::FrameType::ConnectionRequest);
    f.connection_request = Some(nc::ConnectionRequestFrame {
        endpoint_id: Some(endpoint_id.to_owned()),
        endpoint_name: Some(String::new()),
        endpoint_info: Some(endpoint_info.to_vec()),
        nonce: Some(rand::thread_rng().r#gen()),
        mediums: mediums.iter().map(|m| *m as i32).collect(),
        medium_metadata: Some(medium_metadata),
        keep_alive_interval_millis: Some(5_000),
        keep_alive_timeout_millis: Some(30_000),
        ..Default::default()
    });
    offline(f)
}

/// Nearby's per-payload cancel (CONTROL / PAYLOAD_CANCELED at `offset`). Android's own
/// sender and receiver send this for an unfinished file before the sharing CANCEL; without it
/// the phone reports "Can't transfer files" and keeps the partial file.
pub fn payload_canceled(id: i64, total_size: i64, offset: i64) -> nc::OfflineFrame {
    use nc::payload_transfer_frame as ptf;
    let mut f = v1(nc::v1_frame::FrameType::PayloadTransfer);
    f.payload_transfer = Some(nc::PayloadTransferFrame {
        packet_type: Some(ptf::PacketType::Control as i32),
        payload_header: Some(ptf::PayloadHeader {
            id: Some(id),
            r#type: Some(ptf::payload_header::PayloadType::File as i32),
            total_size: Some(total_size),
            is_sensitive: Some(false),
            ..Default::default()
        }),
        payload_chunk: None,
        control_message: Some(ptf::ControlMessage {
            event: Some(ptf::control_message::EventType::PayloadCanceled as i32),
            offset: Some(offset),
        }),
    });
    offline(f)
}

/// A BYTES payload carrying `data`, sent as Android does: one chunk with the
/// whole body, then an empty chunk with LAST_CHUNK set.
pub fn bytes_payload(id: i64, data: &[u8]) -> [nc::OfflineFrame; 2] {
    use nc::payload_transfer_frame as ptf;
    let header = ptf::PayloadHeader {
        id: Some(id),
        r#type: Some(ptf::payload_header::PayloadType::Bytes as i32),
        total_size: Some(data.len() as i64),
        is_sensitive: Some(false),
        ..Default::default()
    };
    let mk = |chunk: ptf::PayloadChunk| {
        let mut f = v1(nc::v1_frame::FrameType::PayloadTransfer);
        f.payload_transfer = Some(nc::PayloadTransferFrame {
            packet_type: Some(ptf::PacketType::Data as i32),
            payload_header: Some(header.clone()),
            payload_chunk: Some(chunk),
            control_message: None,
        });
        offline(f)
    };
    [
        mk(ptf::PayloadChunk { flags: Some(0), offset: Some(0), body: Some(data.to_vec()), index: None }),
        mk(ptf::PayloadChunk {
            flags: Some(ptf::payload_chunk::Flags::LastChunk as i32),
            offset: Some(data.len() as i64),
            body: Some(vec![]),
            index: None,
        }),
    ]
}

pub fn sharing_frame(v: sh::V1Frame) -> Vec<u8> {
    sh::Frame { version: Some(sh::frame::Version::V1 as i32), v1: Some(v) }.encode_to_vec()
}

/// Collects BYTES payload chunks until LAST_CHUNK.
#[derive(Default)]
pub struct BytesAssembler {
    pending: HashMap<i64, Vec<u8>>,
}

impl BytesAssembler {
    /// Feed a chunk; returns the full payload once the last chunk arrives.
    pub fn push(&mut self, id: i64, chunk: &nc::payload_transfer_frame::PayloadChunk) -> Option<Vec<u8>> {
        let buf = self.pending.entry(id).or_default();
        if let Some(body) = &chunk.body {
            buf.extend_from_slice(body);
        }
        if chunk.flags.unwrap_or(0) & 1 != 0 { self.pending.remove(&id) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_payload_reassembles() {
        let frames = bytes_payload(7, b"abc");
        let mut a = BytesAssembler::default();
        let mut out = None;
        for f in &frames {
            let p = f.v1.as_ref().unwrap().payload_transfer.as_ref().unwrap();
            out = a.push(7, p.payload_chunk.as_ref().unwrap());
        }
        assert_eq!(out.unwrap(), b"abc");
    }
}
