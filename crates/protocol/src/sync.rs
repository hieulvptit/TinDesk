//! Clipboard and file transfer over one ordered, reliable data channel.
//!
//! Pure state machines with no I/O of their own: adapters feed received frames
//! into `SyncReceiver` and persist bytes through `Sink`. Text and files share one
//! chunked, hash-verified transfer so the limits and validation live in one place.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const SYNC_CHANNEL_LABEL: &str = "sync";
/// Whole frame on the wire; stays under common SCTP message limits.
pub const SYNC_FRAME_LIMIT: usize = 48 * 1024;
pub const SYNC_CHUNK_SIZE: usize = 32 * 1024;
pub const CLIPBOARD_TEXT_LIMIT: u64 = 4 * 1024 * 1024;
pub const FILE_LIMIT: u64 = 512 * 1024 * 1024;
pub const MAX_ACTIVE_TRANSFERS: usize = 4;
pub const NAME_LIMIT: usize = 255;
pub const MAX_BATCH_FILES: usize = 64;
/// SyncSender may have at most this many unacknowledged bytes in flight.
pub const SYNC_WINDOW: u64 = 1024 * 1024;
/// SyncReceiver reports progress each time this many more bytes were stored.
pub const PROGRESS_STEP: u64 = 256 * 1024;

const TAG_CONTROL: u8 = 0;
const TAG_CHUNK: u8 = 1;
const CHUNK_HEADER: usize = 1 + 4 + 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncKind {
    /// UTF-8 clipboard text; `name` is ignored.
    Text,
    File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    Disabled,
    TooLarge,
    TooMany,
    InvalidName,
    InvalidState,
    IntegrityFailed,
    StorageFailed,
}

// Control messages never carry content (clipboard text can hold passwords);
// content only travels in chunk frames, so Debug here cannot leak it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SyncMessage {
    Offer {
        id: u32,
        kind: SyncKind,
        name: String,
        size: u64,
    },
    Reject {
        id: u32,
        reason: RejectReason,
    },
    /// SyncSender is done; `sha256` is lowercase hex of all content bytes.
    Finish {
        id: u32,
        sha256: String,
    },
    /// SyncReceiver verified and stored the transfer.
    Done {
        id: u32,
    },
    Cancel {
        id: u32,
    },
    /// Sender finished a batch of files; make these completed transfers the
    /// receiver's clipboard, in order. Text transfers publish themselves.
    Publish {
        ids: Vec<u32>,
    },
    /// Flow control: receiver has stored `received` bytes so far.
    Progress {
        id: u32,
        received: u64,
    },
}

pub enum SyncFrame<'a> {
    Control(SyncMessage),
    Chunk {
        id: u32,
        offset: u64,
        data: &'a [u8],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SyncError {
    #[error("malformed sync frame")]
    Malformed,
    #[error("sync frame exceeds limit")]
    FrameTooLarge,
    #[error("invalid transfer name")]
    InvalidName,
    #[error("transfer size is not allowed")]
    TooLarge,
    #[error("chunk is out of order or exceeds announced size")]
    BadChunk,
    #[error("send window is full")]
    WindowFull,
}

pub fn encode_sync_control(message: &SyncMessage) -> Result<Vec<u8>, SyncError> {
    let mut out = vec![TAG_CONTROL];
    serde_json::to_writer(&mut out, message).map_err(|_| SyncError::Malformed)?;
    if out.len() > SYNC_FRAME_LIMIT {
        return Err(SyncError::FrameTooLarge);
    }
    Ok(out)
}

pub fn encode_chunk(id: u32, offset: u64, data: &[u8]) -> Result<Vec<u8>, SyncError> {
    if CHUNK_HEADER + data.len() > SYNC_FRAME_LIMIT {
        return Err(SyncError::FrameTooLarge);
    }
    let mut out = Vec::with_capacity(CHUNK_HEADER + data.len());
    out.push(TAG_CHUNK);
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> Result<SyncFrame<'_>, SyncError> {
    if bytes.len() > SYNC_FRAME_LIMIT {
        return Err(SyncError::FrameTooLarge);
    }
    match bytes.first() {
        Some(&TAG_CONTROL) => serde_json::from_slice(&bytes[1..])
            .map(SyncFrame::Control)
            .map_err(|_| SyncError::Malformed),
        Some(&TAG_CHUNK) if bytes.len() >= CHUNK_HEADER => Ok(SyncFrame::Chunk {
            id: u32::from_be_bytes(bytes[1..5].try_into().unwrap()),
            offset: u64::from_be_bytes(bytes[5..13].try_into().unwrap()),
            data: &bytes[CHUNK_HEADER..],
        }),
        _ => Err(SyncError::Malformed),
    }
}

/// A received name is only ever a single path component; it never selects a directory.
pub fn sanitize_name(name: &str) -> Result<String, SyncError> {
    let bad = name.is_empty()
        || name.len() > NAME_LIMIT
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    if bad {
        Err(SyncError::InvalidName)
    } else {
        Ok(name.to_owned())
    }
}

pub fn size_allowed(kind: SyncKind, size: u64) -> bool {
    match kind {
        SyncKind::Text => size <= CLIPBOARD_TEXT_LIMIT,
        SyncKind::File => size <= FILE_LIMIT,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Produces frames for one outgoing transfer. The caller sends `Offer` first.
pub struct SyncSender {
    id: u32,
    size: u64,
    offset: u64,
    acked: u64,
    hasher: Sha256,
}
impl SyncSender {
    pub fn new(
        id: u32,
        kind: SyncKind,
        name: &str,
        size: u64,
    ) -> Result<(Self, SyncMessage), SyncError> {
        if !size_allowed(kind, size) {
            return Err(SyncError::TooLarge);
        }
        let name = match kind {
            SyncKind::Text => String::new(),
            SyncKind::File => sanitize_name(name)?,
        };
        Ok((
            Self {
                id,
                size,
                offset: 0,
                acked: 0,
                hasher: Sha256::new(),
            },
            SyncMessage::Offer {
                id,
                kind,
                name,
                size,
            },
        ))
    }
    /// False while a full window is unacknowledged; wait for `on_progress`.
    pub fn can_send(&self) -> bool {
        self.offset - self.acked < SYNC_WINDOW
    }
    pub fn on_progress(&mut self, received: u64) {
        if received <= self.offset {
            self.acked = self.acked.max(received);
        }
    }
    pub fn chunk(&mut self, data: &[u8]) -> Result<Vec<u8>, SyncError> {
        let end = self
            .offset
            .checked_add(data.len() as u64)
            .filter(|end| *end <= self.size)
            .ok_or(SyncError::BadChunk)?;
        if !self.can_send() {
            return Err(SyncError::WindowFull);
        }
        let frame = encode_chunk(self.id, self.offset, data)?;
        self.hasher.update(data);
        self.offset = end;
        Ok(frame)
    }
    pub fn finish(self) -> Result<Vec<u8>, SyncError> {
        if self.offset != self.size {
            return Err(SyncError::BadChunk);
        }
        encode_sync_control(&SyncMessage::Finish {
            id: self.id,
            sha256: hex(&self.hasher.finalize()),
        })
    }
}

/// Where accepted bytes go. `commit` publishes atomically; `abort` discards.
pub trait Sink {
    fn begin(&mut self, id: u32, kind: SyncKind, name: &str, size: u64) -> std::io::Result<()>;
    fn write(&mut self, id: u32, data: &[u8]) -> std::io::Result<()>;
    fn commit(&mut self, id: u32) -> std::io::Result<()>;
    fn abort(&mut self, id: u32);
    /// Completed file transfers become the clipboard. Default: nothing to do.
    fn publish(&mut self, _ids: &[u32]) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub clipboard: bool,
    pub files: bool,
}

struct Active {
    size: u64,
    received: u64,
    reported: u64,
    hasher: Sha256,
}

pub struct SyncReceiver<S: Sink> {
    sink: S,
    pub policy: Policy,
    active: HashMap<u32, Active>,
}
impl<S: Sink> SyncReceiver<S> {
    pub fn new(sink: S, policy: Policy) -> Self {
        Self {
            sink,
            policy,
            active: HashMap::new(),
        }
    }
    pub fn sink(&self) -> &S {
        &self.sink
    }
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }
    /// Abort everything in flight, e.g. on peer restart or lease expiry.
    pub fn reset(&mut self) {
        for (id, _) in self.active.drain() {
            self.sink.abort(id);
        }
    }
    fn reject(&mut self, id: u32, reason: RejectReason) -> Vec<SyncMessage> {
        if self.active.remove(&id).is_some() {
            self.sink.abort(id);
        }
        vec![SyncMessage::Reject { id, reason }]
    }
    /// Returns messages to send back. A protocol violation aborts only that transfer.
    pub fn handle(&mut self, bytes: &[u8]) -> Result<Vec<SyncMessage>, SyncError> {
        match decode_frame(bytes)? {
            SyncFrame::Control(SyncMessage::Offer {
                id,
                kind,
                name,
                size,
            }) => {
                let enabled = match kind {
                    SyncKind::Text => self.policy.clipboard,
                    SyncKind::File => self.policy.files,
                };
                if !enabled {
                    return Ok(self.reject(id, RejectReason::Disabled));
                }
                if self.active.contains_key(&id) {
                    return Ok(self.reject(id, RejectReason::InvalidState));
                }
                if self.active.len() >= MAX_ACTIVE_TRANSFERS {
                    return Ok(self.reject(id, RejectReason::TooMany));
                }
                if !size_allowed(kind, size) {
                    return Ok(self.reject(id, RejectReason::TooLarge));
                }
                let name = match kind {
                    SyncKind::Text => String::new(),
                    SyncKind::File => match sanitize_name(&name) {
                        Ok(n) => n,
                        Err(_) => return Ok(self.reject(id, RejectReason::InvalidName)),
                    },
                };
                if self.sink.begin(id, kind, &name, size).is_err() {
                    return Ok(vec![SyncMessage::Reject {
                        id,
                        reason: RejectReason::StorageFailed,
                    }]);
                }
                self.active.insert(
                    id,
                    Active {
                        size,
                        received: 0,
                        reported: 0,
                        hasher: Sha256::new(),
                    },
                );
                Ok(vec![])
            }
            SyncFrame::Chunk { id, offset, data } => {
                // Chunks already in flight when we rejected/cancelled an id are expected.
                let Some(active) = self.active.get_mut(&id) else {
                    return Ok(vec![]);
                };
                let end = offset.checked_add(data.len() as u64);
                if offset != active.received || end.is_none_or(|e| e > active.size) {
                    return Ok(self.reject(id, RejectReason::InvalidState));
                }
                active.received += data.len() as u64;
                active.hasher.update(data);
                if self.sink.write(id, data).is_err() {
                    return Ok(self.reject(id, RejectReason::StorageFailed));
                }
                if active.received - active.reported >= PROGRESS_STEP {
                    active.reported = active.received;
                    return Ok(vec![SyncMessage::Progress {
                        id,
                        received: active.received,
                    }]);
                }
                Ok(vec![])
            }
            SyncFrame::Control(SyncMessage::Finish { id, sha256 }) => {
                let Some(active) = self.active.remove(&id) else {
                    return Ok(vec![SyncMessage::Reject {
                        id,
                        reason: RejectReason::InvalidState,
                    }]);
                };
                if active.received != active.size || hex(&active.hasher.finalize()) != sha256 {
                    self.sink.abort(id);
                    return Ok(vec![SyncMessage::Reject {
                        id,
                        reason: RejectReason::IntegrityFailed,
                    }]);
                }
                match self.sink.commit(id) {
                    Ok(()) => Ok(vec![SyncMessage::Done { id }]),
                    Err(_) => {
                        self.sink.abort(id);
                        Ok(vec![SyncMessage::Reject {
                            id,
                            reason: RejectReason::StorageFailed,
                        }])
                    }
                }
            }
            SyncFrame::Control(SyncMessage::Cancel { id }) => {
                if self.active.remove(&id).is_some() {
                    self.sink.abort(id);
                }
                Ok(vec![])
            }
            // Replies are for the sender side; the receiver has nothing to do.
            SyncFrame::Control(SyncMessage::Publish { ids }) => {
                if ids.is_empty() || ids.len() > MAX_BATCH_FILES {
                    return Err(SyncError::Malformed);
                }
                Ok(match self.sink.publish(&ids) {
                    Ok(()) => vec![],
                    Err(_) => vec![SyncMessage::Reject {
                        id: ids[0],
                        reason: RejectReason::StorageFailed,
                    }],
                })
            }
            SyncFrame::Control(
                SyncMessage::Reject { .. }
                | SyncMessage::Done { .. }
                | SyncMessage::Progress { .. },
            ) => Ok(vec![]),
        }
    }
}
