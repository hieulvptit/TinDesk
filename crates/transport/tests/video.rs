use gst::prelude::*;
use remotevnc_protocol::*;
use remotevnc_transport::*;
use std::time::{Duration, Instant};

fn config(role: PeerRole) -> PeerConfig {
    PeerConfig {
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        local_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(if role == PeerRole::Agent {
            1
        } else {
            2
        })),
        remote_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(if role == PeerRole::Agent {
            2
        } else {
            1
        })),
        role,
        display: Some(DisplayInfo {
            generation: 1,
            width: 640,
            height: 480,
        }),
        ice_servers: vec![],
        relay_only: false,
    }
}
fn encoded(generation: u64) -> EncodedFrame {
    gst::init().unwrap();
    let pipeline = gst::parse::launch(&format!("videotestsrc num-buffers=1 pattern={} ! video/x-raw,width=320,height=240,framerate=30/1 ! videoconvert ! x264enc tune=zerolatency speed-preset=ultrafast bframes=0 key-int-max=30 ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink",if generation==1 {"red"} else {"blue"})).unwrap().downcast::<gst::Pipeline>().unwrap();
    let sink = pipeline.by_name("sink").unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let sample: Option<gst::Sample> = sink.emit_by_name("try-pull-sample", &[&5_000_000_000u64]);
    let sample = sample.unwrap();
    let buffer = sample.buffer().unwrap();
    let frame = EncodedFrame {
        generation,
        pts_ns: 0,
        keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
        data: buffer.map_readable().unwrap().as_slice().to_vec(),
    };
    pipeline.set_state(gst::State::Null).unwrap();
    frame
}
fn forward(peer: &Peer, message: SignalMessage) {
    let epoch = match &message {
        SignalMessage::Display { epoch, .. }
        | SignalMessage::DisplayUnavailable { epoch, .. }
        | SignalMessage::Restart { epoch, .. }
        | SignalMessage::Offer { epoch, .. }
        | SignalMessage::Answer { epoch, .. }
        | SignalMessage::IceCandidate { epoch, .. }
        | SignalMessage::End { epoch, .. } => *epoch,
    };
    if let Err(error) = peer.handle_signal(message) {
        assert!(
            matches!(error, TransportError::StaleEpoch) && epoch < peer.epoch(),
            "unexpected signaling failure: {error}"
        );
    }
}
fn pump(controller: &mut Peer, agent: &mut Peer, generation: u64) -> DecodedFrame {
    let deadline = Instant::now() + Duration::from_secs(15);
    let frame = encoded(generation);
    let mut sent = Instant::now() - Duration::from_secs(1);
    let mut offered = controller.epoch();
    loop {
        if sent.elapsed() > Duration::from_millis(100) {
            agent.push_video(frame.clone()).unwrap();
            sent = Instant::now();
        }
        for event in controller.events().try_iter().collect::<Vec<_>>() {
            match event {
                PeerEvent::Signal(s) => forward(agent, s),
                PeerEvent::Frame(f) => {
                    assert_eq!(f.generation, generation);
                    return f;
                }
                PeerEvent::Error(e) => panic!("controller: {e}"),
                _ => {}
            }
        }
        for event in agent.events().try_iter().collect::<Vec<_>>() {
            match event {
                PeerEvent::Signal(s) => forward(controller, s),
                PeerEvent::Error(e) => panic!("agent: {e}"),
                _ => {}
            }
        }
        if controller.epoch() > offered {
            controller.start_negotiation().unwrap();
            offered = controller.epoch();
        }
        assert!(Instant::now() < deadline, "no RTP decoded frame");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn real_h264_rtp_decodes_scaled_pixels_and_rebuilds_generation() {
    let mut agent = Peer::new(config(PeerRole::Agent)).unwrap();
    let mut controller_config = config(PeerRole::Controller);
    controller_config.display = None;
    let mut controller = Peer::new(controller_config).unwrap();
    for event in agent.events().try_iter().collect::<Vec<_>>() {
        if let PeerEvent::Signal(signal) = event {
            forward(&controller, signal);
        }
    }
    agent.enable_h264().unwrap();
    controller.enable_h264().unwrap();
    controller.start_negotiation().unwrap();
    let frame = pump(&mut controller, &mut agent, 1);
    assert_eq!((frame.width, frame.height), (320, 240));
    assert_eq!(
        frame
            .physical_position(
                DisplayInfo {
                    generation: 1,
                    width: 640,
                    height: 480
                },
                160,
                120
            )
            .unwrap(),
        (320, 240)
    );
    assert!(
        frame
            .physical_position(
                DisplayInfo {
                    generation: 2,
                    width: 640,
                    height: 480
                },
                160,
                120
            )
            .is_err()
    );
    let p = &frame.rgba[(120 * 320 + 160) * 4..][..4];
    assert!(p[0] > 220 && p[1] < 30 && p[2] < 30, "{p:?}");
    for _ in 0..10 {
        let _ = agent.push_video(encoded(1));
    }
    // Unavailability retains the same logical session and current epoch, but
    // prevents old input and frames from crossing the waiting period.
    agent.display_unavailable(2).unwrap();
    assert!(matches!(
        agent.push_video(encoded(1)),
        Err(TransportError::Protocol(ProtocolError::DisplayUnavailable))
    ));
    for event in agent.events().try_iter().collect::<Vec<_>>() {
        if let PeerEvent::Signal(signal) = event {
            forward(&controller, signal);
        }
    }
    assert!(
        controller
            .events()
            .try_iter()
            .any(|event| matches!(event, PeerEvent::DisplayUnavailable { generation: 2 }))
    );
    let waiting = Instant::now() + Duration::from_millis(200);
    while Instant::now() < waiting {
        assert!(!controller.events().try_iter().any(|event| matches!(
            event,
            PeerEvent::Frame(_) | PeerEvent::Input(_) | PeerEvent::Error(_)
        )));
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!agent.is_closed() && !controller.is_closed());
    assert_eq!((agent.epoch(), controller.epoch()), (0, 0));
    assert!(
        controller
            .send_input(InputEnvelope {
                version: WIRE_VERSION,
                session_id: config(PeerRole::Controller).session_id,
                generation: 1,
                sequence: 1,
                event: InputEvent::DisplayAck
            })
            .is_err()
    );
    assert!(
        controller
            .handle_signal(SignalMessage::Display {
                session_id: config(PeerRole::Controller).session_id,
                epoch: 0,
                display: config(PeerRole::Controller).display.unwrap()
            })
            .is_err()
    );
    // Restored metadata precedes the Agent-owned Restart marker.
    let display = DisplayInfo {
        generation: 3,
        width: 800,
        height: 600,
    };
    agent.update_display(display).unwrap();
    assert!(agent.push_video(encoded(1)).is_err());
    let frame = pump(&mut controller, &mut agent, 3);
    assert_eq!(frame.generation, 3);
    let p = &frame.rgba[(120 * 320 + 160) * 4..][..4];
    assert!(
        p[2] > 220 && p[0] < 30,
        "old-generation pixels were relabeled: {p:?}"
    );
    assert_eq!(agent.epoch(), 1);
    assert_eq!(controller.epoch(), 1);
    let mut input = InputEnvelope {
        version: WIRE_VERSION,
        session_id: config(PeerRole::Controller).session_id,
        generation: 3,
        sequence: 2,
        event: InputEvent::Key {
            keysym: 0xffe1,
            pressed: true,
        },
    };
    assert_eq!(
        controller.send_input(input.clone()),
        Err(ProtocolError::DisplayNotAcknowledged.into())
    );
    input.event = InputEvent::DisplayAck;
    controller.send_input(input.clone()).unwrap();
    input.event = InputEvent::Key {
        keysym: 0xffe1,
        pressed: true,
    };
    controller.send_input(input).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if agent.events().try_iter().any(|event| {
            matches!(
                event,
                PeerEvent::Input(InputEnvelope {
                    generation: 3,
                    event: InputEvent::Key { pressed: true, .. },
                    ..
                })
            )
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "restored acknowledged input not delivered"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    controller.close();
    agent.close();
    assert!(controller.is_closed() && agent.is_closed());
}

#[test]
fn adaptation_requires_sustained_measured_pressure_and_stays_bounded() {
    let mut adaptation =
        remotevnc_transport::stats::AdaptiveProfile::new(VideoProfile::default()).unwrap();
    let mut stats = PeerStats::default();
    for _ in 0..10 {
        assert!(adaptation.observe(&stats).is_none());
    }
    stats.native.loss_ppm = Some(100_000);
    assert!(adaptation.observe(&stats).is_none());
    assert!(adaptation.observe(&stats).is_none());
    let lower = adaptation.observe(&stats).unwrap();
    assert!(lower.width < 1920);
    lower.validate().unwrap();
    for _ in 0..2 {
        assert!(adaptation.observe(&stats).is_none());
    }
}
