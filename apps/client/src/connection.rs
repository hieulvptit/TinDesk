//! Measurements from fresh native RTP reports; no interface-wide speed estimates.
use binachicken_protocol::SessionId;
use binachicken_transport::{ConnectionState, stats::NativeStats};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    Offline,
    Pending,
    Direct,
    DirectSlow,
    Relay,
    RelaySlow,
}
impl Badge {
    pub fn classify(connection: Option<ConnectionState>, stats: NativeStats, slow: bool) -> Self {
        match connection {
            Some(
                ConnectionState::Disconnected | ConnectionState::Failed | ConnectionState::Closed,
            ) => Self::Offline,
            Some(ConnectionState::Connected) => match (stats.connection_path(), slow) {
                (Some("P2P direct"), false) => Self::Direct,
                (Some("P2P direct"), true) => Self::DirectSlow,
                (Some("TURN relay"), false) => Self::Relay,
                (Some("TURN relay"), true) => Self::RelaySlow,
                _ => Self::Pending,
            },
            _ => Self::Pending,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "Disconnected",
            Self::Pending => "Connecting / measuring route",
            Self::Direct => "P2P direct",
            Self::DirectSlow => "P2P — slow connection",
            Self::Relay => "Via server (TURN)",
            Self::RelaySlow => "Via server — slow connection",
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Measurements {
    pub rx_mbps: Option<f64>,
    pub tx_mbps: Option<f64>,
    pub loss_ppm: Option<u32>,
    pub rtt_ms: Option<u64>,
    pub slow: bool,
}
#[derive(Default)]
pub struct Telemetry {
    scope: Option<(SessionId, u64)>,
    previous: Option<NativeStats>,
    values: Measurements,
    updated: f64,
    pressured: u8,
    recovered: u8,
}
impl Telemetry {
    pub fn observe(&mut self, scope: Option<(SessionId, u64)>, stats: NativeStats, now: f64) {
        if scope != self.scope {
            *self = Self {
                scope,
                ..Default::default()
            };
        }
        let Some(timestamp) = stats.sampled_at_ms.filter(|_| scope.is_some()) else {
            return;
        };
        if self.previous.and_then(|p| p.sampled_at_ms) == Some(timestamp) {
            return;
        }
        if self.previous.is_some() && now - self.updated > 3. {
            self.previous = None;
            self.pressured = 0;
            self.recovered = 0;
            self.values = Measurements::default();
        }
        let previous = self.previous.replace(stats);
        let mut values = Measurements {
            rtt_ms: stats.rtt_ms,
            loss_ppm: stats.loss_ppm,
            ..Default::default()
        };
        if let Some(previous) = previous
            && let Some(elapsed) = previous
                .sampled_at_ms
                .and_then(|t| timestamp.checked_sub(t))
                .filter(|t| *t > 0)
        {
            values.rx_mbps = bitrate(previous.bytes_received, stats.bytes_received, elapsed);
            values.tx_mbps = bitrate(previous.bytes_sent, stats.bytes_sent, elapsed);
            if let (Some(before), Some(after), Some(received)) = (
                previous.packets_lost,
                stats.packets_lost,
                previous
                    .packets_received
                    .zip(stats.packets_received)
                    .and_then(|(a, b)| b.checked_sub(a)),
            ) && before >= 0
                && after >= before
            {
                let lost = (after - before) as f64;
                if received > 0 || lost > 0. {
                    values.loss_ppm = Some((lost / (received as f64 + lost) * 1_000_000.) as u32);
                }
            }
        } else {
            self.pressured = 0;
            self.recovered = 0;
            self.values.slow = false;
        }
        if values.rtt_ms.is_some() || values.loss_ppm.is_some() {
            let pressure = values.rtt_ms.is_some_and(|v| v > 250)
                || values.loss_ppm.is_some_and(|v| v > 50_000);
            self.pressured = if pressure {
                self.pressured.saturating_add(1)
            } else {
                0
            };
            self.recovered = if pressure {
                0
            } else {
                self.recovered.saturating_add(1)
            };
            if self.pressured >= 3 {
                self.values.slow = true;
            }
            if self.recovered >= 3 {
                self.values.slow = false;
            }
        }
        values.slow = self.values.slow;
        self.values = values;
        self.updated = now;
    }
    pub fn current(&self, now: f64) -> Measurements {
        if self.previous.is_none() || now - self.updated > 3. {
            Measurements::default()
        } else {
            self.values
        }
    }
}
fn bitrate(before: Option<u64>, after: Option<u64>, elapsed_ms: u64) -> Option<f64> {
    Some(after?.checked_sub(before?)? as f64 * 8. / elapsed_ms as f64 / 1000.)
}
pub fn speed(mbps: Option<f64>) -> String {
    match mbps {
        Some(value) if value < 1. => format!("{:.0} kb/s", value * 1000.),
        Some(value) => format!("{value:.1} Mb/s"),
        None => "—".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rates_use_fresh_samples_and_reset_across_connections_or_counter_resets() {
        let mut t = Telemetry::default();
        let scope = Some((SessionId::from_uuid(uuid::Uuid::nil()), 1));
        let mut stats = NativeStats {
            sampled_at_ms: Some(1000),
            bytes_received: Some(100),
            ..Default::default()
        };
        t.observe(scope, stats, 1.);
        assert_eq!(t.current(1.).rx_mbps, None);
        stats.sampled_at_ms = Some(2000);
        stats.bytes_received = Some(125100);
        t.observe(scope, stats, 2.);
        assert_eq!(t.current(2.).rx_mbps, Some(1.));
        t.observe(scope, stats, 2.1);
        assert_eq!(
            t.current(2.1).rx_mbps,
            Some(1.),
            "duplicate polling cannot zero the speed"
        );
        assert_eq!(t.current(6.).rx_mbps, None, "stale reports are unavailable");
        stats.sampled_at_ms = Some(3000);
        stats.bytes_received = Some(1);
        t.observe(scope, stats, 6.);
        assert_eq!(t.current(6.).rx_mbps, None);
        t.observe(scope.map(|s| (s.0, 2)), stats, 7.);
        assert_eq!(t.current(7.).rx_mbps, None);
    }
    #[test]
    fn quality_requires_sustained_measurements_and_color_follows_selected_route() {
        let mut t = Telemetry::default();
        let scope = Some((SessionId::from_uuid(uuid::Uuid::nil()), 1));
        let mut stats = NativeStats {
            local_candidate: Some(binachicken_transport::stats::CandidateType::Host),
            remote_candidate: Some(binachicken_transport::stats::CandidateType::Host),
            rtt_ms: Some(300),
            ..Default::default()
        };
        for n in 1..=3 {
            stats.sampled_at_ms = Some(n * 1000);
            t.observe(scope, stats, n as f64);
        }
        assert!(t.current(3.).slow);
        assert_eq!(
            Badge::classify(Some(ConnectionState::Connected), stats, true),
            Badge::DirectSlow
        );
        stats.remote_candidate = Some(binachicken_transport::stats::CandidateType::Relay);
        assert_eq!(
            Badge::classify(Some(ConnectionState::Connected), stats, true),
            Badge::RelaySlow
        );
        assert_eq!(
            Badge::classify(Some(ConnectionState::Disconnected), stats, false),
            Badge::Offline
        );
        stats.sampled_at_ms = Some(3500);
        t.observe(scope, stats, 8.);
        assert!(
            !t.current(8.).slow,
            "a reporting gap resets the pressure streak"
        );
        for n in 4..=6 {
            stats.sampled_at_ms = Some(n * 1000);
            stats.rtt_ms = Some(20);
            t.observe(scope, stats, n as f64 + 5.);
        }
        assert!(!t.current(11.).slow);
    }
}
