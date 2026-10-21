use crate::{DisplayInfo, ProtocolError, SessionId, TEXT_LIMIT, WIRE_VERSION, encode_control};
use serde::{Deserialize, Serialize};

pub const LEASE_DURATION_MS: u64 = 60_000;
pub const LEASE_RENEWAL_INTERVAL_MS: u64 = 15_000;

// Input can contain passwords; Debug is intentionally unavailable.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputEnvelope {
    pub version: u16,
    pub session_id: SessionId,
    pub generation: u64,
    pub sequence: u64,
    pub event: InputEvent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    PointerMove {
        x: u32,
        y: u32,
    },
    Button {
        button: MouseButton,
        pressed: bool,
        x: u32,
        y: u32,
    },
    /// X11 keysym; adapters translate native key codes to this shared namespace.
    Key {
        keysym: u32,
        pressed: bool,
    },
    Text {
        text: String,
    },
    /// Signed wheel steps at the supplied pixel position.
    /// Content motion: positive Y scrolls up (X11 button 4), positive X left (6).
    Scroll {
        delta_x: i32,
        delta_y: i32,
        x: u32,
        y: u32,
    },
    ReleaseAll,
    DisplayAck,
}

/// Local policy only. Callers must release held input and stop media on expiry,
/// independently of receiving another input message. `now_ms` is monotonic time.
pub struct InputGuard {
    session_id: SessionId,
    display: DisplayInfo,
    last_renewed_ms: u64,
    last_motion_sequence: Option<u64>,
}

impl InputGuard {
    pub fn new(
        session_id: SessionId,
        display: DisplayInfo,
        now_ms: u64,
    ) -> Result<Self, ProtocolError> {
        display.validate()?;
        Ok(Self {
            session_id,
            display,
            last_renewed_ms: now_ms,
            last_motion_sequence: None,
        })
    }

    pub fn renew_lease(&mut self, now_ms: u64) {
        self.last_renewed_ms = self.last_renewed_ms.max(now_ms);
    }

    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_renewed_ms) >= LEASE_DURATION_MS
    }

    pub fn update_display(&mut self, display: DisplayInfo) -> Result<(), ProtocolError> {
        display.validate()?;
        if display.generation <= self.display.generation {
            return Err(ProtocolError::WrongGeneration);
        }
        self.display = display;
        self.last_motion_sequence = None;
        Ok(())
    }

    pub fn accept(&mut self, input: &InputEnvelope, now_ms: u64) -> Result<(), ProtocolError> {
        if input.version != WIRE_VERSION {
            return Err(ProtocolError::UnsupportedVersion);
        }
        if input.session_id != self.session_id {
            return Err(ProtocolError::WrongSession);
        }
        if input.generation != self.display.generation {
            return Err(ProtocolError::WrongGeneration);
        }
        if self.expired(now_ms) {
            return Err(ProtocolError::LeaseExpired);
        }
        match &input.event {
            InputEvent::PointerMove { x, y }
            | InputEvent::Button { x, y, .. }
            | InputEvent::Scroll { x, y, .. }
                if *x >= self.display.width || *y >= self.display.height =>
            {
                return Err(ProtocolError::InvalidCoordinates);
            }
            InputEvent::Text { text } if text.len() > TEXT_LIMIT => {
                return Err(ProtocolError::TextTooLarge);
            }
            _ => {}
        }
        encode_control(input)?;
        // One sender sequence spans both channels. Reliable events are never
        // suppressed, but position-bearing controls advance the motion high-water.
        if matches!(input.event, InputEvent::PointerMove { .. }) {
            if self
                .last_motion_sequence
                .is_some_and(|last| input.sequence <= last)
            {
                return Err(ProtocolError::StaleSequence);
            }
            self.last_motion_sequence = Some(input.sequence);
        } else if matches!(
            input.event,
            InputEvent::Button { .. } | InputEvent::Scroll { .. }
        ) {
            self.last_motion_sequence =
                Some(self.last_motion_sequence.unwrap_or(0).max(input.sequence));
        }
        Ok(())
    }
}
