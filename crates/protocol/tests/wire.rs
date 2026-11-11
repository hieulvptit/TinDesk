use binachicken_protocol::*;

#[test]
fn unicode_payload_round_trips() {
    let event = InputEvent::Text {
        text: "Tiếng Việt 🔐 日本語".into(),
    };
    let bytes = encode_control(&event).unwrap();
    let decoded: InputEvent = decode_control(&bytes).unwrap();
    assert!(matches!(decoded, InputEvent::Text { text } if text == "Tiếng Việt 🔐 日本語"));
}

#[test]
fn click_position_is_self_contained() {
    let input: InputEvent =
        decode_control(br#"{"type":"button","button":"left","pressed":true,"x":45,"y":67}"#)
            .unwrap();
    assert!(matches!(
        input,
        InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
            x: 45,
            y: 67
        }
    ));
    assert!(
        decode_control::<InputEvent>(br#"{"type":"button","button":"left","pressed":true}"#)
            .is_err()
    );
}

#[test]
fn oversized_control_is_rejected_in_both_directions() {
    let bytes = vec![b' '; 65_537];
    assert!(matches!(
        decode_control::<SignalMessage>(&bytes),
        Err(ProtocolError::ControlTooLarge)
    ));
    assert_eq!(
        encode_control(&"x".repeat(65_535)).err(),
        Some(ProtocolError::ControlTooLarge)
    );
    assert_eq!(encode_control(&"x".repeat(65_534)).unwrap().len(), 65_536);
}

#[test]
fn frame_size_and_queue_are_bounded() {
    let frame = EncodedFrame {
        generation: 1,
        pts_ns: 2,
        keyframe: true,
        data: vec![0; 8 * 1024 * 1024],
    };
    assert!(frame.validate().is_ok());
    let mut oversized = frame.clone();
    oversized.data.push(0);
    assert_eq!(oversized.validate(), Err(ProtocolError::FrameTooLarge));
    let (sender, receiver) = frame_channel();
    for _ in 0..FRAME_QUEUE_CAPACITY {
        sender.try_send(frame.clone()).unwrap();
    }
    assert!(matches!(
        sender.try_send(frame),
        Err(FrameSendError::Full(_))
    ));
    assert!(receiver.recv().is_ok());
    assert!(matches!(
        sender.try_send(oversized),
        Err(FrameSendError::Invalid(ProtocolError::FrameTooLarge))
    ));
}

#[test]
fn signaling_and_helper_messages_round_trip() {
    let signal = SignalMessage::Offer {
        epoch: 0,
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        sdp: "v=0\r\n".into(),
    };
    assert!(
        matches!(decode_control::<SignalMessage>(&encode_control(&signal).unwrap()).unwrap(), SignalMessage::Offer { sdp, .. } if sdp == "v=0\r\n")
    );
    let request = HelperRequest::ReleaseAll {
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        generation: 2,
    };
    assert!(matches!(
        decode_control::<HelperRequest>(&encode_control(&request).unwrap()).unwrap(),
        HelperRequest::ReleaseAll { generation: 2, .. }
    ));
    let event = HelperEvent::DisplayChanged(DisplayInfo {
        generation: 2,
        width: 1920,
        height: 1080,
    });
    assert!(matches!(
        decode_control::<HelperEvent>(&encode_control(&event).unwrap()).unwrap(),
        HelperEvent::DisplayChanged(DisplayInfo { width: 1920, .. })
    ));
}

#[test]
fn ice_servers_round_trip_without_debugging_secrets() {
    let server = IceServer {
        urls: vec!["turn:example.test:3478?transport=udp".into()],
        username: Some("temporary-user".into()),
        credential: Some("temporary-secret".into()),
        expires_at_unix_secs: Some(12345),
    };
    let decoded: IceServer = decode_control(&encode_control(&server).unwrap()).unwrap();
    assert!(server == decoded);
    assert!(!format!("{server:?}").contains("temporary-"));
    let signal = SignalMessage::Restart {
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        epoch: 2,
    };
    assert!(signal == decode_control(&encode_control(&signal).unwrap()).unwrap());
}
