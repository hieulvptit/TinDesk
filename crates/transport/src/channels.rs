//! Data-channel contracts and validation, shared by native callbacks and tests.
use remotevnc_protocol::*;

pub struct ChannelSpec {
    pub label: &'static str,
    pub ordered: bool,
    pub max_retransmits: Option<i32>,
}
pub const MOTION: ChannelSpec = ChannelSpec {
    label: "pointer-motion",
    ordered: false,
    max_retransmits: Some(0),
};
pub const CONTROL: ChannelSpec = ChannelSpec {
    label: "control",
    ordered: true,
    max_retransmits: None,
};

pub fn channel_for(event: &InputEvent) -> &'static str {
    if matches!(event, InputEvent::PointerMove { .. }) {
        MOTION.label
    } else {
        CONTROL.label
    }
}

/// Receiving bytes never renews the lease. Only an authorized backend renewal may
/// call renew_lease. Display changes require a fresh DisplayAck before injection.
pub struct InputDecoder {
    session_id: SessionId,
    guard: Option<InputGuard>,
    acknowledged: bool,
    last_renewed_ms: u64,
}
impl InputDecoder {
    pub fn new(
        session_id: SessionId,
        display: Option<DisplayInfo>,
        now_ms: u64,
    ) -> Result<Self, ProtocolError> {
        Ok(Self {
            session_id,
            guard: display
                .map(|d| InputGuard::new(session_id, d, now_ms))
                .transpose()?,
            acknowledged: false,
            last_renewed_ms: now_ms,
        })
    }
    pub fn update_display(&mut self, display: DisplayInfo) -> Result<(), ProtocolError> {
        match &mut self.guard {
            Some(guard) => guard.update_display(display)?,
            None => {
                self.guard = Some(InputGuard::new(
                    self.session_id,
                    display,
                    self.last_renewed_ms,
                )?)
            }
        }
        self.acknowledged = false;
        Ok(())
    }
    pub(crate) fn last_renewed_ms(&self) -> u64 {
        self.last_renewed_ms
    }
    pub fn renew_lease(&mut self, now_ms: u64) {
        self.last_renewed_ms = self.last_renewed_ms.max(now_ms);
        if let Some(guard) = &mut self.guard {
            guard.renew_lease(now_ms);
        }
    }
    pub fn expired(&self, now_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_renewed_ms) >= LEASE_DURATION_MS
    }
    pub fn decode(
        &mut self,
        label: &str,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<InputEnvelope, ProtocolError> {
        let input: InputEnvelope = decode_control(bytes)?;
        if label != channel_for(&input.event) {
            return Err(ProtocolError::InvalidPayload);
        }
        let guard = self
            .guard
            .as_mut()
            .ok_or(ProtocolError::DisplayUnavailable)?;
        // Check the complete envelope before changing either acknowledgement or sequence.
        if !self.acknowledged
            && !matches!(input.event, InputEvent::DisplayAck | InputEvent::ReleaseAll)
        {
            // Validate scope first, but don't advance pointer sequence for rejected input.
            let mut probe = input.clone();
            probe.event = InputEvent::ReleaseAll;
            guard.accept(&probe, now_ms)?;
            return Err(ProtocolError::DisplayNotAcknowledged);
        }
        guard.accept(&input, now_ms)?;
        if matches!(input.event, InputEvent::DisplayAck) {
            self.acknowledged = true;
        }
        Ok(input)
    }
}
