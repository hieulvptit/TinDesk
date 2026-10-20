use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::ProtocolError;

pub const WIRE_VERSION: u16 = 1;
pub const CONTROL_PAYLOAD_LIMIT: usize = 64 * 1024;
pub const TEXT_LIMIT: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DeviceId(pub Uuid);

impl DeviceId {
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
}

impl DisplayInfo {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.width == 0 || self.height == 0 {
            return Err(ProtocolError::InvalidDisplay);
        }
        Ok(())
    }
}

// SDP and ICE can contain credentials, so this type deliberately omits Debug.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SignalMessage {
    Offer {
        session_id: SessionId,
        sdp: String,
    },
    Answer {
        session_id: SessionId,
        sdp: String,
    },
    IceCandidate {
        session_id: SessionId,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    End {
        session_id: SessionId,
    },
}

/// Bound the serialized bytes, including JSON escaping and metadata.
pub fn encode_control<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut writer = BoundedWriter {
        bytes: Vec::new(),
        exceeded: false,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| {
        if writer.exceeded {
            ProtocolError::ControlTooLarge
        } else {
            ProtocolError::InvalidPayload
        }
    })?;
    Ok(writer.bytes)
}

/// Transports must also cap incoming frame lengths before allocating their buffers.
pub fn decode_control<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ProtocolError> {
    if bytes.len() > CONTROL_PAYLOAD_LIMIT {
        return Err(ProtocolError::ControlTooLarge);
    }
    serde_json::from_slice(bytes).map_err(|_| ProtocolError::InvalidPayload)
}

struct BoundedWriter {
    bytes: Vec<u8>,
    exceeded: bool,
}
impl std::io::Write for BoundedWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() > CONTROL_PAYLOAD_LIMIT - self.bytes.len() {
            self.exceeded = true;
            return Err(std::io::Error::other("control payload limit"));
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
