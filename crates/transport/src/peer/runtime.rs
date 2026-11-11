use super::signaling::configure_ice;
use super::*;
pub(super) fn build_engine(shared: &Arc<Shared>, epoch: u64) -> Result<Arc<Engine>> {
    let config = shared.config.lock().unwrap().clone();
    let webrtc = gst::ElementFactory::make("webrtcbin")
        .property("bundle-policy", gst_webrtc::WebRTCBundlePolicy::MaxBundle)
        .property("latency", 40u32)
        .property(
            "ice-transport-policy",
            if config.relay_only {
                gst_webrtc::WebRTCICETransportPolicy::Relay
            } else {
                gst_webrtc::WebRTCICETransportPolicy::All
            },
        )
        .build()
        .map_err(|_| TransportError::NativeDependency)?;
    configure_ice(&webrtc, &config.ice_servers)?;
    webrtc
        .clone()
        .downcast::<gst::Bin>()
        .map_err(|_| TransportError::NativeDependency)?
        .connect_deep_element_added(|_, _, element| {
            if element
                .factory()
                .is_some_and(|f| f.name() == "rtpjitterbuffer")
            {
                element.set_property("drop-on-latency", true);
            }
        });
    let pipeline = gst::Pipeline::new();
    pipeline
        .add(&webrtc)
        .map_err(|_| TransportError::NativeOperation)?;
    let engine = Arc::new(Engine {
        epoch,
        pipeline,
        webrtc,
        channels: Mutex::new(HashMap::new()),
        signaling: Mutex::new(Signaling::default()),
        negotiating: AtomicBool::new(false),
        stats_pending: AtomicBool::new(false),
        keyframe_requested: AtomicBool::new(false),
        video: Mutex::new(None),
    });
    let weak = Arc::downgrade(shared);
    engine
        .webrtc
        .connect("on-ice-candidate", false, move |values| {
            if let Some(shared) = active(&weak, epoch)
                && let (Ok(index), Ok(candidate)) =
                    (values[1].get::<u32>(), values[2].get::<String>())
                && let Ok(index) = u16::try_from(index)
            {
                let session_id = shared.config.lock().unwrap().session_id;
                shared.emit_at(
                    epoch,
                    PeerEvent::Signal(SignalMessage::IceCandidate {
                        session_id,
                        epoch,
                        candidate,
                        sdp_mid: None,
                        sdp_mline_index: Some(index),
                    }),
                );
            }
            None
        });
    let weak = Arc::downgrade(shared);
    let weak_engine = Arc::downgrade(&engine);
    engine
        .webrtc
        .connect("prepare-data-channel", false, move |values| {
            if let (Some(shared), Some(engine)) = (active(&weak, epoch), weak_engine.upgrade())
                && let Ok(channel) = values[1].get::<gst_webrtc::WebRTCDataChannel>()
            {
                attach_channel(&shared, &engine, channel);
            }
            None
        });
    let weak = Arc::downgrade(shared);
    let weak_engine = Arc::downgrade(&engine);
    engine
        .webrtc
        .connect("on-data-channel", false, move |values| {
            if let (Some(shared), Some(engine)) = (active(&weak, epoch), weak_engine.upgrade())
                && let Ok(channel) = values[1].get::<gst_webrtc::WebRTCDataChannel>()
            {
                install_channel(&shared, &engine, &channel);
            }
            None
        });
    let weak = Arc::downgrade(shared);
    engine
        .webrtc
        .connect_notify(Some("connection-state"), move |webrtc, _| {
            if let Some(shared) = active(&weak, epoch) {
                let state = match webrtc
                    .property::<gst_webrtc::WebRTCPeerConnectionState>("connection-state")
                {
                    gst_webrtc::WebRTCPeerConnectionState::New => ConnectionState::New,
                    gst_webrtc::WebRTCPeerConnectionState::Connecting => {
                        ConnectionState::Connecting
                    }
                    gst_webrtc::WebRTCPeerConnectionState::Connected => ConnectionState::Connected,
                    gst_webrtc::WebRTCPeerConnectionState::Disconnected => {
                        ConnectionState::Disconnected
                    }
                    gst_webrtc::WebRTCPeerConnectionState::Closed => ConnectionState::Closed,
                    _ => ConnectionState::Failed,
                };
                shared.emit_at(epoch, PeerEvent::ConnectionChanged(state));
                if matches!(state, ConnectionState::Closed | ConnectionState::Failed) {
                    shared.close_at(epoch);
                }
            }
        });
    let weak = Arc::downgrade(shared);
    engine
        .webrtc
        .connect("on-negotiation-needed", false, move |_| {
            if let Some(s) = active(&weak, epoch)
                && s.negotiation_ready.load(Ordering::Acquire)
            {
                s.emit_at(epoch, PeerEvent::NegotiationNeeded);
            }
            None
        });
    if shared.video_enabled.load(Ordering::Acquire) {
        video::install(shared, &engine)?;
    }
    if engine.pipeline.set_state(gst::State::Playing).is_err() {
        let _ = engine.pipeline.set_state(gst::State::Null);
        return Err(TransportError::NativeOperation);
    }
    if config.role == PeerRole::Controller {
        for spec in [&MOTION, &CONTROL, &SYNC] {
            let mut options = gst::Structure::builder("application/data-channel")
                .field("ordered", spec.ordered)
                .field(
                    "priority",
                    if spec.ordered {
                        gst_webrtc::WebRTCPriorityType::High
                    } else {
                        gst_webrtc::WebRTCPriorityType::Low
                    },
                )
                .build();
            if let Some(retransmits) = spec.max_retransmits {
                options.set("max-retransmits", retransmits);
            }
            let channel: Option<gst_webrtc::WebRTCDataChannel> = engine
                .webrtc
                .emit_by_name("create-data-channel", &[&spec.label, &options]);
            if let Some(channel) = channel {
                install_channel(shared, &engine, &channel);
            } else {
                let _ = engine.pipeline.set_state(gst::State::Null);
                return Err(TransportError::NativeOperation);
            }
        }
    }
    Ok(engine)
}

fn install_channel(
    shared: &Arc<Shared>,
    engine: &Arc<Engine>,
    channel: &gst_webrtc::WebRTCDataChannel,
) {
    let epoch = engine.epoch;
    let label = channel.label().map(|s| s.to_string()).unwrap_or_default();
    let spec = if label == MOTION.label {
        &MOTION
    } else if label == CONTROL.label {
        &CONTROL
    } else if label == SYNC.label {
        &SYNC
    } else {
        channel.close();
        shared.fail_at(epoch, TransportError::NativeOperation);
        return;
    };
    let mut channels = engine.channels.lock().unwrap();
    if channel.is_ordered() != spec.ordered
        || channel.max_retransmits() != spec.max_retransmits.unwrap_or(-1)
        || channel.max_packet_lifetime() != -1
        || channels.get(&label).is_some_and(|old| old != channel)
    {
        drop(channels);
        channel.close();
        shared.fail_at(epoch, TransportError::NativeOperation);
        return;
    }
    channels.insert(label, channel.clone());
}
fn attach_channel(
    shared: &Arc<Shared>,
    engine: &Arc<Engine>,
    channel: gst_webrtc::WebRTCDataChannel,
) {
    // Remote labels are absent in prepare-data-channel until DCEP has arrived.
    // Register all handlers now; on-data-channel installs validated metadata.
    let epoch = engine.epoch;
    let weak = Arc::downgrade(shared);
    let weak_engine = Arc::downgrade(engine);
    channel.connect_on_open(move |channel| {
        if let (Some(shared), Some(engine)) = (active(&weak, epoch), weak_engine.upgrade()) {
            install_channel(&shared, &engine, channel);
            let channels = engine.channels.lock().unwrap();
            if channels.len() == 3
                && channels
                    .values()
                    .all(|c| c.ready_state() == gst_webrtc::WebRTCDataChannelState::Open)
            {
                shared.emit_at(epoch, PeerEvent::ChannelsReady);
            }
        }
    });
    let weak = Arc::downgrade(shared);
    channel.connect_on_message_data(move |channel, bytes| {
        if let Some(shared) = active(&weak, epoch) {
            let label = channel.label().map(|s| s.to_string()).unwrap_or_default();
            let bytes = bytes.map(|b| b.as_ref()).unwrap_or(&[]);
            if label == SYNC.label {
                receive_sync(&shared, epoch, bytes);
                return;
            }
            receive_input(&shared, epoch, &label, bytes);
        }
    });
    let weak = Arc::downgrade(shared);
    channel.connect_on_message_string(move |channel, text| {
        if let Some(shared) = active(&weak, epoch) {
            receive_input(
                &shared,
                epoch,
                channel.label().as_deref().unwrap_or(""),
                text.unwrap_or("").as_bytes(),
            );
        }
    });
    let weak = Arc::downgrade(shared);
    channel.connect_on_error(move |_, _| {
        if let Some(s) = active(&weak, epoch) {
            s.fail_at(epoch, TransportError::NativeOperation);
        }
    });
    let weak = Arc::downgrade(shared);
    channel.connect_on_close(move |_| {
        if let Some(s) = active(&weak, epoch) {
            s.close_at(epoch);
        }
    });
}

/// Never blocks the SCTP callback. The sender's window bounds the backlog, so an
/// overflowing queue means the peer ignores flow control and the session is dropped.
fn receive_sync(shared: &Arc<Shared>, epoch: u64, bytes: &[u8]) {
    if bytes.len() > SYNC_FRAME_LIMIT {
        shared.fail_at(epoch, ProtocolError::ControlTooLarge.into());
        return;
    }
    if !shared.current(epoch) {
        return;
    }
    shared.counters.received.fetch_add(1, Ordering::Relaxed);
    if shared.sync_tx.try_send(bytes.to_vec()).is_err() {
        shared.fail_at(epoch, TransportError::QueueFull);
    }
}

fn receive_input(shared: &Arc<Shared>, epoch: u64, label: &str, bytes: &[u8]) {
    if shared.config.lock().unwrap().role != PeerRole::Agent {
        shared.fail_at(epoch, TransportError::WrongRole);
        return;
    }
    // Serialize validation and publication across both SCTP callback threads.
    let mut decoder = shared.input.lock().unwrap();
    if !shared.current(epoch) {
        return;
    }
    match decoder.decode(label, bytes, shared.now_ms()) {
        Ok(input) => {
            shared.counters.received.fetch_add(1, Ordering::Relaxed);
            if label == MOTION.label {
                if shared
                    .incoming_motion
                    .lock()
                    .unwrap()
                    .replace(input)
                    .is_some()
                {
                    shared.counters.motion.fetch_add(1, Ordering::Relaxed);
                }
                shared.flush_motion();
            } else {
                if matches!(
                    input.event,
                    InputEvent::Button { .. } | InputEvent::Scroll { .. }
                ) {
                    let mut motion = shared.incoming_motion.lock().unwrap();
                    if motion
                        .as_ref()
                        .is_some_and(|m| m.sequence <= input.sequence)
                    {
                        motion.take();
                    }
                }
                shared.emit_at(epoch, PeerEvent::Input(input));
            }
        }
        Err(error) => {
            shared.counters.rejected.fetch_add(1, Ordering::Relaxed);
            shared.emit_at(epoch, PeerEvent::InputRejected(error));
        }
    }
}

pub(super) fn monitor_peer(shared: Arc<Shared>) {
    let mut stats_at = Instant::now();
    while !shared.closed.load(Ordering::Acquire) {
        #[cfg(test)]
        shared.hooks.monitor_before_lock.pause();
        let rebuild_guard = shared.rebuild.lock().unwrap();
        if shared.closed.load(Ordering::Acquire) {
            break;
        }
        if shared.input.lock().unwrap().expired(shared.now_ms()) {
            shared.fail(ProtocolError::LeaseExpired.into());
            break;
        }
        let engine = shared.engine.lock().unwrap().clone();
        if let Some(engine) = engine {
            if let Some(bus) = engine.pipeline.bus() {
                for message in bus.iter() {
                    if matches!(message.view(), gst::MessageView::Error(_))
                        && shared.current(engine.epoch)
                    {
                        shared.fail(TransportError::NativeOperation);
                    }
                }
            }
            if let Some(bytes) = shared.outgoing_motion.lock().unwrap().take() {
                let channel = engine.channels.lock().unwrap().get(MOTION.label).cloned();
                if let Some(channel) = channel.filter(|c| {
                    c.ready_state() == gst_webrtc::WebRTCDataChannelState::Open
                        && c.buffered_amount().saturating_add(bytes.len() as u64) <= MAX_SCTP_BUFFER
                }) {
                    if channel
                        .send_data_full(Some(&gst::glib::Bytes::from_owned(bytes)))
                        .is_ok()
                    {
                        shared.counters.sent.fetch_add(1, Ordering::Relaxed);
                    } else {
                        shared.counters.motion.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    shared.counters.motion.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        shared.flush_motion();
        shared.flush_frame();
        if stats_at.elapsed() >= Duration::from_secs(1) {
            if let Ok(engine) = shared.engine() {
                video::query_stats(&shared, &engine);
            }
            shared.low_priority(PeerEvent::Stats(shared.counters.snapshot()));
            stats_at = Instant::now();
        }
        drop(rebuild_guard);
        // Direct paths handle the common case; this only drains coalesced backlog.
        thread::sleep(Duration::from_millis(2));
    }
    // Teardown and replacement publication share one lifecycle transaction.
    // close() joins only after releasing the event/lifecycle gates.
    let _rebuild = shared.rebuild.lock().unwrap();
    let engine = shared.engine.lock().unwrap().take();
    if let Some(engine) = engine {
        let _ = engine.pipeline.set_state(gst::State::Null);
    }
    let _ = shared
        .events
        .try_send(PeerEvent::ConnectionChanged(ConnectionState::Closed));
    shared.shutdown_complete.store(true, Ordering::Release);
}
