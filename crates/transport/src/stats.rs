//! Sanitized application-level transport counters. No SDP, ICE URLs, or input.
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PeerStats {
    pub sent_messages: u64,
    pub received_messages: u64,
    pub rejected_messages: u64,
    pub dropped_motion: u64,
    /// Decoded frames superseded locally before the consumer could display them.
    /// This is distinct from RTP packet loss and encoded sender backpressure.
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
    /// Sum of available inbound RTP byte counters; excludes SCTP and wire overhead.
    pub bytes_received: Option<u64>,
    /// Inbound RTP packet loss is signed: duplicate packets may produce negatives.
    pub packets_lost: Option<i64>,
    /// Monotonic milliseconds since Peer creation, set only for a fresh native
    /// report accepted from the active epoch. Repeated snapshots retain this time.
    pub sampled_at_ms: Option<u64>,
    pub local_candidate: Option<CandidateType>,
    pub remote_candidate: Option<CandidateType>,
    pub queued_video_frames: Option<u64>,
}
impl NativeStats {
    /// Classify the selected ICE pair only. Configured STUN/TURN servers do not
    /// prove which path was selected, and an incomplete pair stays unknown.
    pub fn connection_path(&self) -> Option<&'static str> {
        match (self.local_candidate, self.remote_candidate) {
            (Some(CandidateType::Relay), _) | (_, Some(CandidateType::Relay)) => Some("TURN relay"),
            (Some(_), Some(_)) => Some("P2P direct"),
            _ => None,
        }
    }
}
/// Observe at one-second intervals. Missing measurements are unavailable, never
/// synthesized from ICE configuration. Three pressured samples trigger a step,
/// followed by five samples of cooldown; the agent applies it to the helper.
/// Fifteen clean samples restore the previous step, back up to the requested profile.
pub struct AdaptiveProfile {
    profile: binachicken_protocol::VideoProfile,
    lowered: Vec<binachicken_protocol::VideoProfile>,
    strikes: u8,
    clean: u8,
    cooldown: u8,
    dropped: u64,
}
/// Loss is sampled before NACK/RTX repair, so moderate loss is mostly invisible.
const LOSS_PRESSURE_PPM: u32 = 100_000;
const CLEAN_SAMPLES_TO_RESTORE: u8 = 15;
impl AdaptiveProfile {
    pub fn new(profile: binachicken_protocol::VideoProfile) -> crate::Result<Self> {
        profile.validate()?;
        Ok(Self {
            profile,
            lowered: Vec::new(),
            strikes: 0,
            clean: 0,
            cooldown: 0,
            dropped: 0,
        })
    }
    pub fn observe(&mut self, stats: &PeerStats) -> Option<binachicken_protocol::VideoProfile> {
        // Frames dropped while ICE is still checking have no network path yet;
        // they are not congestion and must not lower the session's quality.
        if stats.native.connection_path().is_none() {
            self.dropped = stats.dropped_encoded_frames;
            self.strikes = 0;
            self.clean = 0;
            return None;
        }
        // A full send queue that overflows is already counted as a dropped frame;
        // a momentary queue level only reflects bursty encoder output.
        let pressure = stats.dropped_encoded_frames > self.dropped
            || stats.native.loss_ppm.is_some_and(|n| n > LOSS_PRESSURE_PPM)
            || stats.native.rtt_ms.is_some_and(|n| n > 250);
        self.dropped = stats.dropped_encoded_frames;
        if self.cooldown > 0 {
            self.cooldown -= 1;
            return None;
        }
        self.strikes = if pressure { self.strikes + 1 } else { 0 };
        self.clean = if pressure {
            0
        } else {
            self.clean.saturating_add(1)
        };
        if self.clean >= CLEAN_SAMPLES_TO_RESTORE
            && let Some(previous) = self.lowered.pop()
        {
            self.clean = 0;
            self.cooldown = 5;
            self.profile = previous;
            return Some(previous);
        }
        if self.strikes < 3 {
            return None;
        }
        self.strikes = 0;
        self.cooldown = 5;
        let next = self.profile.downshift();
        if next == self.profile {
            return None;
        }
        self.lowered.push(self.profile);
        self.profile = next;
        Some(next)
    }
}
fn seconds_to_ms(record: &gst::StructureRef, field: &str) -> Option<u64> {
    record
        .get::<f64>(field)
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.)
        .map(|v| (v * 1000.) as u64)
}
fn add_counter(total: &mut Option<u64>, next: Option<u64>) {
    if let Some(next) = next {
        *total = Some(total.unwrap_or(0).saturating_add(next));
    }
}
pub(crate) fn parse_native(report: &gst::StructureRef) -> NativeStats {
    use gst_webrtc::WebRTCStatsType as Kind;
    let mut stats = NativeStats::default();
    let mut selected_pair_rtt = None;
    for (_, value) in report.iter() {
        let Ok(record) = value.get::<gst::Structure>() else {
            continue;
        };
        match record.get::<Kind>("type").ok() {
            Some(Kind::RemoteInboundRtp) => {
                stats.rtt_ms = seconds_to_ms(&record, "round-trip-time").or(stats.rtt_ms);
                stats.loss_ppm = record
                    .get::<f64>("fraction-lost")
                    .ok()
                    .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                    .map(|v| (v * 1_000_000.) as u32);
            }
            Some(Kind::OutboundRtp) => {
                add_counter(&mut stats.packets_sent, record.get("packets-sent").ok());
                add_counter(&mut stats.bytes_sent, record.get("bytes-sent").ok());
            }
            Some(Kind::InboundRtp) => {
                add_counter(
                    &mut stats.packets_received,
                    record.get("packets-received").ok(),
                );
                add_counter(&mut stats.bytes_received, record.get("bytes-received").ok());
                let lost = record.get::<i64>("packets-lost").ok().or_else(|| {
                    record
                        .get::<u64>("packets-lost")
                        .ok()
                        .and_then(|n| i64::try_from(n).ok())
                });
                if let Some(lost) = lost {
                    stats.packets_lost = Some(stats.packets_lost.unwrap_or(0).saturating_add(lost));
                }
            }
            Some(Kind::Transport) => {
                let pair = record
                    .get::<&str>("selected-candidate-pair-id")
                    .ok()
                    .and_then(|id| report.get::<gst::Structure>(id).ok());
                if let Some(pair) = pair {
                    selected_pair_rtt =
                        seconds_to_ms(&pair, "current-round-trip-time").or(selected_pair_rtt);
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
    stats.rtt_ms = stats.rtt_ms.or(selected_pair_rtt);
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_classification_uses_the_selected_pair_and_never_guesses() {
        let mut stats = NativeStats::default();
        assert_eq!(stats.connection_path(), None);
        stats.local_candidate = Some(CandidateType::Host);
        assert_eq!(stats.connection_path(), None);
        for remote in [
            CandidateType::Host,
            CandidateType::ServerReflexive,
            CandidateType::PeerReflexive,
        ] {
            stats.remote_candidate = Some(remote);
            assert_eq!(stats.connection_path(), Some("P2P direct"));
        }
        stats.remote_candidate = Some(CandidateType::Relay);
        assert_eq!(stats.connection_path(), Some("TURN relay"));
        stats.local_candidate = Some(CandidateType::Relay);
        stats.remote_candidate = None;
        assert_eq!(stats.connection_path(), Some("TURN relay"));
    }

    #[test]
    fn rtp_counters_are_summed_once_and_signed_loss_is_preserved() {
        gst::init().unwrap();
        use gst_webrtc::WebRTCStatsType as Kind;
        let report = gst::Structure::builder("stats")
            .field(
                "in-a",
                gst::Structure::builder("inbound")
                    .field("type", Kind::InboundRtp)
                    .field("bytes-received", 4000u64)
                    .field("packets-received", 40u64)
                    .field("packets-lost", -2i64)
                    .build(),
            )
            .field(
                "in-b",
                gst::Structure::builder("inbound")
                    .field("type", Kind::InboundRtp)
                    .field("bytes-received", 6000u64)
                    .field("packets-received", 60u64)
                    .field("packets-lost", 3u64)
                    .build(),
            )
            .field(
                "out-a",
                gst::Structure::builder("outbound")
                    .field("type", Kind::OutboundRtp)
                    .field("bytes-sent", 900u64)
                    .field("packets-sent", 9u64)
                    .build(),
            )
            .field(
                "out-b",
                gst::Structure::builder("outbound")
                    .field("type", Kind::OutboundRtp)
                    .field("bytes-sent", 100u64)
                    .field("packets-sent", 1u64)
                    .build(),
            )
            // Remote reports describe the same traffic; never count it twice.
            .field(
                "remote",
                gst::Structure::builder("remote")
                    .field("type", Kind::RemoteOutboundRtp)
                    .field("bytes-sent", 99999u64)
                    .build(),
            )
            .build();
        let stats = parse_native(&report);
        assert_eq!(stats.bytes_received, Some(10000));
        assert_eq!(stats.packets_received, Some(100));
        assert_eq!(stats.packets_lost, Some(1));
        assert_eq!(stats.bytes_sent, Some(1000));
        assert_eq!(stats.packets_sent, Some(10));
        assert_eq!(
            stats.sampled_at_ms, None,
            "native publication supplies its monotonic time"
        );
        let reset = gst::Structure::builder("stats")
            .field(
                "in",
                gst::Structure::builder("inbound")
                    .field("type", Kind::InboundRtp)
                    .field("bytes-received", 0u64)
                    .field("packets-lost", -3i64)
                    .build(),
            )
            .build();
        let reset = parse_native(&reset);
        assert_eq!(reset.bytes_received, Some(0));
        assert_eq!(reset.packets_lost, Some(-3));
        assert_eq!(reset.bytes_sent, None);
        assert_eq!(reset.packets_received, None);
    }

    #[test]
    fn missing_or_invalid_counters_and_unselected_rtt_stay_unknown() {
        gst::init().unwrap();
        use gst_webrtc::WebRTCStatsType as Kind;
        let report = gst::Structure::builder("stats")
            .field(
                "in",
                gst::Structure::builder("inbound")
                    .field("type", Kind::InboundRtp)
                    .field("bytes-received", -4i64)
                    .field("packets-lost", u64::MAX)
                    .build(),
            )
            .field(
                "unselected",
                gst::Structure::builder("pair")
                    .field("type", Kind::CandidatePair)
                    .field("current-round-trip-time", 0.1f64)
                    .build(),
            )
            .build();
        let stats = parse_native(&report);
        assert_eq!(stats.bytes_received, None);
        assert_eq!(stats.packets_lost, None);
        assert_eq!(stats.rtt_ms, None);
    }

    #[test]
    fn selected_pair_rtt_is_only_a_fallback_for_valid_remote_rtcp_rtt() {
        gst::init().unwrap();
        use gst_webrtc::WebRTCStatsType as Kind;
        let mut report = gst::Structure::builder("stats")
            .field(
                "transport",
                gst::Structure::builder("transport")
                    .field("type", Kind::Transport)
                    .field("selected-candidate-pair-id", "pair")
                    .build(),
            )
            .field(
                "pair",
                gst::Structure::builder("pair")
                    .field("type", Kind::CandidatePair)
                    .field("current-round-trip-time", 0.075f64)
                    .build(),
            )
            .build();
        assert_eq!(parse_native(&report).rtt_ms, Some(75));
        report.set(
            "rtcp",
            gst::Structure::builder("rtcp")
                .field("type", Kind::RemoteInboundRtp)
                .field("round-trip-time", 0.125f64)
                .build(),
        );
        assert_eq!(parse_native(&report).rtt_ms, Some(125));
        report.set(
            "rtcp",
            gst::Structure::builder("rtcp")
                .field("type", Kind::RemoteInboundRtp)
                .field("round-trip-time", f64::NAN)
                .build(),
        );
        assert_eq!(parse_native(&report).rtt_ms, Some(75));
        report.set(
            "pair",
            gst::Structure::builder("pair")
                .field("type", Kind::CandidatePair)
                .field("current-round-trip-time", -1.0f64)
                .build(),
        );
        assert_eq!(parse_native(&report).rtt_ms, None);
    }

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
