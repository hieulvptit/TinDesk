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
    StartCapture {
        session_id: SessionId,
        generation: u64,
        profile: VideoProfile,
    },
    UpdateProfile {
        session_id: SessionId,
        generation: u64,
        profile: VideoProfile,
    },
    RequestKeyframe {
        session_id: SessionId,
        generation: u64,
    },
    Start {
        session_id: SessionId,
        display: DisplayInfo,
        /// Absolute Linux CLOCK_BOOTTIME milliseconds, at most 60 seconds ahead.
        deadline_boot_ms: u64,
    },
    Input(InputEnvelope),
    /// Only sent by the authenticated local agent after a backend lease renewal.
    RenewLease {
        session_id: SessionId,
        generation: u64,
        deadline_boot_ms: u64,
    },
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
    DisplayUnavailable { generation: u64 },
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
    /// A fresh Annex-B SPS/PPS and IDR can restart decoding after encoded loss.
    pub fn is_config_keyframe(&self) -> bool {
        if !self.keyframe {
            return false;
        }
        let mut types = [false; 3];
        for window in self.data.windows(4) {
            if window[..3] == [0, 0, 1] {
                match window[3] & 31 {
                    7 => types[0] = true,
                    8 => types[1] = true,
                    5 => types[2] = true,
                    _ => {}
                }
            }
        }
        types.into_iter().all(|v| v)
    }
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

/// Encoder names are an allowlist, never an arbitrary pipeline fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoEncoder {
    Auto,
    X264,
    Va,
    Nv,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: VideoEncoder,
}
impl Default for VideoProfile {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate_kbps: 6000,
            encoder: VideoEncoder::Auto,
        }
    }
}
impl VideoProfile {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if !(160..=3840).contains(&self.width)
            || !(90..=2160).contains(&self.height)
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
            || !(5..=60).contains(&self.fps)
            || !(128..=30000).contains(&self.bitrate_kbps)
        {
            return Err(ProtocolError::InvalidPayload);
        }
        Ok(())
    }
    /// Caller applies a sustained-pressure policy; this operation cannot exceed bounds.
    pub fn downshift(self) -> Self {
        Self {
            width: (self.width * 3 / 4 / 2 * 2).max(160),
            height: (self.height * 3 / 4 / 2 * 2).max(90),
            fps: (self.fps * 3 / 4).max(5),
            bitrate_kbps: (self.bitrate_kbps * 3 / 4).max(128),
            ..self
        }
    }
}

/// Dedicated binary stream: RVF1, payload length u32 BE, generation u64 BE,
/// PTS nanoseconds u64 BE, keyframe byte (0/1), then one Annex-B H264 access unit.
/// The 8 MiB data length is validated before allocation; EOF/truncation is fatal.
pub fn write_video_frame(
    writer: &mut impl std::io::Write,
    frame: &EncodedFrame,
) -> std::io::Result<()> {
    frame.validate().map_err(std::io::Error::other)?;
    if frame.data.is_empty() {
        return Err(std::io::Error::other("empty video frame"));
    }
    writer.write_all(b"RVF1")?;
    writer.write_all(&(frame.data.len() as u32).to_be_bytes())?;
    writer.write_all(&frame.generation.to_be_bytes())?;
    writer.write_all(&frame.pts_ns.to_be_bytes())?;
    writer.write_all(&[u8::from(frame.keyframe)])?;
    writer.write_all(&frame.data)
}
pub fn read_video_frame(reader: &mut impl std::io::Read) -> std::io::Result<EncodedFrame> {
    let mut header = [0; 8];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
    if &header[..4] != b"RVF1" || length == 0 || length > ENCODED_FRAME_LIMIT {
        return Err(std::io::Error::other("invalid video frame header"));
    }
    let mut meta = [0; 17];
    reader.read_exact(&mut meta)?;
    if meta[16] > 1 {
        return Err(std::io::Error::other("invalid video frame flags"));
    }
    let mut data = vec![0; length];
    reader.read_exact(&mut data)?;
    Ok(EncodedFrame {
        generation: u64::from_be_bytes(meta[..8].try_into().unwrap()),
        pts_ns: u64::from_be_bytes(meta[8..16].try_into().unwrap()),
        keyframe: meta[16] == 1,
        data,
    })
}

pub enum HelperOutput {
    Event(HelperEvent),
    Video(EncodedFrame),
}
/// Controls retain their existing u32 BE length. Video sets bit31 and contains
/// an RVF1 frame. Unknown magic, oversize or mismatched lengths close the stream.
pub fn write_helper_output(
    writer: &mut impl std::io::Write,
    value: &HelperOutput,
) -> std::io::Result<()> {
    match value {
        HelperOutput::Event(event) => {
            let bytes = crate::encode_control(event).map_err(std::io::Error::other)?;
            writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
            writer.write_all(&bytes)
        }
        HelperOutput::Video(frame) => {
            frame.validate().map_err(std::io::Error::other)?;
            writer.write_all(&(0x80000000u32 | (25 + frame.data.len()) as u32).to_be_bytes())?;
            write_video_frame(writer, frame)
        }
    }
}
pub fn read_helper_output(reader: &mut impl std::io::Read) -> std::io::Result<HelperOutput> {
    use std::io::Read;
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let word = u32::from_be_bytes(size);
    let length = (word & 0x7fffffff) as usize;
    if word & 0x80000000 != 0 {
        if !(26..=ENCODED_FRAME_LIMIT + 25).contains(&length) {
            return Err(std::io::Error::other("invalid video envelope length"));
        }
        let mut limited = reader.take(length as u64);
        let frame = read_video_frame(&mut limited)?;
        if limited.limit() != 0 {
            return Err(std::io::Error::other("video envelope length mismatch"));
        }
        Ok(HelperOutput::Video(frame))
    } else {
        if length == 0 || length > crate::CONTROL_PAYLOAD_LIMIT {
            return Err(std::io::Error::other("invalid control envelope length"));
        }
        let mut bytes = vec![0; length];
        reader.read_exact(&mut bytes)?;
        Ok(HelperOutput::Event(
            crate::decode_control(&bytes).map_err(std::io::Error::other)?,
        ))
    }
}
