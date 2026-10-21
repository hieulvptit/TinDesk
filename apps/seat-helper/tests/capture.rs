use remotevnc_protocol::*;
use remotevnc_seat_helper::{
    capture::Capture,
    seat::{DisplayTarget, ServerIdentity},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};
#[test]
#[ignore = "requires Xvfb fixture and Linux capture worker"]
fn physical_capture_worker_is_bounded_and_restarts_with_config() {
    let target = DisplayTarget {
        seat: "test-only".into(),
        display: std::env::var("DISPLAY").unwrap(),
        authority: std::env::var_os("XAUTHORITY").unwrap().into(),
        identity: ServerIdentity {
            process_start: "fixture".into(),
            socket_inode: 0,
            authority_inode: 0,
            authority_modified_ns: 0,
            connection_epoch: 0,
        },
        output: "screen".into(),
        origin_x: 100,
        origin_y: 50,
        width: 640,
        height: 480,
    };
    let profile = VideoProfile {
        width: 320,
        height: 240,
        encoder: VideoEncoder::X264,
        ..VideoProfile::default()
    };
    for generation in [10, 11] {
        let mut capture = Capture::start_with_worker(
            Path::new(env!("CARGO_BIN_EXE_remotevnc-seat-helper")),
            &target,
            profile,
            generation,
        )
        .unwrap();
        let first = capture
            .frames()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        assert_eq!(first.generation, generation);
        assert!(first.keyframe);
        assert_eq!(capture.selected_encoder(), Some(VideoEncoder::X264));
        std::thread::sleep(Duration::from_millis(500));
        assert!(capture.frames().try_iter().count() <= FRAME_QUEUE_CAPACITY);
        capture.request_keyframe().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if capture
                .frames()
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .keyframe
            {
                break;
            }
            assert!(Instant::now() < deadline);
        }
        // A stopped consumer and blocked worker pipe must not delay cleanup.
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        capture.stop();
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}

#[test]
#[ignore = "requires Xvfb fixture and Linux capture worker"]
fn cropped_capture_hot_profile_decodes_over_rtp_without_input_reset() {
    use remotevnc_seat_helper::{input::X11Input, x11};
    use remotevnc_transport::*;
    use x11rb::{
        connection::Connection,
        protocol::xproto::{ConnectionExt, CreateGCAux, Rectangle},
    };
    let target = DisplayTarget {
        seat: "test-only".into(),
        display: std::env::var("DISPLAY").unwrap(),
        authority: std::env::var_os("XAUTHORITY").unwrap().into(),
        identity: ServerIdentity {
            process_start: "fixture".into(),
            socket_inode: 0,
            authority_inode: 0,
            authority_modified_ns: 0,
            connection_epoch: 0,
        },
        output: "screen".into(),
        origin_x: 100,
        origin_y: 50,
        width: 640,
        height: 480,
    };
    let x = x11::connect(&target).unwrap();
    let root = x.setup().roots[0].root;
    let gc = x.generate_id().unwrap();
    x.create_gc(gc, root, &CreateGCAux::new().foreground(0xff0000))
        .unwrap()
        .check()
        .unwrap();
    x.poly_fill_rectangle(
        root,
        gc,
        &[Rectangle {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }],
    )
    .unwrap()
    .check()
    .unwrap();
    x.change_gc(
        gc,
        &x11rb::protocol::xproto::ChangeGCAux::new().foreground(0x0000ff),
    )
    .unwrap()
    .check()
    .unwrap();
    x.poly_fill_rectangle(
        root,
        gc,
        &[Rectangle {
            x: 100,
            y: 50,
            width: 640,
            height: 480,
        }],
    )
    .unwrap()
    .check()
    .unwrap();
    let profile = VideoProfile {
        width: 320,
        height: 240,
        encoder: VideoEncoder::X264,
        ..VideoProfile::default()
    };
    let mut capture = Capture::start_with_worker(
        Path::new(env!("CARGO_BIN_EXE_remotevnc-seat-helper")),
        &target,
        profile,
        7,
    )
    .unwrap();
    let session = SessionId::from_uuid(uuid::Uuid::nil());
    let config =
        |role| PeerConfig {
            session_id: session,
            local_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(if role == PeerRole::Agent {
                1
            } else {
                2
            })),
            remote_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(
                if role == PeerRole::Agent { 2 } else { 1 },
            )),
            role,
            display: Some(DisplayInfo {
                generation: 7,
                width: 640,
                height: 480,
            }),
            ice_servers: vec![],
            relay_only: false,
        };
    let mut agent = Peer::new(config(PeerRole::Agent)).unwrap();
    let mut controller = Peer::new(config(PeerRole::Controller)).unwrap();
    agent.enable_h264().unwrap();
    controller.enable_h264().unwrap();
    controller.start_negotiation().unwrap();
    let mut acknowledged = false;
    let mut received_ack = false;
    let mut input = X11Input::connect(&target).unwrap();
    input
        .apply(&InputEvent::PointerMove { x: 10, y: 20 })
        .unwrap();
    let pointer = x.query_pointer(root).unwrap().reply().unwrap();
    assert_eq!((pointer.root_x, pointer.root_y), (110, 70));
    let output = x11::selected_output(&target, Some("screen")).unwrap();
    assert_eq!(
        (output.x, output.y, output.width, output.height),
        (0, 0, 1920, 1080)
    );
    assert!(
        capture
            .update_profile(VideoProfile {
                encoder: VideoEncoder::Nv,
                ..profile
            })
            .is_err()
    );
    let mut stage = 0;
    let mut changed = None;
    let mut previous_pts = None;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Ok(frame) = capture.frames().try_recv() {
            if let Some(last) = previous_pts {
                assert!(frame.pts_ns > last);
            }
            previous_pts = Some(frame.pts_ns);
            match agent.push_video(frame) {
                Ok(()) => {}
                Err(TransportError::QueueFull | TransportError::NotReady) => {
                    capture.request_keyframe().unwrap();
                }
                Err(e) => panic!("push: {e}"),
            }
        }
        for event in agent.events().try_iter().collect::<Vec<_>>() {
            match event {
                PeerEvent::Signal(s) => controller.handle_signal(s).unwrap(),
                PeerEvent::KeyframeNeeded => capture.request_keyframe().unwrap(),
                PeerEvent::Input(i) => {
                    received_ack |= matches!(i.event, InputEvent::DisplayAck);
                    input.apply(&i.event).unwrap();
                }
                PeerEvent::InputReset { .. } => panic!("profile reset input"),
                PeerEvent::Error(e) => panic!("agent: {e}"),
                _ => {}
            }
        }
        for event in controller.events().try_iter().collect::<Vec<_>>() {
            match event {
                PeerEvent::Signal(s) => agent.handle_signal(s).unwrap(),
                PeerEvent::ChannelsReady if !acknowledged => {
                    controller
                        .send_input(InputEnvelope {
                            version: WIRE_VERSION,
                            session_id: session,
                            generation: 7,
                            sequence: 0,
                            event: InputEvent::DisplayAck,
                        })
                        .unwrap();
                    acknowledged = true;
                }
                PeerEvent::Frame(f) => {
                    assert_eq!(f.generation, 7);
                    let center = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
                    let p = &f.rgba[center..center + 4];
                    assert!(p[2] > 220 && p[0] < 30 && p[1] < 30, "crop pixels {p:?}");
                    if stage == 0 && received_ack && f.width == 320 {
                        controller
                            .send_input(InputEnvelope {
                                version: WIRE_VERSION,
                                session_id: session,
                                generation: 7,
                                sequence: 1,
                                event: InputEvent::Key {
                                    keysym: 0xffe1,
                                    pressed: true,
                                },
                            })
                            .unwrap();
                        capture.update_profile(profile.downshift()).unwrap();
                        changed = Some(Instant::now());
                        stage = 1;
                    } else if stage == 1
                        && f.width == 240
                        && f.height == 180
                        && changed.unwrap().elapsed() > Duration::from_secs(2)
                    {
                        assert_eq!((agent.epoch(), controller.epoch()), (0, 0));
                        assert!(
                            x.query_keymap()
                                .unwrap()
                                .reply()
                                .unwrap()
                                .keys
                                .iter()
                                .any(|v| *v != 0),
                            "held input was released by profile update"
                        );
                        assert_eq!(
                            agent.stats().native.local_candidate,
                            Some(remotevnc_transport::stats::CandidateType::Host)
                        );
                        assert!(agent.stats().native.packets_sent.is_some_and(|n| n > 0));
                        controller
                            .send_input(InputEnvelope {
                                version: WIRE_VERSION,
                                session_id: session,
                                generation: 7,
                                sequence: 2,
                                event: InputEvent::Key {
                                    keysym: 0xffe1,
                                    pressed: false,
                                },
                            })
                            .unwrap();
                        stage = 2;
                    }
                }
                PeerEvent::InputReset { .. } => panic!("profile reset input"),
                PeerEvent::Error(e) => panic!("controller: {e}"),
                _ => {}
            }
        }
        if stage == 2
            && x.query_keymap()
                .unwrap()
                .reply()
                .unwrap()
                .keys
                .iter()
                .all(|v| *v == 0)
        {
            break;
        }
        assert!(Instant::now() < deadline, "profile transition timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    agent.close();
    controller.close();
    capture.stop();
    x.free_gc(gc).unwrap().check().unwrap();
}

#[test]
#[ignore = "requires Xvfb fixture and Linux capture worker"]
fn hot_profile_renegotiation_preserves_capture_and_held_input() {
    use gst::prelude::*;
    use remotevnc_seat_helper::{input::X11Input, x11};
    use x11rb::{
        connection::Connection,
        protocol::xproto::{ConnectionExt, KeyButMask},
    };
    // A panic must stop the test decoder before GStreamer elements are dropped.
    struct Decoder(gst::Pipeline);
    impl Drop for Decoder {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }
    let target = DisplayTarget {
        seat: "test-only".into(),
        display: std::env::var("DISPLAY").unwrap(),
        authority: std::env::var_os("XAUTHORITY").unwrap().into(),
        identity: ServerIdentity {
            process_start: "fixture".into(),
            socket_inode: 0,
            authority_inode: 0,
            authority_modified_ns: 0,
            connection_epoch: 0,
        },
        output: "screen".into(),
        origin_x: 0,
        origin_y: 0,
        width: 1920,
        height: 1080,
    };
    let high = VideoProfile {
        width: 320,
        height: 180,
        fps: 10,
        bitrate_kbps: 512,
        encoder: VideoEncoder::X264,
    };
    let low = VideoProfile {
        width: 240,
        height: 134,
        fps: 7,
        bitrate_kbps: 384,
        encoder: VideoEncoder::X264,
    };
    high.validate().unwrap();
    low.validate().unwrap();
    let profiles = [high, low, high, low, high];
    let x = x11::connect(&target).unwrap();
    let root = x.setup().roots[0].root;
    let min = x.setup().min_keycode;
    let mapping = x
        .get_keyboard_mapping(min, x.setup().max_keycode - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let shift = min as usize
        + mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .position(|keys| keys.contains(&0xffe1))
            .unwrap();
    let held = || {
        let keys = x.query_keymap().unwrap().reply().unwrap().keys;
        let pointer = x.query_pointer(root).unwrap().reply().unwrap();
        (
            keys[shift / 8] & (1 << (shift % 8)) != 0,
            pointer.mask.contains(KeyButMask::BUTTON1),
        )
    };
    assert_eq!(held(), (false, false));
    let mut input = X11Input::connect(&target).unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe1,
            pressed: true,
        })
        .unwrap();
    input
        .apply(&InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
            x: 20,
            y: 20,
        })
        .unwrap();
    assert_eq!(held(), (true, true));
    gst::init().unwrap();
    let decoder = Decoder(gst::parse::launch(
        "appsrc name=input is-live=true format=time block=false max-buffers=2 caps=\"video/x-h264,stream-format=byte-stream,alignment=au\" ! h264parse ! avdec_h264 ! appsink name=decoded sync=false max-buffers=2 drop=false"
    ).unwrap().downcast::<gst::Pipeline>().unwrap());
    let source = decoder.0.by_name("input").unwrap();
    let sink = decoder.0.by_name("decoded").unwrap();
    decoder.0.set_state(gst::State::Playing).unwrap();
    let mut capture = Capture::start_with_worker(
        Path::new(env!("CARGO_BIN_EXE_remotevnc-seat-helper")),
        &target,
        high,
        7,
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    let mut previous_pts = None;
    let mut stage = 0;
    let mut stage_started = Instant::now();
    let mut stage_pts = None;
    let mut config_pts = None;
    let mut new_output = false;
    let mut keyframe_requested = Instant::now();
    loop {
        if let Some(bus) = decoder.0.bus() {
            for message in bus.iter() {
                if let gst::MessageView::Error(error) = message.view() {
                    panic!("decoder stage{stage}: {}", error.error());
                }
            }
        }
        while let Ok(frame) = capture.frames().try_recv() {
            assert_eq!(frame.generation, 7, "quality change cannot replace display");
            assert!(
                previous_pts.is_none_or(|pts| frame.pts_ns > pts),
                "source PTS must advance stage{stage}"
            );
            previous_pts = Some(frame.pts_ns);
            if frame.is_config_keyframe() && stage_pts.is_none_or(|pts| frame.pts_ns > pts) {
                config_pts = Some(frame.pts_ns);
            }
            let mut buffer = gst::Buffer::from_mut_slice(frame.data);
            let data = buffer.get_mut().unwrap();
            data.set_pts(gst::ClockTime::from_nseconds(frame.pts_ns));
            data.set_dts(gst::ClockTime::from_nseconds(frame.pts_ns));
            if !frame.keyframe {
                data.set_flags(gst::BufferFlags::DELTA_UNIT);
            }
            let flow: gst::FlowReturn = source.emit_by_name("push-buffer", &[&buffer]);
            flow.into_result().unwrap();
        }
        loop {
            let sample: Option<gst::Sample> = sink.emit_by_name("try-pull-sample", &[&0u64]);
            let Some(sample) = sample else {
                break;
            };
            let caps = sample.caps().unwrap().structure(0).unwrap();
            let dimensions = (
                caps.get::<i32>("width").unwrap(),
                caps.get::<i32>("height").unwrap(),
            );
            let pts = sample.buffer().unwrap().pts().unwrap().nseconds();
            let expected = profiles[stage];
            if dimensions == (expected.width as i32, expected.height as i32)
                && config_pts.is_some_and(|config| pts >= config)
            {
                new_output = true;
            }
        }
        assert_eq!(
            held(),
            (true, true),
            "quality-only update released input stage{stage}"
        );
        if new_output && stage_started.elapsed() >= Duration::from_secs(2) {
            if stage == 4 {
                break;
            }
            stage += 1;
            stage_pts = previous_pts;
            config_pts = None;
            new_output = false;
            capture.update_profile(profiles[stage]).unwrap();
            stage_started = Instant::now();
        }
        if keyframe_requested.elapsed() > Duration::from_secs(1) {
            capture.request_keyframe().unwrap_or_else(|error| {
                panic!("capture ended during profile stage{stage}: {error}")
            });
            keyframe_requested = Instant::now();
        }
        assert!(
            Instant::now() < deadline,
            "hot profile stage{stage} lacks fresh decoded config-IDR output, lastPTS{previous_pts:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    capture.stop();
    assert_eq!(held(), (true, true));
    input.release_all().unwrap();
    assert_eq!(held(), (false, false));
}

#[test]
#[ignore = "requires 1920x1080 Xvfb fixture and Linux capture worker"]
fn repeated_1080p_rtp_stream_stays_bounded_and_stops_cleanly() {
    use remotevnc_transport::*;
    for generation in 20..23 {
        let target = DisplayTarget {
            seat: "test-only".into(),
            display: std::env::var("DISPLAY").unwrap(),
            authority: std::env::var_os("XAUTHORITY").unwrap().into(),
            identity: ServerIdentity {
                process_start: "stress".into(),
                socket_inode: 0,
                authority_inode: 0,
                authority_modified_ns: 0,
                connection_epoch: 0,
            },
            output: "screen".into(),
            origin_x: 0,
            origin_y: 0,
            width: 1920,
            height: 1080,
        };
        let profile = VideoProfile::default();
        assert_eq!(
            (profile.width, profile.height, profile.fps),
            (1920, 1080, 60)
        );
        let mut capture = Capture::start_with_worker(
            Path::new(env!("CARGO_BIN_EXE_remotevnc-seat-helper")),
            &target,
            profile,
            generation,
        )
        .unwrap();
        let config =
            |role| PeerConfig {
                session_id: SessionId::from_uuid(uuid::Uuid::nil()),
                local_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(
                    if role == PeerRole::Agent { 1 } else { 2 },
                )),
                remote_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(
                    if role == PeerRole::Agent { 2 } else { 1 },
                )),
                role,
                display: Some(DisplayInfo {
                    generation,
                    width: 1920,
                    height: 1080,
                }),
                ice_servers: vec![],
                relay_only: false,
            };
        let mut agent = Peer::new(config(PeerRole::Agent)).unwrap();
        let mut controller = Peer::new(config(PeerRole::Controller)).unwrap();
        agent.enable_h264().unwrap();
        controller.enable_h264().unwrap();
        controller.start_negotiation().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut sent = 0;
        let mut decoded = 0;
        let mut stalled = false;
        while sent < 75 || decoded < 20 {
            for frame in capture.frames().try_iter().collect::<Vec<_>>() {
                match agent.push_video(frame) {
                    Ok(()) => sent += 1,
                    Err(TransportError::QueueFull | TransportError::NotReady) => {
                        capture.request_keyframe().unwrap();
                    }
                    Err(e) => panic!("push: {e}"),
                }
            }
            for event in agent.events().try_iter().collect::<Vec<_>>() {
                match event {
                    PeerEvent::Signal(s) => controller.handle_signal(s).unwrap(),
                    PeerEvent::KeyframeNeeded => capture.request_keyframe().unwrap(),
                    PeerEvent::Error(e) => panic!("agent: {e}"),
                    _ => {}
                }
            }
            for event in controller.events().try_iter().collect::<Vec<_>>() {
                match event {
                    PeerEvent::Signal(s) => agent.handle_signal(s).unwrap(),
                    PeerEvent::Frame(f) => {
                        assert_eq!((f.generation, f.width, f.height), (generation, 1920, 1080));
                        decoded += 1;
                    }
                    PeerEvent::Error(e) => panic!("controller: {e}"),
                    _ => {}
                }
            }
            assert!(
                agent
                    .stats()
                    .native
                    .queued_video_frames
                    .is_none_or(|n| n <= 2)
            );
            if sent >= 20 && !stalled {
                std::thread::sleep(Duration::from_millis(300));
                stalled = true;
            }
            assert!(
                Instant::now() < deadline,
                "1080p stalled: sent={sent}, decoded={decoded}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(capture.selected_encoder(), Some(VideoEncoder::X264));
        let stopped = Instant::now();
        capture.stop();
        agent.close();
        controller.close();
        assert!(stopped.elapsed() < Duration::from_secs(2));
        eprintln!(
            "1080p generation {generation}: sent {sent}, decoded {decoded}, encoder x264, queue<=2, clean stop"
        );
    }
}
