//! H264 access units remain separate from the reliable input channel.
use super::*;

pub(super) struct SenderState {
    source: gst::Element,
    generation: u64,
    needs_keyframe: bool,
    last_pts: Option<u64>,
    timestamp_origin: Option<(u64, u64)>,
}
fn caps() -> gst::Caps {
    gst::Caps::builder("application/x-rtp")
        .field("media", "video")
        .field("encoding-name", "H264")
        .field("clock-rate", 90000i32)
        .field("payload", 96i32)
        .build()
}
fn native<T, E>(result: std::result::Result<T, E>) -> Result<T> {
    result.map_err(|_| TransportError::NativeOperation)
}
impl Peer {
    /// Test-only downstream stall, independent of healthy input/signaling channels.
    #[cfg(feature = "native-test")]
    pub fn hold_video_for_native_test(&self) -> Result<NativeVideoHold> {
        let engine = self.shared.engine()?;
        let source = engine
            .video
            .lock()
            .unwrap()
            .as_ref()
            .ok_or(TransportError::WrongRole)?
            .source
            .clone();
        let pad = source
            .static_pad("src")
            .ok_or(TransportError::NativeOperation)?;
        let probe = pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
                gst::PadProbeReturn::Ok
            })
            .ok_or(TransportError::NativeOperation)?;
        Ok(NativeVideoHold {
            pad,
            probe: Some(probe),
        })
    }

    /// Configure both peers before the controller's first offer. Persists across
    /// native rebuilds and is reattached synchronously before a remote offer.
    pub fn enable_h264(&self) -> Result<()> {
        let _rebuild = self.shared.rebuild.lock().unwrap();
        let engine = self.shared.engine()?;
        if self.shared.video_enabled.load(Ordering::Acquire) {
            return Ok(());
        }
        if engine.negotiating.load(Ordering::Acquire)
            || engine.signaling.lock().unwrap().remote_ready
        {
            return Err(TransportError::NegotiationBusy);
        }
        install(&self.shared, &engine)?;
        self.shared.video_enabled.store(true, Ordering::Release);
        Ok(())
    }
    /// Nonblocking, at most two encoded access units queued. Following any drop,
    /// only a fresh config-bearing IDR is accepted; caller handles KeyframeNeeded.
    pub fn push_video(&self, frame: EncodedFrame) -> Result<()> {
        frame.validate()?;
        let _rebuild = self.shared.rebuild.lock().unwrap();
        let engine = self.shared.engine()?;
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
        let mut sender = engine.video.lock().unwrap();
        let sender = sender.as_mut().ok_or(TransportError::WrongRole)?;
        if sender.generation != frame.generation {
            return Err(ProtocolError::WrongGeneration.into());
        }
        if sender.needs_keyframe && !frame.is_config_keyframe() {
            if !engine.keyframe_requested.swap(true, Ordering::AcqRel) {
                self.shared.emit_at(engine.epoch, PeerEvent::KeyframeNeeded);
            }
            return Err(TransportError::NotReady);
        }
        if sender.source.property::<u64>("current-level-buffers") >= 2 {
            self.shared.counters.encoded.fetch_add(1, Ordering::Relaxed);
            if !engine.keyframe_requested.swap(true, Ordering::AcqRel) {
                self.shared.emit_at(engine.epoch, PeerEvent::KeyframeNeeded);
            }
            sender.needs_keyframe = true;
            return Err(TransportError::QueueFull);
        }
        let mut buffer = gst::Buffer::from_mut_slice(frame.data);
        let buffer_mut = buffer.get_mut().unwrap();
        // Capture and transport are separate live pipelines. Map their clock
        // origins once, preserving capture cadence rather than timestamping each
        // arrival (which would turn network/IPC jitter into video jitter).
        if sender.last_pts.is_some_and(|last| frame.pts_ns < last) {
            return Err(ProtocolError::InvalidPayload.into());
        }
        let origin = match sender.timestamp_origin {
            Some(origin) => origin,
            None => (
                frame.pts_ns,
                engine
                    .pipeline
                    .current_running_time()
                    // Live pipelines acquire a clock/base time asynchronously.
                    // Retain the keyframe gate and retry within the existing
                    // bounded video-recovery/authorization deadlines.
                    .ok_or(TransportError::NotReady)?
                    .nseconds(),
            ),
        };
        let pts = frame
            .pts_ns
            .checked_sub(origin.0)
            .and_then(|elapsed| origin.1.checked_add(elapsed))
            .ok_or(ProtocolError::InvalidPayload)?;
        buffer_mut.set_pts(gst::ClockTime::from_nseconds(pts));
        buffer_mut.set_dts(gst::ClockTime::from_nseconds(pts));
        if !frame.keyframe {
            buffer_mut.set_flags(gst::BufferFlags::DELTA_UNIT);
        }
        let flow: gst::FlowReturn = sender.source.emit_by_name("push-buffer", &[&buffer]);
        native(flow.into_result())?;
        sender.needs_keyframe = false;
        if frame.keyframe {
            engine.keyframe_requested.store(false, Ordering::Release);
        }
        sender.last_pts = Some(frame.pts_ns);
        sender.timestamp_origin = Some(origin);
        Ok(())
    }
}
pub(super) fn install(shared: &Arc<Shared>, engine: &Arc<Engine>) -> Result<()> {
    let config = shared.config.lock().unwrap().clone();
    let Some(display) = config.display else {
        return Ok(());
    };
    let generation = display.generation;
    if config.role == PeerRole::Agent {
        let bin = native(gst::parse::bin_from_description(
            "appsrc name=source is-live=true format=time block=false max-buffers=2 max-bytes=16777216 ! h264parse config-interval=-1 ! rtph264pay config-interval=-1 pt=96 mtu=1200 aggregate-mode=zero-latency ! capsfilter caps=\"application/x-rtp,media=video,encoding-name=H264,payload=96\"",
            true,
        ))?;
        let source = bin
            .by_name("source")
            .ok_or(TransportError::NativeOperation)?;
        source.set_property(
            "caps",
            gst::Caps::builder("video/x-h264")
                .field("stream-format", "byte-stream")
                .field("alignment", "au")
                .build(),
        );
        let weak = Arc::downgrade(shared);
        let epoch = engine.epoch;
        let weak_engine = Arc::downgrade(engine);
        source
            .static_pad("src")
            .ok_or(TransportError::NativeOperation)?
            .add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_, probe| {
                if let Some(gst::PadProbeData::Event(event)) = &probe.data
                    && event
                        .structure()
                        .is_some_and(|s| s.name() == "GstForceKeyUnit")
                    && let Some(shared) = active(&weak, epoch)
                    && let Some(engine) = weak_engine.upgrade()
                    && !engine.keyframe_requested.swap(true, Ordering::AcqRel)
                {
                    shared.emit_at(epoch, PeerEvent::KeyframeNeeded);
                }
                gst::PadProbeReturn::Ok
            });
        native(engine.pipeline.add(&bin))?;
        native(bin.link(&engine.webrtc))?;
        let transceiver = bin
            .static_pad("src")
            .and_then(|pad| pad.peer())
            .ok_or(TransportError::NativeOperation)?
            .property::<gst_webrtc::WebRTCRTPTransceiver>("transceiver");
        enable_nack(&transceiver);
        native(bin.sync_state_with_parent())?;
        *engine.video.lock().unwrap() = Some(SenderState {
            source,
            generation,
            needs_keyframe: true,
            last_pts: None,
            timestamp_origin: None,
        });
    } else {
        let transceiver: gst_webrtc::WebRTCRTPTransceiver = engine.webrtc.emit_by_name(
            "add-transceiver",
            &[
                &gst_webrtc::WebRTCRTPTransceiverDirection::Recvonly,
                &caps(),
            ],
        );
        enable_nack(&transceiver);
        let weak = Arc::downgrade(shared);
        let weak_engine = Arc::downgrade(engine);
        engine.webrtc.connect_pad_added(move |_, pad| {
            let (Some(shared), Some(engine)) = (
                active(&weak, engine_epoch(&weak_engine)),
                weak_engine.upgrade(),
            ) else {
                return;
            };
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            if let Err(error) = receive(&shared, &engine, pad, generation) {
                shared.fail_at(engine.epoch, error);
            }
        });
    }
    Ok(())
}
/// Retransmit individual lost RTP packets (RFC 4588) instead of decoding
/// around the hole; a PLI keyframe remains the fallback for late losses.
fn enable_nack(transceiver: &gst_webrtc::WebRTCRTPTransceiver) {
    if transceiver.find_property("do-nack").is_some() {
        transceiver.set_property("do-nack", true);
    }
}
fn engine_epoch(engine: &Weak<Engine>) -> u64 {
    engine.upgrade().map_or(u64::MAX, |e| e.epoch)
}
fn receive(
    shared: &Arc<Shared>,
    engine: &Arc<Engine>,
    pad: &gst::Pad,
    generation: u64,
) -> Result<()> {
    let bin = native(gst::parse::bin_from_description(
        "rtph264depay request-keyframe=true wait-for-keyframe=true ! h264parse ! video/x-h264,width=(int)[1,3840],height=(int)[1,2160] ! avdec_h264 max-threads=2 thread-type=slice output-corrupt=false ! videoconvert n-threads=2 ! video/x-raw,format=RGBA ! appsink name=frames emit-signals=true max-buffers=1 drop=true sync=false",
        true,
    ))?;
    let sink = bin
        .by_name("frames")
        .ok_or(TransportError::NativeOperation)?;
    let weak = Arc::downgrade(shared);
    let epoch = engine.epoch;
    sink.connect("new-sample", false, move |values| {
        let result = (|| -> Result<()> {
            let Some(shared) = active(&weak, epoch) else {
                return Ok(());
            };
            let sink = values[0]
                .get::<gst::Element>()
                .map_err(|_| TransportError::NativeOperation)?;
            let sample: Option<gst::Sample> = sink.emit_by_name("pull-sample", &[]);
            let sample = sample.ok_or(TransportError::NativeOperation)?;
            let caps = sample.caps().ok_or(TransportError::NativeOperation)?;
            let info = caps.structure(0).ok_or(TransportError::NativeOperation)?;
            let width = native(info.get::<i32>("width"))? as u32;
            let height = native(info.get::<i32>("height"))? as u32;
            let buffer = sample.buffer().ok_or(TransportError::NativeOperation)?;
            let map = native(buffer.map_readable())?;
            if width == 0
                || height == 0
                || width > 3840
                || height > 2160
                || (width as usize)
                    .checked_mul(height as usize)
                    .and_then(|n| n.checked_mul(4))
                    != Some(map.len())
            {
                return Err(ProtocolError::InvalidPayload.into());
            }
            // Event gate makes the epoch check and frame publication atomic with teardown.
            let _gate = shared.event_gate.lock().unwrap();
            if !shared.current(epoch)
                || shared
                    .config
                    .lock()
                    .unwrap()
                    .display
                    .is_none_or(|d| d.generation != generation)
            {
                return Ok(());
            }
            let frame = DecodedFrame {
                generation,
                width,
                height,
                rgba: map.as_slice().to_vec(),
                pts_ns: buffer.pts().map(|v| v.nseconds()),
            };
            if shared.frame.lock().unwrap().replace(frame).is_some() {
                shared.counters.frames.fetch_add(1, Ordering::Relaxed);
            }
            drop(_gate);
            // Publish without waiting for the monitor tick when the consumer is idle.
            shared.flush_frame();
            Ok(())
        })();
        Some(
            if result.is_ok() {
                gst::FlowReturn::Ok
            } else {
                gst::FlowReturn::Error
            }
            .to_value(),
        )
    });
    native(engine.pipeline.add(&bin))?;
    native(
        pad.link(
            &bin.static_pad("sink")
                .ok_or(TransportError::NativeOperation)?,
        ),
    )?;
    native(bin.sync_state_with_parent())?;
    Ok(())
}

pub(super) fn query_stats(shared: &Arc<Shared>, engine: &Arc<Engine>) {
    if engine.stats_pending.swap(true, Ordering::AcqRel) {
        return;
    }
    let weak = Arc::downgrade(shared);
    let weak_engine = Arc::downgrade(engine);
    let epoch = engine.epoch;
    let promise = gst::Promise::with_change_func(move |reply| {
        let Some(engine) = weak_engine.upgrade() else {
            return;
        };
        engine.stats_pending.store(false, Ordering::Release);
        let Some(shared) = active(&weak, epoch) else {
            return;
        };
        let Ok(Some(report)) = reply else {
            return;
        };
        let mut stats = crate::stats::parse_native(report);
        stats.queued_video_frames = engine
            .video
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.source.property("current-level-buffers"));
        let _gate = shared.event_gate.lock().unwrap();
        if shared.current(epoch) {
            stats.sampled_at_ms = Some(shared.now_ms());
            *shared.counters.native.lock().unwrap() = stats;
        }
    });
    engine
        .webrtc
        .emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encoded_backpressure_is_bounded_and_requires_config_keyframe_after_drop() {
        let peer = Peer::new(super::super::tests::config(PeerRole::Agent)).unwrap();
        peer.enable_h264().unwrap();
        let engine = peer.shared.engine().unwrap();
        let source = engine
            .video
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .source
            .clone();
        let pad = source.static_pad("src").unwrap();
        let block = pad
            .add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_, _| {
                gst::PadProbeReturn::Ok
            })
            .unwrap();
        let frame = EncodedFrame {
            generation: 7,
            pts_ns: 0,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
        };
        // The blocked native streaming thread leaves exactly two appsrc slots.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut full = false;
        while !full {
            match peer.push_video(frame.clone()) {
                Ok(()) => {}
                Err(TransportError::QueueFull) => full = true,
                Err(e) => panic!("{e}"),
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(source.property::<u64>("current-level-buffers") <= 2);
        assert_eq!(peer.stats().dropped_encoded_frames, 1);
        // A delta is inadmissible after a missing reference, even while the
        // native queue remains full. No asynchronous flush timing assumption.
        let mut delta = frame;
        delta.keyframe = false;
        assert_eq!(peer.push_video(delta), Err(TransportError::NotReady));
        pad.remove_probe(block);
        peer.close();
    }

    #[test]
    fn initial_delta_requests_one_keyframe_until_config_idr_arrives() {
        let mut peer = Peer::new(super::super::tests::config(PeerRole::Agent)).unwrap();
        peer.enable_h264().unwrap();
        let delta = EncodedFrame {
            generation: 7,
            pts_ns: 0,
            keyframe: false,
            data: vec![0, 0, 0, 1, 0x41],
        };
        for _ in 0..4 {
            assert_eq!(
                peer.push_video(delta.clone()),
                Err(TransportError::NotReady)
            );
        }
        assert_eq!(
            peer.events()
                .try_iter()
                .filter(|e| matches!(e, PeerEvent::KeyframeNeeded))
                .count(),
            1,
            "multiple buffered P-frames must request exactly one replacement keyframe"
        );
        peer.push_video(EncodedFrame {
            generation: 7,
            pts_ns: 1,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
        })
        .unwrap();
        assert!(
            !peer
                .shared
                .engine()
                .unwrap()
                .keyframe_requested
                .load(Ordering::Acquire)
        );
        peer.close();
    }

    #[test]
    fn temporarily_unavailable_live_clock_retries_without_retiring_authorized_peer() {
        let peer = Peer::new(super::super::tests::config(PeerRole::Agent)).unwrap();
        peer.enable_h264().unwrap();
        let engine = peer.shared.engine().unwrap();
        let clock = engine.pipeline.clock().unwrap();
        // Model the native pipeline's pre-clock state with a real GstPipeline.
        // Missing running time is documented during state changes, not a fatal
        // media operation. Do not commit an origin until a clock becomes usable.
        engine.pipeline.set_clock(None::<&gst::Clock>).unwrap();
        assert!(engine.pipeline.current_running_time().is_none());
        let frame = EncodedFrame {
            generation: 7,
            pts_ns: 0,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
        };
        let waiting = peer.push_video(frame.clone());
        engine.pipeline.set_clock(Some(&clock)).unwrap();
        assert_eq!(waiting, Err(TransportError::NotReady));
        assert!(!peer.is_closed());
        assert!(
            engine
                .video
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .timestamp_origin
                .is_none()
        );
        peer.push_video(frame).unwrap();
        assert!(
            engine
                .video
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .timestamp_origin
                .is_some()
        );
        peer.close();
    }

    #[test]
    fn capture_pts_are_rebased_to_live_pipeline_time_without_losing_cadence() {
        let peer = Peer::new(super::super::tests::config(PeerRole::Agent)).unwrap();
        peer.enable_h264().unwrap();
        let engine = peer.shared.engine().unwrap();
        let source = engine
            .video
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .source
            .clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let probe = source
            .static_pad("src")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                    tx.send(buffer.pts().unwrap().nseconds()).unwrap();
                }
                // This test inspects appsrc timestamps, without needing an SDP peer.
                gst::PadProbeReturn::Drop
            })
            .unwrap();
        thread::sleep(Duration::from_millis(100));
        let before = engine.pipeline.current_running_time().unwrap().nseconds();
        let frame = EncodedFrame {
            generation: 7,
            pts_ns: 9_000_000_000,
            keyframe: true,
            data: vec![0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65],
        };
        peer.push_video(frame.clone()).unwrap();
        let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let after = engine.pipeline.current_running_time().unwrap().nseconds();
        assert!(
            (before..=after).contains(&first),
            "capture clock must map to live running time"
        );
        let mut second = frame.clone();
        second.pts_ns += 33_333_333;
        peer.push_video(second).unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)).unwrap() - first,
            33_333_333
        );
        let mut backwards = frame;
        backwards.pts_ns -= 1;
        assert_eq!(
            peer.push_video(backwards),
            Err(ProtocolError::InvalidPayload.into())
        );
        source.static_pad("src").unwrap().remove_probe(probe);
        peer.close();
    }

    #[test]
    fn occupied_event_slot_retains_latest_decoded_frame_until_consumer_catches_up() {
        let mut peer = Peer::new(super::super::tests::config(PeerRole::Controller)).unwrap();
        peer.shared
            .events
            .try_send(PeerEvent::Stats(PeerStats::default()))
            .ok()
            .unwrap();
        peer.publish_frame(DecodedFrame {
            generation: 7,
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            pts_ns: Some(42),
        })
        .unwrap();
        // Several monitor iterations must not discard the pending frame merely
        // because an unrelated low-priority event occupies the delivery slot.
        thread::sleep(Duration::from_millis(30));
        assert!(peer.shared.frame.lock().unwrap().is_some());
        assert_eq!(peer.stats().dropped_frames, 0);
        let queued: Vec<_> = peer.events().try_iter().collect();
        assert!(
            queued
                .iter()
                .any(|event| matches!(event, PeerEvent::Stats(_)))
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(PeerEvent::Frame(frame)) =
                peer.events().recv_timeout(Duration::from_millis(10))
            {
                assert_eq!(frame.pts_ns, Some(42));
                break;
            }
            assert!(Instant::now() < deadline, "pending frame was not delivered");
        }
        peer.close();
    }

    #[test]
    fn native_jitter_latency_is_explicitly_bounded() {
        let peer = Peer::new(super::super::tests::config(PeerRole::Controller)).unwrap();
        assert_eq!(
            peer.shared
                .engine()
                .unwrap()
                .webrtc
                .property::<u32>("latency"),
            40
        );
        peer.close();
    }

    #[test]
    fn rebuild_preserves_current_generation_input_reset_notification() {
        let mut peer = Peer::new(super::super::tests::config(PeerRole::Controller)).unwrap();
        peer.enable_h264().unwrap();
        peer.handle_signal(SignalMessage::Display {
            session_id: super::super::tests::config(PeerRole::Controller).session_id,
            epoch: 0,
            display: DisplayInfo {
                generation: 8,
                width: 100,
                height: 100,
            },
        })
        .unwrap();
        peer.handle_signal(SignalMessage::Restart {
            session_id: super::super::tests::config(PeerRole::Controller).session_id,
            epoch: 1,
        })
        .unwrap();
        assert!(
            peer.events()
                .try_iter()
                .any(|e| matches!(e, PeerEvent::InputReset { generation: 8 }))
        );
        peer.close();
    }
    #[test]
    fn display_signals_fence_scope_high_water_and_preserve_lease() {
        let config = super::super::tests::config(PeerRole::Controller);
        let session_id = config.session_id;
        let mut controller = Peer::new(config).unwrap();
        let agent = Peer::new(super::super::tests::config(PeerRole::Agent)).unwrap();
        let unavailable = SignalMessage::DisplayUnavailable {
            session_id,
            epoch: 0,
            generation: 8,
        };
        assert_eq!(
            agent.handle_signal(unavailable.clone()),
            Err(TransportError::WrongRole)
        );
        assert_eq!(
            controller.display_unavailable(8),
            Err(TransportError::WrongRole)
        );
        assert_eq!(
            controller.update_display(DisplayInfo {
                generation: 8,
                width: 100,
                height: 100
            }),
            Err(TransportError::WrongRole)
        );
        controller.shared.input.lock().unwrap().renew_lease(123);
        controller
            .publish_frame(DecodedFrame {
                generation: 7,
                width: 1,
                height: 1,
                rgba: vec![0; 4],
                pts_ns: None,
            })
            .unwrap();
        controller.handle_signal(unavailable.clone()).unwrap();
        controller.handle_signal(unavailable).unwrap(); // exact duplicate is harmless
        assert!(controller.shared.frame.lock().unwrap().is_none());
        assert!(
            !controller
                .events()
                .try_iter()
                .any(|e| matches!(e, PeerEvent::Frame(_)))
        );
        assert_eq!(
            controller.shared.input.lock().unwrap().last_renewed_ms(),
            123
        );
        let stale = SignalMessage::Display {
            session_id,
            epoch: 0,
            display: DisplayInfo {
                generation: 8,
                width: 100,
                height: 100,
            },
        };
        assert_eq!(
            controller.handle_signal(stale),
            Err(ProtocolError::WrongGeneration.into())
        );
        assert_eq!(
            controller.handle_signal(SignalMessage::DisplayUnavailable {
                session_id,
                epoch: 1,
                generation: 9
            }),
            Err(TransportError::StaleEpoch)
        );
        assert_eq!(
            controller.handle_signal(SignalMessage::DisplayUnavailable {
                session_id: SessionId::from_uuid(uuid::Uuid::from_u128(999)),
                epoch: 0,
                generation: 9
            }),
            Err(ProtocolError::WrongSession.into())
        );
        controller
            .handle_signal(SignalMessage::Display {
                session_id,
                epoch: 0,
                display: DisplayInfo {
                    generation: 9,
                    width: 100,
                    height: 100,
                },
            })
            .unwrap();
        controller
            .handle_signal(SignalMessage::Restart {
                session_id,
                epoch: 1,
            })
            .unwrap();
        assert_eq!(
            controller.shared.input.lock().unwrap().last_renewed_ms(),
            123
        );
        assert!(controller.shared.input.lock().unwrap().expired(60_123));
        assert_eq!(
            controller.handle_signal(SignalMessage::DisplayUnavailable {
                session_id,
                epoch: 0,
                generation: 10
            }),
            Err(TransportError::StaleEpoch)
        );
        assert_eq!(
            controller
                .shared
                .config
                .lock()
                .unwrap()
                .display
                .unwrap()
                .generation,
            9
        );
        controller.close();
        agent.close();
    }
}
