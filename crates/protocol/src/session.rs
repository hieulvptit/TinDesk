//! Authenticated backend control plane, independent of media implementations.
use crate::{DeviceId, IceServer, SessionId, SignalMessage};
use serde::{Deserialize, Serialize};
pub const LEASE_RENEWAL_SECS: u64 = 15;
pub const LEASE_LIFETIME_SECS: u64 = 60;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Available,
    Unavailable,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SessionGrant {
    pub session_id: SessionId,
    pub client_id: DeviceId,
    pub agent_id: DeviceId,
    /// Absolute Unix milliseconds. Renew every LEASE_RENEWAL_SECS.
    pub lease_expires_at: u64,
    pub ice_servers: Vec<IceServer>,
    pub turn_status: TurnStatus,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct CreateSession {
    pub agent_id: DeviceId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceRole {
    Agent,
    Client,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePresence {
    pub device_id: DeviceId,
    pub role: DeviceRole,
    pub online: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceCommand {
    Signal {
        receiver: DeviceId,
        message: SignalMessage,
    },
    /// Send only after installing these credentials locally without restarting.
    IceInstalled {
        session_id: SessionId,
        generation: u64,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceEvent {
    Presence {
        devices: Vec<DevicePresence>,
    },
    /// Authoritative active IDs at connection start; empty clears stale local grants.
    SessionSnapshot {
        session_ids: Vec<SessionId>,
    },
    Session {
        grant: SessionGrant,
    },
    SessionClosed {
        session_id: SessionId,
    },
    Signal {
        sender: DeviceId,
        message: SignalMessage,
    },
    /// Both peers install, then ACK. Agent alone restarts on IceRestartReady.
    IceConfiguration {
        session_id: SessionId,
        generation: u64,
        requires_ack: bool,
        ice_servers: Vec<IceServer>,
    },
    IceRestartReady {
        session_id: SessionId,
        generation: u64,
    },
}

impl SessionGrant {
    /// Schedule the agent's refresh request independently of the 15-second lease timer.
    pub fn turn_refresh_at_unix_secs(&self) -> Option<u64> {
        self.ice_servers
            .iter()
            .filter_map(|s| s.expires_at_unix_secs)
            .min()
            .map(|expiry| expiry.saturating_sub(300))
    }
}
