//! Shared RemoteVNC wire contracts and local input policy, with no native dependencies.
pub mod identity;
pub mod input;
pub mod ipc;
pub mod wire;

pub use identity::*;
pub use input::*;
pub use ipc::*;
pub use wire::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported wire version")]
    UnsupportedVersion,
    #[error("control payload exceeds 64 KiB")]
    ControlTooLarge,
    #[error("text exceeds 16 KiB UTF-8")]
    TextTooLarge,
    #[error("encoded frame exceeds 8 MiB")]
    FrameTooLarge,
    #[error("invalid wire payload")]
    InvalidPayload,
    #[error("input belongs to another session")]
    WrongSession,
    #[error("input belongs to another display generation")]
    WrongGeneration,
    #[error("pointer motion is duplicate or out of order")]
    StaleSequence,
    #[error("coordinates are outside the display")]
    InvalidCoordinates,
    #[error("display dimensions must be nonzero")]
    InvalidDisplay,
    #[error("control lease expired")]
    LeaseExpired,
    #[error("device challenge expired")]
    ChallengeExpired,
    #[error("device challenge already consumed")]
    ChallengeReplayed,
    #[error("invalid device signature")]
    InvalidSignature,
}
