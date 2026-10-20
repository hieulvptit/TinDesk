//! Device identity and enrollment client, independent of native media.
pub mod credentials;
pub use credentials::*;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("credential storage unavailable or unsafe")]
    Storage,
    #[error("invalid device configuration or credentials")]
    Invalid,
    #[error("event connection closed by authorization or protocol policy")]
    PolicyClosed,
    #[error("backend request failed")]
    Request,
    #[error("caller already has an active session")]
    CallerBusy,
    #[error("agent already has an active session")]
    AgentBusy,
    #[error("TURN unavailable")]
    TurnUnavailable,
    #[error("active device limit reached")]
    Limit,
}
pub type Result<T> = std::result::Result<T, Error>;

pub mod api;
pub use api::DeviceApi;

pub mod enrollment;
