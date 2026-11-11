use crate::{
    Result, TransportError,
    channels::{CONTROL, InputDecoder, MOTION, SYNC, channel_for},
    stats::{Counters, PeerStats},
};
use binachicken_protocol::*;
use crossbeam_channel::{Receiver, Sender, bounded};
use gst::prelude::*;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const EVENT_CAPACITY: usize = 130;
const CRITICAL_LIMIT: usize = 128; // two slots reserved for terminal error/closed
// At most one queued low-priority event plus one latest motion/frame slot.
const LOW_PRIORITY_LIMIT: usize = 1;
const MAX_EARLY_ICE: usize = 128;
const MAX_SCTP_BUFFER: u64 = 256 * 1024;
/// Local send backlog for sync frames; callers retry on QueueFull instead of failing.
const MAX_SYNC_BUFFER: u64 = 512 * 1024;
/// Peers are window-limited (SYNC_WINDOW per transfer); beyond this they violate flow control.
const SYNC_QUEUE_CAPACITY: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    Agent,
    Controller,
}
#[derive(Debug, Clone)]
pub struct PeerConfig {
    pub session_id: SessionId,
    pub local_peer_id: DeviceId,
    pub remote_peer_id: DeviceId,
    pub role: PeerRole,
    /// None while the agent has no selected physical display.
    pub display: Option<DisplayInfo>,
    pub ice_servers: Vec<IceServer>,
    pub relay_only: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    New,
    Connecting,
    Connected,
    Disconnected,
    Failed,
    Restarting,
    Closed,
}
/// Frame contents can include passwords; deliberately no Debug implementation.
#[derive(Clone)]
pub struct DecodedFrame {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub pts_ns: Option<u64>,
}
impl DecodedFrame {
    /// Map a decoded image pixel into physical output coordinates. UI callers
    /// first remove letterboxing/viewport scaling; helper adds the RandR origin.
    pub fn physical_position(&self, display: DisplayInfo, x: u32, y: u32) -> Result<(u32, u32)> {
        display.validate()?;
        if self.generation != display.generation {
            return Err(ProtocolError::WrongGeneration.into());
        }
        if self.width == 0 || self.height == 0 || x >= self.width || y >= self.height {
            return Err(ProtocolError::InvalidCoordinates.into());
        }
        Ok((
            (u64::from(x) * u64::from(display.width) / u64::from(self.width)) as u32,
            (u64::from(y) * u64::from(display.height) / u64::from(self.height)) as u32,
        ))
    }
}
/// Explicit native tests hold only the video appsrc branch; Drop always unblocks it.
#[cfg(feature = "native-test")]
#[must_use]
pub struct NativeVideoHold {
    pad: gst::Pad,
    probe: Option<gst::PadProbeId>,
}
#[cfg(feature = "native-test")]
impl Drop for NativeVideoHold {
    fn drop(&mut self) {
        if let Some(probe) = self.probe.take() {
            self.pad.remove_probe(probe);
        }
    }
}

/// Secret-bearing variants deliberately prevent deriving Debug for this enum.
pub enum PeerEvent {
    Signal(SignalMessage),
    Input(InputEnvelope),
    Frame(DecodedFrame),
    ConnectionChanged(ConnectionState),
    ChannelsReady,
    NegotiationNeeded,
    KeyframeNeeded,
    DisplayChanged(DisplayInfo),
    DisplayUnavailable {
        generation: u64,
    },
    /// Consumers clear locally held key/button state; Agent releases helper input.
    InputReset {
        generation: u64,
    },
    Stats(PeerStats),
    InputRejected(ProtocolError),
    Error(TransportError),
}

#[derive(Default)]
struct Signaling {
    remote_ready: bool,
    applying: bool,
    early_ice: Vec<(u32, String)>,
}
struct Engine {
    epoch: u64,
    pipeline: gst::Pipeline,
    webrtc: gst::Element,
    channels: Mutex<HashMap<String, gst_webrtc::WebRTCDataChannel>>,
    signaling: Mutex<Signaling>,
    negotiating: AtomicBool,
    stats_pending: AtomicBool,
    keyframe_requested: AtomicBool,
    video: Mutex<Option<video::SenderState>>,
}
struct Shared {
    #[cfg(test)]
    hooks: lifecycle_tests::Hooks,
    config: Mutex<PeerConfig>,
    display_generation: Mutex<Option<u64>>,
    video_enabled: AtomicBool,
    engine: Mutex<Option<Arc<Engine>>>,
    rebuild: Mutex<()>,
    epoch: AtomicU64,
    closed: AtomicBool,
    shutdown_complete: AtomicBool,
    negotiation_ready: AtomicBool,
    events: Sender<PeerEvent>,
    events_drain: Receiver<PeerEvent>,
    sync_tx: Sender<Vec<u8>>,
    event_gate: Mutex<()>,
    input: Mutex<InputDecoder>,
    send_gate: Mutex<()>,
    outgoing_motion: Mutex<Option<Vec<u8>>>,
    incoming_motion: Mutex<Option<InputEnvelope>>,
    frame: Mutex<Option<DecodedFrame>>,
    counters: Counters,
    started: Instant,
}
impl Shared {
    fn now_ms(&self) -> u64 {
        self.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    fn engine(&self) -> Result<Arc<Engine>> {
        if self.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        self.engine
            .lock()
            .unwrap()
            .clone()
            .ok_or(TransportError::NotReady)
    }
    fn current(self: &Arc<Self>, epoch: u64) -> bool {
        !self.closed.load(Ordering::Acquire) && self.epoch.load(Ordering::Acquire) == epoch
    }
    fn emit(&self, event: PeerEvent) {
        self.emit_at(self.epoch.load(Ordering::Acquire), event);
    }
    fn emit_at(&self, epoch: u64, event: PeerEvent) {
        let _gate = self.event_gate.lock().unwrap();
        if self.closed.load(Ordering::Acquire) || self.epoch.load(Ordering::Acquire) != epoch {
            return;
        }
        if self.events.len() >= CRITICAL_LIMIT || self.events.try_send(event).is_err() {
            self.closed.store(true, Ordering::Release);
            let _ = self
                .events
                .try_send(PeerEvent::Error(TransportError::QueueFull));
        }
    }
    fn fail(&self, error: TransportError) {
        self.fail_at(self.epoch.load(Ordering::Acquire), error);
    }
    fn fail_at(&self, epoch: u64, error: TransportError) {
        self.emit_at(epoch, PeerEvent::Error(error));
        self.close_at(epoch);
    }
    fn close_at(&self, epoch: u64) {
        let _gate = self.event_gate.lock().unwrap();
        if self.epoch.load(Ordering::Acquire) == epoch {
            self.closed.store(true, Ordering::Release);
        }
    }
    /// Caller holds the lifecycle mutex. Keep the input lock until callbacks
    /// are fenced and queued input/media are removed; callbacks publish under it.
    fn set_display_locked(&self, display: Option<DisplayInfo>, generation: u64) -> Result<()> {
        if let Some(display) = display {
            display.validate()?;
        }
        let mut high_water = self.display_generation.lock().unwrap();
        if high_water.is_some_and(|old| generation <= old) {
            if *high_water == Some(generation) && self.config.lock().unwrap().display == display {
                return Ok(());
            }
            return Err(ProtocolError::WrongGeneration.into());
        }
        let mut input = self.input.lock().unwrap();
        if let Some(display) = display {
            input.update_display(display)?;
        } else {
            input.clear_display();
        }
        {
            let _gate = self.event_gate.lock().unwrap();
            self.config.lock().unwrap().display = display;
            *high_water = Some(generation);
            self.incoming_motion.lock().unwrap().take();
            self.outgoing_motion.lock().unwrap().take();
            self.frame.lock().unwrap().take();
            let keep: Vec<_> = self
                .events_drain
                .try_iter()
                .filter(|event| !matches!(event, PeerEvent::Input(_) | PeerEvent::Frame(_)))
                .collect();
            for event in keep {
                let _ = self.events.try_send(event);
            }
        }
        drop(input);
        self.emit(PeerEvent::InputReset { generation });
        self.emit(match display {
            Some(d) => PeerEvent::DisplayChanged(d),
            None => PeerEvent::DisplayUnavailable { generation },
        });
        Ok(())
    }
    fn flush_frame(&self) {
        let _gate = self.event_gate.lock().unwrap();
        if self.closed.load(Ordering::Acquire) || self.events.len() >= LOW_PRIORITY_LIMIT {
            return;
        }
        // Keep the latest frame in its coalescing slot while the consumer is
        // busy. Only a newer decoded frame may supersede it, not a stats event.
        if let Some(frame) = self.frame.lock().unwrap().take() {
            let _ = self.events.try_send(PeerEvent::Frame(frame));
        }
    }
    /// Deliver the latest pointer position as soon as the queue has room. Unlike
    /// a dropped intermediate position, the final one must never be discarded.
    fn flush_motion(&self) {
        let _gate = self.event_gate.lock().unwrap();
        if self.closed.load(Ordering::Acquire) || self.events.len() >= LOW_PRIORITY_LIMIT {
            return;
        }
        if let Some(input) = self.incoming_motion.lock().unwrap().take() {
            let _ = self.events.try_send(PeerEvent::Input(input));
        }
    }
    fn low_priority(&self, event: PeerEvent) -> bool {
        let _gate = self.event_gate.lock().unwrap();
        !self.closed.load(Ordering::Acquire)
            && self.events.len() < LOW_PRIORITY_LIMIT
            && self.events.try_send(event).is_ok()
    }
}

pub struct Peer {
    shared: Arc<Shared>,
    events: Receiver<PeerEvent>,
    sync: Receiver<Vec<u8>>,
    monitor: Mutex<Option<JoinHandle<()>>>,
}
impl Peer {
    /// Builds a real pipeline, but does not offer until start_negotiation().
    /// Authentication and account/lease authorization belong to the caller.
    pub fn new(config: PeerConfig) -> Result<Self> {
        gst::init().map_err(|_| TransportError::NativeDependency)?;
        for plugin in ["webrtcbin", "nicesrc", "nicesink", "dtlsenc", "sctpenc"] {
            if gst::ElementFactory::find(plugin).is_none() {
                return Err(TransportError::NativeDependency);
            }
        }
        if config.local_peer_id == config.remote_peer_id {
            return Err(TransportError::WrongRole);
        }
        validate_ice(&config.ice_servers)?;
        let (tx, events) = bounded(EVENT_CAPACITY);
        let (sync_tx, sync) = bounded(SYNC_QUEUE_CAPACITY);
        let input = InputDecoder::new(config.session_id, config.display, 0)?;
        let shared = Arc::new(Shared {
            #[cfg(test)]
            hooks: lifecycle_tests::Hooks::default(),
            display_generation: Mutex::new(config.display.map(|d| d.generation)),
            config: Mutex::new(config),
            video_enabled: AtomicBool::new(false),
            engine: Mutex::new(None),
            rebuild: Mutex::new(()),
            epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
            negotiation_ready: AtomicBool::new(true),
            events: tx,
            events_drain: events.clone(),
            sync_tx,
            event_gate: Mutex::new(()),
            input: Mutex::new(input),
            send_gate: Mutex::new(()),
            outgoing_motion: Mutex::new(None),
            incoming_motion: Mutex::new(None),
            frame: Mutex::new(None),
            counters: Counters::default(),
            started: Instant::now(),
        });
        let engine = build_engine(&shared, 0)?;
        *shared.engine.lock().unwrap() = Some(engine);
        let monitor_shared = shared.clone();
        let monitor = match thread::Builder::new()
            .name("binachicken-peer".into())
            .spawn(move || monitor_peer(monitor_shared))
        {
            Ok(monitor) => monitor,
            Err(_) => {
                shared.closed.store(true, Ordering::Release);
                if let Some(engine) = shared.engine.lock().unwrap().as_ref() {
                    let _ = engine.pipeline.set_state(gst::State::Null);
                }
                return Err(TransportError::NativeOperation);
            }
        };
        let config = shared.config.lock().unwrap().clone();
        if config.role == PeerRole::Agent
            && let Some(display) = config.display
        {
            shared.emit(PeerEvent::Signal(SignalMessage::Display {
                session_id: config.session_id,
                epoch: 0,
                display,
            }));
        }
        Ok(Self {
            shared,
            events,
            sync,
            monitor: Mutex::new(Some(monitor)),
        })
    }
    pub fn events(&mut self) -> &mut Receiver<PeerEvent> {
        &mut self.events
    }
    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::Acquire)
    }
    pub fn stats(&self) -> PeerStats {
        self.shared.counters.snapshot()
    }
    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::Acquire)
            && self.shared.shutdown_complete.load(Ordering::Acquire)
            && self
                .shared
                .engine
                .lock()
                .unwrap()
                .as_ref()
                .is_none_or(|e| e.pipeline.current_state() == gst::State::Null)
    }
    /// Reads the native policy, allowing setup checks without exposing native handles.
    pub fn relay_only(&self) -> bool {
        self.shared
            .engine
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|e| {
                e.webrtc
                    .property::<gst_webrtc::WebRTCICETransportPolicy>("ice-transport-policy")
                    == gst_webrtc::WebRTCICETransportPolicy::Relay
            })
    }
    pub fn start_negotiation(&self) -> Result<()> {
        #[cfg(test)]
        self.shared.hooks.negotiation_requested.pause();
        let _rebuild = self.shared.rebuild.lock().unwrap();
        if self.shared.config.lock().unwrap().role != PeerRole::Controller {
            return Err(TransportError::WrongRole);
        }
        let engine = self.shared.engine()?;
        create_description(&self.shared, &engine, true)
    }
    pub fn handle_signal(&self, message: SignalMessage) -> Result<()> {
        encode_control(&message)?;
        let (session_id, epoch) = match &message {
            SignalMessage::Display {
                session_id, epoch, ..
            }
            | SignalMessage::DisplayUnavailable {
                session_id, epoch, ..
            }
            | SignalMessage::Offer {
                session_id, epoch, ..
            }
            | SignalMessage::Answer {
                session_id, epoch, ..
            }
            | SignalMessage::IceCandidate {
                session_id, epoch, ..
            }
            | SignalMessage::End { session_id, epoch }
            | SignalMessage::Restart { session_id, epoch } => (*session_id, *epoch),
        };
        if session_id != self.shared.config.lock().unwrap().session_id {
            return Err(ProtocolError::WrongSession.into());
        }
        if matches!(message, SignalMessage::Restart { .. }) {
            if epoch == self.epoch() {
                return Ok(());
            }
            if epoch
                != self
                    .epoch()
                    .checked_add(1)
                    .ok_or(TransportError::StaleEpoch)?
            {
                return Err(TransportError::StaleEpoch);
            }
            rebuild(&self.shared, epoch, None)?;
            return Ok(());
        }
        if epoch != self.epoch() {
            return Err(TransportError::StaleEpoch);
        }
        #[cfg(test)]
        self.shared.hooks.end_validated.pause();
        let rebuild_guard = self.shared.rebuild.lock().unwrap();
        if epoch != self.epoch() {
            return Err(TransportError::StaleEpoch);
        }
        if matches!(message, SignalMessage::End { .. }) {
            self.shared.close_at(epoch);
            drop(rebuild_guard);
            self.join_monitor();
            return Ok(());
        }
        let engine = self.shared.engine()?;
        match message {
            SignalMessage::End { .. } => unreachable!(),
            SignalMessage::Display { display, .. } => {
                if self.shared.config.lock().unwrap().role != PeerRole::Controller {
                    return Err(TransportError::WrongRole);
                }
                display.validate()?;
                if self.shared.config.lock().unwrap().display != Some(display) {
                    self.shared
                        .set_display_locked(Some(display), display.generation)?;
                }
            }
            SignalMessage::DisplayUnavailable { generation, .. } => {
                if self.shared.config.lock().unwrap().role != PeerRole::Controller {
                    return Err(TransportError::WrongRole);
                }
                self.shared.set_display_locked(None, generation)?;
            }
            SignalMessage::Offer { sdp, .. } => {
                if self.shared.config.lock().unwrap().role != PeerRole::Agent {
                    return Err(TransportError::WrongRole);
                }
                apply_remote(&self.shared, &engine, sdp, true)?;
            }
            SignalMessage::Answer { sdp, .. } => {
                if self.shared.config.lock().unwrap().role != PeerRole::Controller
                    || !engine.negotiating.load(Ordering::Acquire)
                {
                    return Err(TransportError::WrongRole);
                }
                apply_remote(&self.shared, &engine, sdp, false)?;
            }
            SignalMessage::IceCandidate {
                candidate,
                sdp_mline_index,
                ..
            } => {
                let index = u32::from(sdp_mline_index.ok_or(ProtocolError::InvalidPayload)?);
                if candidate.len() > 4096
                    || (!candidate.is_empty() && !candidate.starts_with("candidate:"))
                {
                    return Err(ProtocolError::InvalidPayload.into());
                }
                let mut signaling = engine.signaling.lock().unwrap();
                if signaling.remote_ready && !signaling.applying {
                    engine
                        .webrtc
                        .emit_by_name::<()>("add-ice-candidate", &[&index, &candidate]);
                } else if signaling.early_ice.len() < MAX_EARLY_ICE {
                    signaling.early_ice.push((index, candidate));
                } else {
                    return Err(TransportError::QueueFull);
                }
            }
            SignalMessage::Restart { .. } => unreachable!(),
        }
        Ok(())
    }
    /// Raw `sync` channel frames, in order. Reset any in-progress receive state
    /// on every `ChannelsReady`, which marks a fresh PeerConnection epoch.
    pub fn sync_events(&self) -> &Receiver<Vec<u8>> {
        &self.sync
    }
    /// Either role may send. `QueueFull` is backpressure, not a failure: retry later.
    pub fn send_sync(&self, frame: &[u8]) -> Result<()> {
        if frame.len() > SYNC_FRAME_LIMIT {
            return Err(TransportError::Protocol(ProtocolError::ControlTooLarge));
        }
        let _rebuild = self.shared.rebuild.lock().unwrap();
        let _send = self.shared.send_gate.lock().unwrap();
        let engine = self.shared.engine()?;
        let channel = engine
            .channels
            .lock()
            .unwrap()
            .get(SYNC.label)
            .cloned()
            .ok_or(TransportError::NotReady)?;
        if channel.ready_state() != gst_webrtc::WebRTCDataChannelState::Open {
            return Err(TransportError::NotReady);
        }
        if channel.buffered_amount().saturating_add(frame.len() as u64) > MAX_SYNC_BUFFER {
            return Err(TransportError::QueueFull);
        }
        channel
            .send_data_full(Some(&gst::glib::Bytes::from(frame)))
            .map_err(|_| TransportError::NativeOperation)?;
        self.shared.counters.sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub fn send_input(&self, input: InputEnvelope) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        let _send = self.shared.send_gate.lock().unwrap();
        let engine = self.shared.engine()?;
        if self.shared.config.lock().unwrap().role != PeerRole::Controller {
            return Err(TransportError::WrongRole);
        }
        let label = channel_for(&input.event);
        let channel = engine
            .channels
            .lock()
            .unwrap()
            .get(label)
            .cloned()
            .ok_or(TransportError::NotReady)?;
        if channel.ready_state() != gst_webrtc::WebRTCDataChannelState::Open {
            return Err(TransportError::NotReady);
        }
        let bytes = encode_control(&input)?;
        self.shared
            .input
            .lock()
            .unwrap()
            .decode(label, &bytes, self.shared.now_ms())?;
        if label == MOTION.label {
            // The monitor also holds the lifecycle lock, so a direct send cannot
            // race an older coalesced position. Coalesce only while SCTP is backed up.
            let mut pending = self.shared.outgoing_motion.lock().unwrap();
            if pending.take().is_some() {
                self.shared.counters.motion.fetch_add(1, Ordering::Relaxed);
            }
            if channel.buffered_amount() == 0
                && channel
                    .send_data_full(Some(&gst::glib::Bytes::from(&bytes[..])))
                    .is_ok()
            {
                self.shared.counters.sent.fetch_add(1, Ordering::Relaxed);
            } else {
                *pending = Some(bytes);
            }
            return Ok(());
        }
        if channel.buffered_amount().saturating_add(bytes.len() as u64) > MAX_SCTP_BUFFER {
            self.shared.fail(TransportError::QueueFull);
            return Err(TransportError::QueueFull);
        }
        channel
            .send_data_full(Some(&gst::glib::Bytes::from_owned(bytes)))
            .map_err(|_| TransportError::NativeOperation)?;
        self.shared.counters.sent.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    /// Only the Agent owns physical generation changes. Metadata and Restart
    /// are published in order within the same lifecycle transaction.
    pub fn update_display(&self, display: DisplayInfo) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        self.shared.engine()?;
        if self.shared.config.lock().unwrap().role != PeerRole::Agent {
            return Err(TransportError::WrongRole);
        }
        self.shared
            .set_display_locked(Some(display), display.generation)?;
        let epoch = self
            .epoch()
            .checked_add(1)
            .ok_or(TransportError::StaleEpoch)?;
        let ice = self.shared.config.lock().unwrap().ice_servers.clone();
        rebuild_locked(&self.shared, epoch, Some(ice))
    }
    /// Pause a missing physical display without ending the authorized session.
    /// The next available display must have a strictly newer helper generation.
    pub fn display_unavailable(&self, generation: u64) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        self.shared.engine()?;
        let config = self.shared.config.lock().unwrap().clone();
        if config.role != PeerRole::Agent {
            return Err(TransportError::WrongRole);
        }
        self.shared.set_display_locked(None, generation)?;
        self.shared
            .emit(PeerEvent::Signal(SignalMessage::DisplayUnavailable {
                session_id: config.session_id,
                epoch: self.epoch(),
                generation,
            }));
        Ok(())
    }
    /// Invoke only following a successful authorized backend lease renewal.
    pub fn renew_input_lease(&self) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        self.shared.engine()?;
        self.shared
            .input
            .lock()
            .unwrap()
            .renew_lease(self.shared.now_ms());
        Ok(())
    }
    /// Store validated ICE credentials for the next coordinated rebuild. This
    /// does not alter the active connection, signal a restart, or renew its lease.
    pub fn install_ice(&self, ice: Vec<IceServer>) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        self.shared.engine()?;
        validate_ice(&ice)?;
        self.shared.config.lock().unwrap().ice_servers = ice;
        Ok(())
    }
    /// GStreamer does not implement in-place ICE restart. This replaces the whole
    /// PeerConnection and sends a typed epoch marker before its new offer/ICE.
    pub fn restart_ice(&self, ice: Vec<IceServer>) -> Result<()> {
        validate_ice(&ice)?;
        let epoch = self
            .epoch()
            .checked_add(1)
            .ok_or(TransportError::StaleEpoch)?;
        rebuild(&self.shared, epoch, Some(ice))
    }
    /// Coalesce decoded frames; media never blocks input or shutdown. Generation
    /// must match current display metadata; encoded dimensions may be scaled.
    pub fn publish_frame(&self, frame: DecodedFrame) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        self.shared.engine()?;
        let display = self
            .shared
            .config
            .lock()
            .unwrap()
            .display
            .ok_or(ProtocolError::DisplayUnavailable)?;
        if frame.generation != display.generation {
            return Err(ProtocolError::WrongGeneration.into());
        }
        if frame.width == 0
            || frame.height == 0
            || frame.width > 3840
            || frame.height > 2160
            || frame.rgba.len() > 128 * 1024 * 1024
            || (frame.width as usize)
                .checked_mul(frame.height as usize)
                .and_then(|n| n.checked_mul(4))
                != Some(frame.rgba.len())
        {
            return Err(ProtocolError::InvalidPayload.into());
        }
        if self.shared.frame.lock().unwrap().replace(frame).is_some() {
            self.shared.counters.frames.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
    pub fn close(&self) {
        {
            let _gate = self.shared.event_gate.lock().unwrap();
            self.shared.closed.store(true, Ordering::Release);
        }
        #[cfg(test)]
        self.shared.hooks.close_requested.pause();
        self.join_monitor();
    }
    fn join_monitor(&self) {
        if let Some(monitor) = self.monitor.lock().unwrap().take() {
            let _ = monitor.join();
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.close();
    }
}

fn active(weak: &Weak<Shared>, epoch: u64) -> Option<Arc<Shared>> {
    weak.upgrade().filter(|s| s.current(epoch))
}

mod runtime;
mod signaling;
#[path = "video.rs"]
mod video;
use runtime::{build_engine, monitor_peer};
use signaling::{apply_remote, create_description, rebuild, rebuild_locked, validate_ice};

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn config(role: PeerRole) -> PeerConfig {
        PeerConfig {
            session_id: SessionId::from_uuid(uuid::Uuid::nil()),
            local_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(
                if role == PeerRole::Controller { 1 } else { 2 },
            )),
            remote_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(
                if role == PeerRole::Controller { 2 } else { 1 },
            )),
            role,
            display: Some(DisplayInfo {
                generation: 7,
                width: 100,
                height: 100,
            }),
            ice_servers: vec![],
            relay_only: false,
        }
    }
    fn input(event: InputEvent) -> InputEnvelope {
        InputEnvelope {
            version: WIRE_VERSION,
            session_id: config(PeerRole::Controller).session_id,
            generation: 7,
            sequence: 0,
            event,
        }
    }
    fn connected() -> (Peer, Peer) {
        let a = Peer::new(config(PeerRole::Controller)).unwrap();
        let b = Peer::new(config(PeerRole::Agent)).unwrap();
        a.start_negotiation().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            for (from, to) in [(&a, &b), (&b, &a)] {
                for event in from.shared.events_drain.try_iter() {
                    match event {
                        PeerEvent::Signal(signal) => to.handle_signal(signal).unwrap(),
                        PeerEvent::Error(error) => panic!("{error}"),
                        _ => {}
                    }
                }
            }
            let ready = [&a, &b].iter().all(|p| {
                p.shared
                    .engine()
                    .unwrap()
                    .channels
                    .lock()
                    .unwrap()
                    .values()
                    .filter(|c| c.ready_state() == gst_webrtc::WebRTCDataChannelState::Open)
                    .count()
                    == 3
            });
            if ready {
                return (a, b);
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn actual_sctp_rejects_malformed_wrong_scope_and_native_oversize() {
        let (a, mut b) = connected();
        let control = a.shared.engine().unwrap().channels.lock().unwrap()[CONTROL.label].clone();
        let mut other_session = input(InputEvent::ReleaseAll);
        other_session.session_id = SessionId::from_uuid(uuid::Uuid::from_u128(99));
        let mut other_generation = input(InputEvent::ReleaseAll);
        other_generation.generation = 6;
        for bytes in [
            b"{bad".to_vec(),
            encode_control(&other_session).unwrap(),
            encode_control(&other_generation).unwrap(),
        ] {
            control
                .send_data_full(Some(&gst::glib::Bytes::from_owned(bytes)))
                .unwrap();
        }
        // The negotiated native SCTP maximum rejects this before sending or
        // allocating a receiving JSON payload; decoder tests cover hostile bytes.
        assert!(
            control
                .send_data_full(Some(&gst::glib::Bytes::from_owned(vec![
                    0;
                    CONTROL_PAYLOAD_LIMIT
                        + 1
                ])))
                .is_err()
        );
        let mut rejected = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while rejected.len() < 3 {
            match b.events().recv_timeout(Duration::from_millis(50)) {
                Ok(PeerEvent::InputRejected(error)) => rejected.push(error),
                Ok(PeerEvent::Input(_)) => panic!("invalid wire input was delivered"),
                Ok(PeerEvent::Error(error)) => panic!("{error}"),
                _ => {}
            }
            assert!(Instant::now() < deadline);
        }
        assert_eq!(
            rejected,
            [
                ProtocolError::InvalidPayload,
                ProtocolError::WrongSession,
                ProtocolError::WrongGeneration
            ]
        );
        a.send_input(input(InputEvent::DisplayAck)).unwrap();
        loop {
            if let Ok(PeerEvent::Input(event)) = b.events().recv_timeout(Duration::from_millis(50))
            {
                assert!(matches!(event.event, InputEvent::DisplayAck));
                break;
            }
            assert!(Instant::now() < deadline);
        }
        a.close();
        b.close();
    }

    #[test]
    fn stale_native_callbacks_cannot_publish_or_close_a_replacement_epoch() {
        let (a, mut b) = connected();
        b.restart_ice(vec![]).unwrap();
        // Simulate an old callback finishing after the real old pipeline teardown.
        b.shared
            .emit_at(0, PeerEvent::Input(input(InputEvent::ReleaseAll)));
        b.shared.fail_at(0, TransportError::NativeOperation);
        assert!(!b.shared.closed.load(Ordering::Acquire));
        assert!(
            !b.events()
                .try_iter()
                .any(|e| matches!(e, PeerEvent::Input(_) | PeerEvent::Error(_)))
        );
        a.close();
        b.close();
    }
}

#[cfg(test)]
mod lifecycle_tests;
