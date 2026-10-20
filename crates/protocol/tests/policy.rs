use remotevnc_protocol::*;

fn display() -> DisplayInfo {
    DisplayInfo {
        generation: 3,
        width: 1920,
        height: 1080,
    }
}
fn envelope(event: InputEvent, sequence: u64) -> InputEnvelope {
    InputEnvelope {
        version: 1,
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        generation: 3,
        sequence,
        event,
    }
}
fn guard() -> InputGuard {
    InputGuard::new(SessionId::from_uuid(uuid::Uuid::nil()), display(), 0).unwrap()
}

#[test]
fn lease_expires_at_sixty_seconds_and_renews() {
    let mut guard = guard();
    assert!(!guard.expired(59_999));
    assert!(guard.expired(60_000));
    assert_eq!(
        guard.accept(&envelope(InputEvent::ReleaseAll, 1), 60_000),
        Err(ProtocolError::LeaseExpired)
    );
    guard.renew_lease(60_000);
    assert!(!guard.expired(119_999));
    assert!(guard.expired(120_000));
}

#[test]
fn other_session_is_rejected() {
    let mut input = envelope(InputEvent::ReleaseAll, 1);
    input.session_id = SessionId::from_uuid(uuid::Uuid::from_u128(1));
    assert_eq!(guard().accept(&input, 0), Err(ProtocolError::WrongSession));
}

#[test]
fn old_generation_is_rejected() {
    let mut input = envelope(InputEvent::ReleaseAll, 1);
    input.generation = 2;
    assert_eq!(
        guard().accept(&input, 0),
        Err(ProtocolError::WrongGeneration)
    );
}

#[test]
fn pointer_sequences_do_not_reject_ordered_channel_events() {
    let mut guard = guard();
    assert!(
        guard
            .accept(&envelope(InputEvent::PointerMove { x: 10, y: 20 }, 100), 0)
            .is_ok()
    );
    assert_eq!(
        guard.accept(&envelope(InputEvent::PointerMove { x: 10, y: 20 }, 100), 0),
        Err(ProtocolError::StaleSequence)
    );
    assert_eq!(
        guard.accept(&envelope(InputEvent::PointerMove { x: 10, y: 20 }, 99), 0),
        Err(ProtocolError::StaleSequence)
    );
    assert!(
        guard
            .accept(
                &envelope(
                    InputEvent::Key {
                        keysym: 0xff0d,
                        pressed: true
                    },
                    1
                ),
                0
            )
            .is_ok()
    );
    assert!(
        guard
            .accept(&envelope(InputEvent::ReleaseAll, 2), 0)
            .is_ok()
    );
}

#[test]
fn coordinates_are_validated_before_advancing_sequence() {
    let mut guard = guard();
    for (x, y) in [(1920, 0), (0, 1080)] {
        assert_eq!(
            guard.accept(&envelope(InputEvent::PointerMove { x, y }, 9), 0),
            Err(ProtocolError::InvalidCoordinates)
        );
    }
    assert!(
        guard
            .accept(
                &envelope(InputEvent::PointerMove { x: 1919, y: 1079 }, 9),
                0
            )
            .is_ok()
    );
}

#[test]
fn input_version_text_bytes_and_display_ack_are_checked() {
    let mut guard = guard();
    let mut input = envelope(InputEvent::ReleaseAll, 1);
    input.version = 2;
    assert_eq!(
        guard.accept(&input, 0),
        Err(ProtocolError::UnsupportedVersion)
    );
    assert!(
        guard
            .accept(
                &envelope(
                    InputEvent::Text {
                        text: "é".repeat(8192)
                    },
                    2
                ),
                0
            )
            .is_ok()
    );
    assert_eq!(
        guard.accept(
            &envelope(
                InputEvent::Text {
                    text: "é".repeat(8193)
                },
                3
            ),
            0
        ),
        Err(ProtocolError::TextTooLarge)
    );
    assert!(
        guard
            .accept(&envelope(InputEvent::DisplayAck, 4), 0)
            .is_ok()
    );
    assert_eq!(
        InputGuard::new(
            SessionId::from_uuid(uuid::Uuid::nil()),
            DisplayInfo {
                generation: 1,
                width: 0,
                height: 1
            },
            0
        )
        .err(),
        Some(ProtocolError::InvalidDisplay)
    );
}

#[test]
fn display_update_invalidates_old_input_and_resets_motion_sequence() {
    let mut guard = guard();
    guard
        .accept(&envelope(InputEvent::PointerMove { x: 1, y: 1 }, 99), 0)
        .unwrap();
    guard
        .update_display(DisplayInfo {
            generation: 4,
            width: 800,
            height: 600,
        })
        .unwrap();
    assert_eq!(
        guard.accept(&envelope(InputEvent::ReleaseAll, 1), 0),
        Err(ProtocolError::WrongGeneration)
    );
    let mut input = envelope(InputEvent::PointerMove { x: 799, y: 599 }, 0);
    input.generation = 4;
    assert!(guard.accept(&input, 0).is_ok());
    assert_eq!(
        guard.update_display(display()),
        Err(ProtocolError::WrongGeneration)
    );
}
