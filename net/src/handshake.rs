//! Incoming connection setup: ConnectionRequest → UKEY2 → ConnectionResponse.

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use drift_core::endpoint::EndpointInfo;
use drift_core::payload;
use drift_core::proto::connections as nc;
use drift_core::ukey2::{HandshakeResult, ServerHandshake};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::channel::{BoxRead, BoxWrite, read_frame, write_frame};

const STEP_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct LocalIdentity {
    pub endpoint_id: [u8; 4],
    /// Drift's long-term device key (trust between Drift devices); None = behave as stock.
    pub device_key: Option<std::sync::Arc<drift_core::trust::DeviceKey>>,
    /// Our Wi-Fi Direct device name (Windows: computer name). Lets an Android
    /// GO admit us by name when it supports that auth type.
    pub wifi_direct_device_name: Option<String>,
    /// OS we report to the phone in our ConnectionResponse.
    pub os: drift_core::proto::connections::os_info::OsType,
}

impl LocalIdentity {
    pub fn endpoint_id_str(&self) -> String {
        String::from_utf8_lossy(&self.endpoint_id).into_owned()
    }
}

#[derive(Clone, Debug)]
pub struct PeerInfo {
    pub endpoint_id: String,
    pub info: Option<EndpointInfo>,
    /// Upgrade mediums the phone says it supports.
    pub mediums: Vec<nc::connection_request_frame::Medium>,
    pub metadata: Option<nc::MediumMetadata>,
    /// The phone's IPv4 address on its current Wi-Fi, if it reported one.
    pub ip: Option<Ipv4Addr>,
}

impl PeerInfo {
    pub fn name(&self) -> String {
        self.info.as_ref().and_then(|i| i.name.clone()).unwrap_or_else(|| self.endpoint_id.clone())
    }
    pub fn supports(&self, m: nc::connection_request_frame::Medium) -> bool {
        self.mediums.contains(&m)
    }
}

pub struct Connected {
    pub peer: PeerInfo,
    pub handshake: HandshakeResult,
    pub rd: BoxRead,
    pub wr: BoxWrite,
}

impl Connected {
    pub fn pin(&self) -> String {
        self.handshake.pin()
    }
}

async fn step<T>(what: &str, f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(STEP_TIMEOUT, f).await.map_err(|_| anyhow!("timed out waiting for {what}"))?
}

/// Run the server side of connection setup on a freshly accepted stream.
pub async fn accept<S>(stream: S, local: &LocalIdentity) -> Result<Connected>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (rd, wr) = tokio::io::split(stream);
    let (mut rd, mut wr): (BoxRead, BoxWrite) = (Box::new(rd), Box::new(wr));

    let req = step("connection request", async {
        Ok(nc::OfflineFrame::decode(read_frame(&mut rd).await?.as_slice())?)
    })
    .await?;
    let cr = req
        .v1
        .and_then(|v| v.connection_request)
        .ok_or_else(|| anyhow!("first frame was not a ConnectionRequest"))?;
    let peer = PeerInfo {
        endpoint_id: cr.endpoint_id.clone().unwrap_or_default(),
        info: cr.endpoint_info.as_deref().and_then(|b| EndpointInfo::decode(b).ok()),
        mediums: cr.mediums.iter().filter_map(|m| nc::connection_request_frame::Medium::try_from(*m).ok()).collect(),
        ip: cr
            .medium_metadata
            .as_ref()
            .and_then(|m| m.ip_address.as_deref())
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(Ipv4Addr::from),
        metadata: cr.medium_metadata,
    };
    tracing::info!(peer = %peer.name(), mediums = ?peer.mediums, ip = ?peer.ip, "connection request");

    let ci = step("UKEY2 client init", async { Ok(read_frame(&mut rd).await?) }).await?;
    let (hs, si) = ServerHandshake::handle_client_init(&ci).context("UKEY2 client init")?;
    write_frame(&mut wr, &si).await?;
    let cf = step("UKEY2 client finish", async { Ok(read_frame(&mut rd).await?) }).await?;
    let handshake = hs.handle_client_finish(&cf).context("UKEY2 client finish")?;

    let resp = step("connection response", async {
        Ok(nc::OfflineFrame::decode(read_frame(&mut rd).await?.as_slice())?)
    })
    .await?;
    let accepted = resp
        .v1
        .and_then(|v| v.connection_response)
        .and_then(|r| r.response)
        .is_some_and(|r| r == nc::connection_response_frame::ResponseStatus::Accept as i32);
    if !accepted {
        return Err(anyhow!("peer did not accept the connection"));
    }
    let ours = payload::connection_response_accept(local.os, local.wifi_direct_device_name.as_deref());
    write_frame(&mut wr, &ours.encode_to_vec()).await?;
    Ok(Connected { peer, handshake, rd, wr })
}
