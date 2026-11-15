//! Drift core: Quick Share / Nearby Connections protocol, no OS dependencies.

pub mod ble;
pub mod endpoint;
pub mod framing;
pub mod payload;
pub mod proto;
pub mod sharing;
pub mod secure;
pub mod send;
pub mod trust;
pub mod ukey2;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("protobuf decode: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("crypto: {0}")]
    Crypto(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn proto_err<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Protocol(msg.into()))
}
