//! Generated protobuf types (Apache-2.0 protos from google/nearby and google/ukey2).
#![allow(clippy::all, missing_docs)]

mod generated {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

pub use generated::location::nearby::connections;
pub use generated::nearby::sharing::service::proto as sharing;
pub use generated::securegcm;
pub use generated::securemessage;
