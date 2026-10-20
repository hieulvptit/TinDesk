use remotevnc_client::{app::Authority, video::LatestFrame};
use remotevnc_transport::DecodedFrame;
#[test]
fn delayed_and_duplicate_grants_never_gain_authority_from_receipt_or_display() {
    let mut a = Authority::default();
    assert!(a.grant(160_000, 110_000, 10_000, 0));
    assert_eq!(a.deadline_ms, 60_000);
    assert!(a.grant(160_000, 120_000, 20_000, 20_000));
    assert_eq!(a.deadline_ms, 60_000);
    assert!(a.grant(175_000, 125_000, 25_000, 25_000));
    assert_eq!(a.deadline_ms, 75_000);
    assert!(!a.grant(220_000, 175_000, 75_000, 75_000));
    assert!(a.expired(75_000));
}
#[test]
fn newest_decoded_frame_replaces_old_without_queueing() {
    let frames = LatestFrame::default();
    for n in 1..100 {
        frames.replace(DecodedFrame {
            generation: n,
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            pts_ns: Some(n),
        });
    }
    assert_eq!(frames.take().unwrap().generation, 99);
    assert!(frames.take().is_none());
}
use remotevnc_client::app::ControllerSession;
use remotevnc_protocol::*;
use remotevnc_transport::{Peer, PeerConfig, PeerEvent, PeerRole};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
fn grant() -> SessionGrant {
    SessionGrant {
        session_id: SessionId(uuid::Uuid::new_v4()),
        client_id: DeviceId(uuid::Uuid::new_v4()),
        agent_id: DeviceId(uuid::Uuid::new_v4()),
        lease_expires_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 60_000,
        ice_servers: vec![],
        turn_status: TurnStatus::Unavailable,
    }
}
#[test]
fn native_focus_release_preserves_session_restart_fences_old_signals_and_future_epoch_fails() {
    let grant = grant();
    let mut client =
        ControllerSession::new(grant.clone(), Instant::now(), false, LatestFrame::default())
            .unwrap();
    let mut agent = Peer::new(PeerConfig {
        session_id: grant.session_id,
        local_peer_id: grant.agent_id,
        remote_peer_id: grant.client_id,
        role: PeerRole::Agent,
        display: None,
        ice_servers: vec![],
        relay_only: false,
    })
    .unwrap();
    agent.enable_h264().unwrap();
    agent
        .update_display(DisplayInfo {
            generation: 1,
            width: 1280,
            height: 720,
        })
        .unwrap();
    let events = agent.events().clone();
    let until = Instant::now() + Duration::from_secs(10);
    let mut sent = false;
    let mut released = false;
    while Instant::now() < until {
        for event in events.try_iter() {
            match event {
                PeerEvent::Signal(message) => client.signal(grant.agent_id, message).unwrap(),
                PeerEvent::Input(input) if matches!(input.event, InputEvent::ReleaseAll) => {
                    released = true
                }
                PeerEvent::Error(e) => panic!("agent error: {e}"),
                _ => {}
            }
        }
        for command in client.poll().unwrap() {
            if let DeviceCommand::Signal { message, .. } = command {
                agent.handle_signal(message).unwrap();
            }
        }
        if client.ready && !sent {
            client
                .input(
                    1,
                    client.peer.epoch(),
                    vec![
                        InputEvent::Key {
                            keysym: 0xffe3,
                            pressed: true,
                        },
                        InputEvent::ReleaseAll,
                    ],
                )
                .unwrap();
            sent = true;
        }
        if released {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(released, "real reliable channel receives focus ReleaseAll");
    assert!(client.ready);
    assert!(!client.peer.is_closed());
    let epoch = client.peer.epoch();
    assert!(epoch > 0);
    client
        .signal(
            grant.agent_id,
            SignalMessage::End {
                session_id: grant.session_id,
                epoch: epoch - 1,
            },
        )
        .unwrap();
    assert!(!client.peer.is_closed());
    assert!(
        client
            .signal(
                grant.agent_id,
                SignalMessage::End {
                    session_id: grant.session_id,
                    epoch: epoch + 1
                }
            )
            .is_err()
    );
    assert!(client.install_ice(1, false, vec![]).unwrap().is_none());
    assert!(matches!(
        client.install_ice(1, true, vec![]).unwrap(),
        Some(DeviceCommand::IceInstalled { generation: 1, .. })
    ));
}
#[test]
fn independent_deadline_closes_native_peer_without_ui_or_worker_polling() {
    let mut grant = grant();
    grant.lease_expires_at -= 59_500;
    let client =
        ControllerSession::new(grant, Instant::now(), false, LatestFrame::default()).unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !client.peer.is_closed() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(client.peer.is_closed());
}
