use crate::{DisplayInfo, InputEnvelope, ProtocolError, SessionId};
use serde::{Deserialize, Serialize};
use std::{
    fmt,
    sync::mpsc::{self, Receiver, SyncSender, TrySendError},
};

pub const ENCODED_FRAME_LIMIT: usize = 8 * 1024 * 1024;
pub const FRAME_QUEUE_CAPACITY: usize = 2;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HelperRequest {
    Start {
        session_id: SessionId,
        display: DisplayInfo,
    },
    Input(InputEnvelope),
    ReleaseAll {
        session_id: SessionId,
        generation: u64,
    },
    Stop {
        session_id: SessionId,
        generation: u64,
    },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "snake_case")]
pub enum HelperEvent {
    Ready,
    DisplayChanged(DisplayInfo),
    Stopped { session_id: SessionId },
    Error { message: String },
}

/// Encoded bytes use a separate binary IPC frame, not the JSON control codec.
/// IPC readers must validate the announced data length before allocation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodedFrame {
    pub generation: u64,
    pub pts_ns: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

impl EncodedFrame {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.data.len() > ENCODED_FRAME_LIMIT {
            return Err(ProtocolError::FrameTooLarge);
        }
        Ok(())
    }
}

impl fmt::Debug for EncodedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EncodedFrame")
            .field("generation", &self.generation)
            .field("pts_ns", &self.pts_ns)
            .field("keyframe", &self.keyframe)
            .field("data_len", &self.data.len())
            .finish()
    }
}

#[derive(Debug)]
pub enum FrameSendError {
    Invalid(ProtocolError),
    Full(EncodedFrame),
    Disconnected(EncodedFrame),
}

pub struct FrameSender(SyncSender<EncodedFrame>);
impl FrameSender {
    /// Nonblocking: producers choose their explicit drop/retry policy when full.
    pub fn try_send(&self, frame: EncodedFrame) -> Result<(), FrameSendError> {
        frame.validate().map_err(FrameSendError::Invalid)?;
        self.0.try_send(frame).map_err(|error| match error {
            TrySendError::Full(frame) => FrameSendError::Full(frame),
            TrySendError::Disconnected(frame) => FrameSendError::Disconnected(frame),
        })
    }
}

/// At most two frames (16 MiB of encoded data) can wait in this queue.
pub fn frame_channel() -> (FrameSender, Receiver<EncodedFrame>) {
    let (sender, receiver) = mpsc::sync_channel(FRAME_QUEUE_CAPACITY);
    (FrameSender(sender), receiver)
}
