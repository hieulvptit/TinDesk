//! Sanitized application-level transport counters. No SDP, ICE URLs, or input.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PeerStats {
    pub sent_messages: u64,
    pub received_messages: u64,
    pub rejected_messages: u64,
    pub dropped_motion: u64,
    pub dropped_frames: u64,
}
#[derive(Default)]
pub(crate) struct Counters {
    pub sent: AtomicU64,
    pub received: AtomicU64,
    pub rejected: AtomicU64,
    pub motion: AtomicU64,
    pub frames: AtomicU64,
}
impl Counters {
    pub fn snapshot(&self) -> PeerStats {
        PeerStats {
            sent_messages: self.sent.load(Ordering::Relaxed),
            received_messages: self.received.load(Ordering::Relaxed),
            rejected_messages: self.rejected.load(Ordering::Relaxed),
            dropped_motion: self.motion.load(Ordering::Relaxed),
            dropped_frames: self.frames.load(Ordering::Relaxed),
        }
    }
}
