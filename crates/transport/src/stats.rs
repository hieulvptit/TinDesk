//! Sanitized application-level transport counters. No SDP, ICE URLs, or input.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PeerStats {
    pub sent_messages: u64,
    pub received_messages: u64,
    pub rejected_messages: u64,
    pub dropped_motion: u64,
    pub dropped_frames: u64,
    pub dropped_encoded_frames: u64,
    pub native: NativeStats,
}
#[derive(Default)]
pub(crate) struct Counters {
    pub sent: AtomicU64,
    pub received: AtomicU64,
    pub rejected: AtomicU64,
    pub motion: AtomicU64,
    pub frames: AtomicU64,
    pub encoded: AtomicU64,
    pub native: std::sync::Mutex<NativeStats>,
}
impl Counters {
    pub fn snapshot(&self) -> PeerStats {
        PeerStats {
            sent_messages: self.sent.load(Ordering::Relaxed),
            received_messages: self.received.load(Ordering::Relaxed),
            rejected_messages: self.rejected.load(Ordering::Relaxed),
            dropped_motion: self.motion.load(Ordering::Relaxed),
            dropped_frames: self.frames.load(Ordering::Relaxed),
            dropped_encoded_frames: self.encoded.load(Ordering::Relaxed),
            native: *self.native.lock().unwrap(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateType {
    Host,
    ServerReflexive,
    PeerReflexive,
    Relay,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NativeStats {
    pub rtt_ms: Option<u64>,
    pub loss_ppm: Option<u32>,
    pub packets_sent: Option<u64>,
    pub bytes_sent: Option<u64>,
    pub packets_received: Option<u64>,
    pub local_candidate: Option<CandidateType>,
    pub remote_candidate: Option<CandidateType>,
    pub queued_video_frames: Option<u64>,
}
/// Observe at one-second intervals. Missing measurements are unavailable, never
/// synthesized from ICE configuration. Three pressured samples trigger a step,
/// followed by five samples of cooldown; the agent applies it to the helper.
pub struct AdaptiveProfile {
    profile: remotevnc_protocol::VideoProfile,
    strikes: u8,
    cooldown: u8,
    dropped: u64,
}
impl AdaptiveProfile {
    pub fn new(profile: remotevnc_protocol::VideoProfile) -> crate::Result<Self> {
        profile.validate()?;
        Ok(Self {
            profile,
            strikes: 0,
            cooldown: 0,
            dropped: 0,
        })
    }
    pub fn observe(&mut self, stats: &PeerStats) -> Option<remotevnc_protocol::VideoProfile> {
        let pressure = stats.dropped_encoded_frames > self.dropped
            || stats.native.queued_video_frames.is_some_and(|n| n >= 2)
            || stats.native.loss_ppm.is_some_and(|n| n > 50_000)
            || stats.native.rtt_ms.is_some_and(|n| n > 250);
        self.dropped = stats.dropped_encoded_frames;
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return None;
        }
        self.strikes = if pressure { self.strikes + 1 } else { 0 };
        if self.strikes < 3 {
            return None;
        }
        self.strikes = 0;
        self.cooldown = 5;
        let next = self.profile.downshift();
        if next == self.profile {
            return None;
        }
        self.profile = next;
        Some(next)
    }
}
pub(crate) fn parse_native(report: &gst::StructureRef) -> NativeStats {
    use gst_webrtc::WebRTCStatsType as Kind;
    let mut stats = NativeStats::default();
    for (_, value) in report.iter() {
        let Ok(record) = value.get::<gst::Structure>() else {
            continue;
        };
        match record.get::<Kind>("type").ok() {
            Some(Kind::RemoteInboundRtp) => {
                stats.rtt_ms = record
                    .get::<f64>("round-trip-time")
                    .ok()
                    .filter(|v| v.is_finite() && *v >= 0.)
                    .map(|v| (v * 1000.) as u64);
                stats.loss_ppm = record
                    .get::<f64>("fraction-lost")
                    .ok()
                    .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                    .map(|v| (v * 1_000_000.) as u32);
            }
            Some(Kind::OutboundRtp) => {
                stats.packets_sent = record.get("packets-sent").ok();
                stats.bytes_sent = record.get("bytes-sent").ok();
            }
            Some(Kind::InboundRtp) => {
                stats.packets_received = record.get("packets-received").ok();
            }
            Some(Kind::Transport) => {
                let pair = record
                    .get::<&str>("selected-candidate-pair-id")
                    .ok()
                    .and_then(|id| report.get::<gst::Structure>(id).ok());
                if let Some(pair) = pair {
                    for (key, out) in [
                        ("local-candidate-id", &mut stats.local_candidate),
                        ("remote-candidate-id", &mut stats.remote_candidate),
                    ] {
                        *out = pair
                            .get::<&str>(key)
                            .ok()
                            .and_then(|id| report.get::<gst::Structure>(id).ok())
                            .and_then(|candidate| {
                                match candidate.get::<&str>("candidate-type").ok()? {
                                    "host" => Some(CandidateType::Host),
                                    "srflx" => Some(CandidateType::ServerReflexive),
                                    "prflx" => Some(CandidateType::PeerReflexive),
                                    "relay" => Some(CandidateType::Relay),
                                    _ => None,
                                }
                            });
                    }
                }
            }
            _ => {}
        }
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_measurements_are_optional_and_selected_pair_is_resolved() {
        gst::init().unwrap();
        let empty = gst::Structure::new_empty("stats");
        assert_eq!(parse_native(&empty), NativeStats::default());
        let report = gst::Structure::builder("stats")
            .field(
                "transport",
                gst::Structure::builder("transport")
                    .field("type", gst_webrtc::WebRTCStatsType::Transport)
                    .field("selected-candidate-pair-id", "pair")
                    .build(),
            )
            .field(
                "pair",
                gst::Structure::builder("pair")
                    .field("local-candidate-id", "local")
                    .field("remote-candidate-id", "remote")
                    .build(),
            )
            .field(
                "local",
                gst::Structure::builder("candidate")
                    .field("candidate-type", "host")
                    .build(),
            )
            .field(
                "remote",
                gst::Structure::builder("candidate")
                    .field("candidate-type", "relay")
                    .build(),
            )
            .field(
                "rtcp",
                gst::Structure::builder("rtcp")
                    .field("type", gst_webrtc::WebRTCStatsType::RemoteInboundRtp)
                    .field("round-trip-time", 0.125f64)
                    .field("fraction-lost", 0.04f64)
                    .build(),
            )
            .build();
        let stats = parse_native(&report);
        assert_eq!(stats.rtt_ms, Some(125));
        assert_eq!(stats.loss_ppm, Some(40_000));
        assert_eq!(stats.local_candidate, Some(CandidateType::Host));
        assert_eq!(stats.remote_candidate, Some(CandidateType::Relay));
        assert_eq!(stats.packets_sent, None);
    }
}
