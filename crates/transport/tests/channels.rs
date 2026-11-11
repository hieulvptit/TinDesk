use binachicken_protocol::*;
use binachicken_transport::channels::*;

fn envelope(event: InputEvent) -> InputEnvelope {
    InputEnvelope {
        version: WIRE_VERSION,
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        generation: 7,
        sequence: 1,
        event,
    }
}
fn decoder() -> InputDecoder {
    {
        let mut d = InputDecoder::new(
            SessionId::from_uuid(uuid::Uuid::nil()),
            Some(DisplayInfo {
                generation: 7,
                width: 100,
                height: 100,
            }),
            0,
        )
        .unwrap();
        d.decode(
            CONTROL.label,
            &encode_control(&envelope(InputEvent::DisplayAck)).unwrap(),
            0,
        )
        .unwrap();
        d
    }
}

#[test]
fn channel_contracts_are_low_latency_motion_and_reliable_control() {
    const { assert!(!MOTION.ordered) };
    assert_eq!(MOTION.max_retransmits, Some(0));
    const { assert!(CONTROL.ordered) };
    assert_eq!(CONTROL.max_retransmits, None);
    assert_eq!(
        channel_for(&InputEvent::PointerMove { x: 1, y: 2 }),
        MOTION.label
    );
    assert_eq!(channel_for(&InputEvent::ReleaseAll), CONTROL.label);
}

#[test]
fn decoder_rejects_malformed_oversized_wrong_scope_and_wrong_channel() {
    let mut d = decoder();
    assert_eq!(
        d.decode(CONTROL.label, b"{bad", 0).err(),
        Some(ProtocolError::InvalidPayload)
    );
    assert_eq!(
        d.decode(CONTROL.label, &vec![0; CONTROL_PAYLOAD_LIMIT + 1], 0)
            .err(),
        Some(ProtocolError::ControlTooLarge)
    );
    let mut input = envelope(InputEvent::ReleaseAll);
    input.session_id = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    assert_eq!(
        d.decode(CONTROL.label, &encode_control(&input).unwrap(), 0)
            .err(),
        Some(ProtocolError::WrongSession)
    );
    input.session_id = envelope(InputEvent::ReleaseAll).session_id;
    input.generation = 6;
    assert_eq!(
        d.decode(CONTROL.label, &encode_control(&input).unwrap(), 0)
            .err(),
        Some(ProtocolError::WrongGeneration)
    );
    let input = envelope(InputEvent::Text {
        text: "x".repeat(TEXT_LIMIT + 1),
    });
    assert_eq!(
        d.decode(CONTROL.label, &encode_control(&input).unwrap(), 0)
            .err(),
        Some(ProtocolError::TextTooLarge)
    );
    let bytes = encode_control(&envelope(InputEvent::ReleaseAll)).unwrap();
    assert_eq!(
        d.decode(MOTION.label, &bytes, 0).err(),
        Some(ProtocolError::InvalidPayload)
    );
}

#[test]
fn decoder_preserves_exact_unicode_and_cross_channel_pointer_ordering() {
    let mut d = decoder();
    let input = envelope(InputEvent::Text {
        text: "Tiếng Việt|é|e\u{301}|中文|😀|\n\t\0".into(),
    });
    let decoded = d
        .decode(CONTROL.label, &encode_control(&input).unwrap(), 0)
        .unwrap();
    assert!(decoded == input);
    for (sequence, event, accepted) in [
        (10, InputEvent::PointerMove { x: 1, y: 2 }, true),
        (
            12,
            InputEvent::Button {
                button: MouseButton::Left,
                pressed: true,
                x: 30,
                y: 40,
            },
            true,
        ),
        (11, InputEvent::PointerMove { x: 2, y: 3 }, false),
        (13, InputEvent::PointerMove { x: 50, y: 60 }, true),
    ] {
        let mut input = envelope(event);
        input.sequence = sequence;
        assert_eq!(
            d.decode(
                channel_for(&input.event),
                &encode_control(&input).unwrap(),
                0
            )
            .is_ok(),
            accepted
        );
    }
}

#[test]
fn ice_debug_never_includes_credentials_or_credential_bearing_urls() {
    let ice = IceServer {
        urls: vec!["turn:secret-user:secret-password@example.test:3478".into()],
        username: Some("secret-user".into()),
        credential: Some("secret-password".into()),
        expires_at_unix_secs: None,
    };
    let debug = format!("{ice:?}");
    assert!(!debug.contains("secret-user"));
    assert!(!debug.contains("secret-password"));
}

#[test]
fn no_display_and_unacknowledged_generation_reject_input_without_renewing_lease() {
    let mut d = InputDecoder::new(SessionId::from_uuid(uuid::Uuid::nil()), None, 0).unwrap();
    let bytes = encode_control(&envelope(InputEvent::ReleaseAll)).unwrap();
    assert_eq!(
        d.decode(CONTROL.label, &bytes, 0).err(),
        Some(ProtocolError::DisplayUnavailable)
    );
    d.update_display(DisplayInfo {
        generation: 7,
        width: 100,
        height: 100,
    })
    .unwrap();
    let text = encode_control(&envelope(InputEvent::Text {
        text: "password".into(),
    }))
    .unwrap();
    assert_eq!(
        d.decode(CONTROL.label, &text, 0).err(),
        Some(ProtocolError::DisplayNotAcknowledged)
    );
    d.decode(
        CONTROL.label,
        &encode_control(&envelope(InputEvent::DisplayAck)).unwrap(),
        0,
    )
    .unwrap();
    d.decode(CONTROL.label, &text, 59_999).unwrap();
    assert!(d.expired(60_000));
    assert_eq!(
        d.decode(CONTROL.label, &text, 60_000).err(),
        Some(ProtocolError::LeaseExpired)
    );
    d.renew_lease(60_000);
    d.decode(CONTROL.label, &text, 60_001).unwrap();
}
