use crate::{Result, video::LatestFrame};
use remotevnc_protocol::*;
use remotevnc_transport::{ConnectionState, Peer, PeerConfig, PeerEvent, PeerRole, TransportError};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;
/// Convert absolute authenticated expiry to monotonic authority; repeated grants
/// and display/socket activity cannot move this deadline.
#[derive(Default)]
pub struct Authority {
    pub deadline_ms: u64,
    expires_at: u64,
}
impl Authority {
    pub fn grant(&mut self, expires: u64, unix: u64, now: u64, request_start: u64) -> bool {
        if expires <= unix || (self.expires_at != 0 && self.expired(now)) {
            return false;
        }
        if expires <= self.expires_at {
            return true;
        }
        self.deadline_ms = now
            .saturating_add(expires - unix)
            .min(request_start.saturating_add(60_000));
        self.expires_at = expires;
        !self.expired(now)
    }
    pub fn expired(&self, now: u64) -> bool {
        now >= self.deadline_ms
    }
}
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// The native Peer and session authority live off the UI thread. Public methods
/// expose the same boundary used by the SDK worker and native loopback tests.
pub struct ControllerSession {
    pub grant: SessionGrant,
    pub peer: Arc<Peer>,
    events: remotevnc_transport::Receiver<PeerEvent>,
    pub display: Option<DisplayInfo>,
    pub ready: bool,
    authority: Authority,
    clock: Instant,
    unix_origin: u64,
    absolute_deadline: Arc<AtomicU64>,
    deadline: Arc<AtomicU64>,
    stop: CancellationToken,
    sequence: u64,
    installed: u64,
    ice: Vec<IceServer>,
    negotiation: Option<Instant>,
    frame_at: Instant,
    offered: bool,
    pub frames: LatestFrame,
}
impl ControllerSession {
    pub fn new(
        grant: SessionGrant,
        request_start: Instant,
        relay_only: bool,
        frames: LatestFrame,
    ) -> Result<Self> {
        let mut authority = Authority::default();
        let clock = request_start;
        let now = clock.elapsed().as_millis() as u64;
        if !authority.grant(grant.lease_expires_at, unix_ms(), now, 0) {
            return Err("expired session grant".into());
        }
        let mut peer = Peer::new(PeerConfig {
            session_id: grant.session_id,
            local_peer_id: grant.client_id,
            remote_peer_id: grant.agent_id,
            role: PeerRole::Controller,
            display: None,
            ice_servers: grant.ice_servers.clone(),
            relay_only,
        })?;
        peer.enable_h264()?;
        let events = peer.events().clone();
        let peer = Arc::new(peer);
        let stop = CancellationToken::new();
        let deadline = Arc::new(AtomicU64::new(authority.deadline_ms));
        let unix_origin = unix_ms().saturating_sub(clock.elapsed().as_millis() as u64);
        let absolute_deadline = Arc::new(AtomicU64::new(grant.lease_expires_at));
        let watched_absolute = absolute_deadline.clone();
        let (watched_peer, watched_stop, watched_deadline) =
            (peer.clone(), stop.clone(), deadline.clone());
        std::thread::Builder::new()
            .name("client-authority".into())
            .spawn(move || {
                while !watched_stop.is_cancelled()
                    && unix_ms() < watched_absolute.load(Ordering::Acquire)
                    && (clock.elapsed().as_millis() as u64)
                        < watched_deadline.load(Ordering::Acquire)
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                watched_peer.close();
            })?;
        Ok(Self {
            ice: grant.ice_servers.clone(),
            grant,
            peer,
            events,
            display: None,
            ready: false,
            authority,
            clock,
            unix_origin,
            absolute_deadline,
            deadline,
            stop,
            sequence: 0,
            installed: 0,
            negotiation: Some(Instant::now() + Duration::from_secs(20)),
            frame_at: Instant::now(),
            offered: false,
            frames,
        })
    }
    pub fn refreshed(&mut self, grant: SessionGrant) -> Result<()> {
        if grant.session_id != self.grant.session_id
            || grant.client_id != self.grant.client_id
            || grant.agent_id != self.grant.agent_id
        {
            return Err("grant scope mismatch".into());
        }
        let now = self.clock.elapsed().as_millis() as u64;
        let newer = grant.lease_expires_at > self.grant.lease_expires_at;
        if unix_ms().max(self.unix_origin.saturating_add(now)) >= self.grant.lease_expires_at
            || !self.authority.grant(
                grant.lease_expires_at,
                unix_ms().max(self.unix_origin.saturating_add(now)),
                now,
                now,
            )
        {
            return Err("expired session grant".into());
        }
        if newer {
            self.peer.renew_input_lease()?;
            self.grant.lease_expires_at = grant.lease_expires_at;
            self.deadline
                .store(self.authority.deadline_ms, Ordering::Release);
            self.absolute_deadline
                .store(grant.lease_expires_at, Ordering::Release);
        }
        Ok(())
    }
    pub fn signal(&mut self, sender: DeviceId, message: SignalMessage) -> Result<()> {
        let (id, epoch) = signal_scope(&message);
        if sender != self.grant.agent_id || id != self.grant.session_id {
            return Err("wrong signaling scope".into());
        }
        if epoch < self.peer.epoch() {
            return Ok(());
        }
        let restart = matches!(message, SignalMessage::Restart { .. }) && epoch > self.peer.epoch();
        let initial = matches!(message, SignalMessage::DisplayUnavailable { .. }) && !self.offered;
        if restart {
            self.ready = false;
            self.frames.clear();
            self.negotiation = Some(Instant::now() + Duration::from_secs(20));
        }
        self.peer.handle_signal(message)?;
        if restart || initial {
            self.peer.start_negotiation()?;
            self.offered = true;
        }
        Ok(())
    }
    pub fn install_ice(
        &mut self,
        generation: u64,
        requires_ack: bool,
        ice: Vec<IceServer>,
    ) -> Result<Option<DeviceCommand>> {
        if generation < self.installed {
            return Ok(None);
        }
        if generation == self.installed && ice != self.ice {
            return Err("conflicting ICE configuration".into());
        }
        self.peer.install_ice(ice.clone())?;
        self.installed = generation;
        self.ice = ice;
        Ok(requires_ack.then_some(DeviceCommand::IceInstalled {
            session_id: self.grant.session_id,
            generation,
        }))
    }
    pub fn input(&mut self, generation: u64, epoch: u64, events: Vec<InputEvent>) -> Result<()> {
        if !self.ready
            || epoch != self.peer.epoch()
            || self.display.is_none_or(|d| d.generation != generation)
        {
            return Ok(());
        }
        for event in events {
            self.send(event)?;
        }
        Ok(())
    }
    fn send(&mut self, event: InputEvent) -> Result<()> {
        let d = self.display.ok_or("no physical display")?;
        if self
            .authority
            .expired(self.clock.elapsed().as_millis() as u64)
        {
            return Err("session expired".into());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("input sequence exhausted")?;
        self.peer.send_input(InputEnvelope {
            version: WIRE_VERSION,
            session_id: self.grant.session_id,
            generation: d.generation,
            sequence: self.sequence,
            event,
        })?;
        Ok(())
    }
    fn acknowledge(&mut self) -> Result<()> {
        if self.display.is_some() {
            match self.send(InputEvent::DisplayAck) {
                Ok(()) => {
                    self.ready = true;
                    self.negotiation = None;
                    self.frame_at = Instant::now();
                }
                Err(e) => {
                    if !e
                        .downcast_ref::<TransportError>()
                        .is_some_and(|e| *e == TransportError::NotReady)
                    {
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }
    pub fn poll(&mut self) -> Result<Vec<DeviceCommand>> {
        if self.peer.is_closed()
            || self
                .authority
                .expired(self.clock.elapsed().as_millis() as u64)
            || self.negotiation.is_some_and(|d| Instant::now() >= d)
            || (self.ready && self.frame_at.elapsed() > Duration::from_secs(20))
        {
            return Err("session ended; connect again".into());
        }
        let mut commands = vec![];
        for _ in 0..64 {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            match event {
                PeerEvent::Signal(message) => commands.push(DeviceCommand::Signal {
                    receiver: self.grant.agent_id,
                    message,
                }),
                PeerEvent::DisplayChanged(display) => {
                    self.ready = false;
                    self.display = Some(display);
                    self.frames.clear();
                    self.acknowledge()?;
                }
                PeerEvent::DisplayUnavailable { .. } => {
                    self.display = None;
                    self.ready = false;
                    self.frames.clear();
                    self.negotiation = None;
                }
                PeerEvent::InputReset { .. } => {
                    self.ready = false;
                    self.frames.clear();
                }
                PeerEvent::ChannelsReady => self.acknowledge()?,
                PeerEvent::Frame(frame)
                    if self
                        .display
                        .is_some_and(|d| d.generation == frame.generation) =>
                {
                    self.frame_at = Instant::now();
                    self.frames.replace(frame);
                }
                PeerEvent::ConnectionChanged(ConnectionState::Disconnected) => {
                    if self.ready {
                        let _ = self.send(InputEvent::ReleaseAll);
                    }
                    self.ready = false;
                    self.negotiation
                        .get_or_insert(Instant::now() + Duration::from_secs(10));
                }
                PeerEvent::ConnectionChanged(ConnectionState::Connected) => self.acknowledge()?,
                PeerEvent::ConnectionChanged(ConnectionState::Failed | ConnectionState::Closed)
                | PeerEvent::Error(_) => return Err("native connection ended".into()),
                _ => {}
            }
        }
        Ok(commands)
    }
}
impl Drop for ControllerSession {
    fn drop(&mut self) {
        if self.ready {
            let _ = self.send(InputEvent::ReleaseAll);
        }
        self.stop.cancel();
        self.peer.close();
        self.frames.clear();
    }
}
fn signal_scope(message: &SignalMessage) -> (SessionId, u64) {
    match message {
        SignalMessage::Display {
            session_id, epoch, ..
        }
        | SignalMessage::DisplayUnavailable {
            session_id, epoch, ..
        }
        | SignalMessage::Restart { session_id, epoch }
        | SignalMessage::Offer {
            session_id, epoch, ..
        }
        | SignalMessage::Answer {
            session_id, epoch, ..
        }
        | SignalMessage::IceCandidate {
            session_id, epoch, ..
        }
        | SignalMessage::End { session_id, epoch } => (*session_id, *epoch),
    }
}
