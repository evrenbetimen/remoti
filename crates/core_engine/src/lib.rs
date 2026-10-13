//! Remoti agent core: capture, encode, end-to-end encrypted QUIC transport
//! with TCP fallback, adaptive rate control and sanitized input injection.

pub mod agent;
pub mod capture;
pub mod congestion;
pub mod crypto;
pub mod encoder;
pub mod frame;
pub mod handshake;
pub mod host;
pub mod identity;
pub mod input;
pub mod ipc;
pub mod network;
pub mod relay;
pub mod router;
pub mod signaling;
pub mod stream;
pub mod transport;
pub mod viewer;
