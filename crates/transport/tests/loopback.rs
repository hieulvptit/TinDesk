use remotevnc_protocol::*;
use remotevnc_transport::*;
use std::time::{Duration, Instant};

fn config(role: PeerRole) -> PeerConfig {
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
fn envelope(sequence: u64, event: InputEvent) -> InputEnvelope {
    InputEnvelope {
        version: WIRE_VERSION,
        session_id: config(PeerRole::Controller).session_id,
        generation: 7,
        sequence,
        event,
    }
}
fn pump(a: &mut Peer, b: &mut Peer, until: impl Fn(&PeerEvent) -> bool) -> PeerEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut pending_description = [None, None];
    let mut early_ice_seen = [false, false];
    loop {
        for direction in 0..2 {
            let (from, to) = if direction == 0 {
                (&mut *a, &mut *b)
            } else {
                (&mut *b, &mut *a)
            };
            while let Ok(event) = from.events().try_recv() {
                if until(&event) {
                    return event;
                }
                match event {
                    PeerEvent::Signal(
                        signal @ (SignalMessage::Offer { .. } | SignalMessage::Answer { .. }),
                    ) => {
                        if early_ice_seen[direction] {
                            to.handle_signal(signal).unwrap();
                        } else {
                            pending_description[direction] = Some(signal);
                        }
                    }
                    PeerEvent::Signal(signal @ SignalMessage::IceCandidate { .. }) => {
                        // Deliberately deliver actual native ICE before its SDP.
                        to.handle_signal(signal).unwrap();
                        early_ice_seen[direction] = true;
                        if let Some(description) = pending_description[direction].take() {
                            to.handle_signal(description).unwrap();
                        }
                    }
                    PeerEvent::Signal(signal) => to.handle_signal(signal).unwrap(),
                    PeerEvent::Error(error) => panic!("peer error: {error}"),
                    _ => {}
                }
            }
        }
        assert!(Instant::now() < deadline, "real WebRTC loopback timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn real_peers_negotiate_exchange_exact_input_and_close() {
    let mut a = Peer::new(config(PeerRole::Controller)).unwrap();
    let mut b = Peer::new(config(PeerRole::Agent)).unwrap();
    a.start_negotiation().unwrap();
    pump(&mut a, &mut b, |event| {
        matches!(event, PeerEvent::ChannelsReady)
    });
    a.send_input(envelope(0, InputEvent::DisplayAck)).unwrap();
    pump(&mut a, &mut b, |event| matches!(event, PeerEvent::Input(_)));
    let text = envelope(
        1,
        InputEvent::Text {
            text: "Tiếng Việt|é|e\u{301}|中文|😀|\n\t\0".into(),
        },
    );
    a.send_input(text.clone()).unwrap();
    let received = pump(&mut a, &mut b, |event| matches!(event, PeerEvent::Input(_)));
    assert!(matches!(received, PeerEvent::Input(input) if input == text));
    for input in [
        envelope(
            2,
            InputEvent::Key {
                keysym: 0xff0d,
                pressed: false,
            },
        ),
        envelope(
            3,
            InputEvent::Button {
                button: MouseButton::Left,
                pressed: true,
                x: 90,
                y: 80,
            },
        ),
        envelope(4, InputEvent::ReleaseAll),
    ] {
        a.send_input(input.clone()).unwrap();
        let received = pump(&mut a, &mut b, |event| matches!(event, PeerEvent::Input(_)));
        assert!(matches!(received, PeerEvent::Input(got) if got == input));
    }
    assert!(b.send_input(envelope(5, InputEvent::ReleaseAll)).is_err());
    a.close();
    b.close();
    assert!(a.is_closed());
    assert!(b.is_closed());
    assert!(a.send_input(envelope(6, InputEvent::ReleaseAll)).is_err());
}

#[test]
fn relay_policy_early_ice_and_signal_scope_are_enforced() {
    let mut cfg = config(PeerRole::Agent);
    cfg.relay_only = true;
    let peer = Peer::new(cfg).unwrap();
    assert!(peer.relay_only());
    peer.handle_signal(SignalMessage::IceCandidate {
        epoch: 0,
        session_id: config(PeerRole::Agent).session_id,
        candidate: "candidate:1 1 UDP 2122260223 127.0.0.1 12345 typ host".into(),
        sdp_mid: Some("0".into()),
        sdp_mline_index: Some(0),
    })
    .unwrap();
    assert!(
        peer.handle_signal(SignalMessage::End {
            epoch: 0,
            session_id: SessionId::from_uuid(uuid::Uuid::from_u128(9))
        })
        .is_err()
    );
    assert!(
        peer.handle_signal(SignalMessage::Offer {
            epoch: 0,
            session_id: config(PeerRole::Agent).session_id,
            sdp: "x".repeat(CONTROL_PAYLOAD_LIMIT + 1)
        })
        .is_err()
    );
    peer.close();
}

#[test]
fn real_peers_rebuild_ice_and_reject_old_epoch_before_resuming() {
    let mut a = Peer::new(config(PeerRole::Controller)).unwrap();
    let mut b = Peer::new(config(PeerRole::Agent)).unwrap();
    a.start_negotiation().unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::ChannelsReady));
    a.send_input(envelope(0, InputEvent::DisplayAck)).unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::Input(_)));
    a.restart_ice(vec![]).unwrap();
    a.start_negotiation().unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::ChannelsReady));
    assert_eq!(a.epoch(), 1);
    assert_eq!(b.epoch(), 1);
    assert!(
        b.handle_signal(SignalMessage::IceCandidate {
            session_id: config(PeerRole::Agent).session_id,
            epoch: 0,
            candidate: "candidate:1 1 UDP 2122260223 127.0.0.1 1 typ host".into(),
            sdp_mid: None,
            sdp_mline_index: Some(0)
        })
        .is_err()
    );
    assert!(a.send_input(envelope(1, InputEvent::ReleaseAll)).is_ok());
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::Input(_)));
    assert!(
        a.send_input(envelope(
            2,
            InputEvent::Text {
                text: "must wait".into()
            }
        ))
        .is_err()
    );
    a.send_input(envelope(3, InputEvent::DisplayAck)).unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::Input(_)));
    let input = envelope(
        4,
        InputEvent::Text {
            text: "after restart 😀".into(),
        },
    );
    a.send_input(input.clone()).unwrap();
    assert!(
        matches!(pump(&mut a, &mut b, |e| matches!(e, PeerEvent::Input(_))), PeerEvent::Input(got) if got == input)
    );
    a.close();
    b.close();
}

#[test]
fn malformed_scope_and_low_priority_frames_cannot_starve_reliable_release() {
    let mut a = Peer::new(config(PeerRole::Controller)).unwrap();
    let mut b = Peer::new(config(PeerRole::Agent)).unwrap();
    a.start_negotiation().unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::ChannelsReady));
    a.send_input(envelope(0, InputEvent::DisplayAck)).unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::Input(_)));
    let mut wrong = envelope(1, InputEvent::ReleaseAll);
    wrong.session_id = SessionId::from_uuid(uuid::Uuid::from_u128(99));
    assert!(matches!(
        a.send_input(wrong),
        Err(TransportError::Protocol(ProtocolError::WrongSession))
    ));
    let mut wrong = envelope(2, InputEvent::ReleaseAll);
    wrong.generation = 6;
    assert!(matches!(
        a.send_input(wrong),
        Err(TransportError::Protocol(ProtocolError::WrongGeneration))
    ));
    assert!(
        a.send_input(envelope(
            3,
            InputEvent::Text {
                text: "x".repeat(TEXT_LIMIT + 1)
            }
        ))
        .is_err()
    );
    for n in 0..1000 {
        b.publish_frame(DecodedFrame {
            generation: 7,
            width: 100,
            height: 100,
            rgba: vec![n as u8; 40_000],
            pts_ns: Some(n),
        })
        .unwrap();
    }
    a.send_input(envelope(4, InputEvent::ReleaseAll)).unwrap();
    assert!(
        matches!(pump(&mut a, &mut b, |e| matches!(e, PeerEvent::Input(_))), PeerEvent::Input(input) if matches!(input.event, InputEvent::ReleaseAll))
    );
    assert!(b.stats().dropped_frames > 0);
    let started = Instant::now();
    a.close();
    b.close();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "close blocked behind frames"
    );
}

#[test]
fn reliable_event_overflow_closes_instead_of_silently_dropping_input() {
    let mut a = Peer::new(config(PeerRole::Controller)).unwrap();
    let mut b = Peer::new(config(PeerRole::Agent)).unwrap();
    a.start_negotiation().unwrap();
    pump(&mut a, &mut b, |e| matches!(e, PeerEvent::ChannelsReady));
    // Do not consume the agent's event queue while reliable messages arrive.
    for seq in 0..200 {
        if a.send_input(envelope(seq, InputEvent::ReleaseAll)).is_err() {
            break;
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !b.is_closed() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(b.is_closed());
    assert!(b.events().len() <= 130);
    let events: Vec<_> = b.events().try_iter().collect();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PeerEvent::Error(TransportError::QueueFull)))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PeerEvent::ConnectionChanged(ConnectionState::Closed)))
    );
    a.close();
    b.close();
}

#[test]
fn structured_h264_is_configured_before_the_initial_offer() {
    let mut a = Peer::new(config(PeerRole::Controller)).unwrap();
    a.enable_h264().unwrap();
    a.start_negotiation().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(PeerEvent::Signal(SignalMessage::Offer { sdp, .. })) =
            a.events().recv_timeout(Duration::from_millis(50))
        {
            assert!(sdp.contains("m=video"));
            assert!(sdp.contains("H264/90000"));
            break;
        }
        assert!(Instant::now() < deadline);
    }
    a.close();
}
