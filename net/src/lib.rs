//! Drift connection layer: handshake, switchable encrypted channel, bandwidth
//! upgrade ladder, and the receive loop. Async (tokio), OS-agnostic; platform
//! services come in through [`upgrade::Platform`].

pub mod channel;
pub mod handshake;
pub mod health;
pub mod receive;
pub mod send;
pub mod stats;
pub mod upgrade;

pub use handshake::{Connected, LocalIdentity, PeerInfo, accept};
pub use receive::{Medium, ReceiveConfig, ReceiveEvent};
pub use health::Health;
pub use upgrade::{NoPlatform, Platform, Rung, Timeouts};
