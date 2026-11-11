//! Real GStreamer WebRTC peers for already-authorized, single-controller sessions.
//! No credentials, SDP, decoded input, or frame pixels are included in Debug.
pub mod channels;
mod peer;
pub mod stats;
pub use binachicken_protocol::IceServer;
pub use crossbeam_channel::Receiver;
pub use peer::*;
pub use stats::PeerStats;

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum TransportError {
    #[error("signaling belongs to another PeerConnection epoch")]
    StaleEpoch,
    #[error(transparent)]
    Protocol(#[from] binachicken_protocol::ProtocolError),
    #[error("required GStreamer runtime or plugin is unavailable")]
    NativeDependency,
    #[error("GStreamer operation failed")]
    NativeOperation,
    #[error("peer is closed")]
    Closed,
    #[error("channel is not open")]
    NotReady,
    #[error("operation is not allowed for this role")]
    WrongRole,
    #[error("bounded transport queue is full")]
    QueueFull,
    #[error("invalid or expired ICE configuration")]
    InvalidIce,
    #[error("another negotiation is in progress")]
    NegotiationBusy,
}
pub type Result<T> = std::result::Result<T, TransportError>;
