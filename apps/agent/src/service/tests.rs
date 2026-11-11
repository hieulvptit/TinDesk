use super::*;
use std::os::unix::net::UnixListener;

#[test]
fn signaling_burst_waits_for_network_capacity_without_retiring_session() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let helper = std::thread::spawn(move || listener.accept().unwrap().0);
    let grant = SessionGrant {
        session_id: SessionId::from_uuid(uuid::Uuid::new_v4()),
        client_id: DeviceId::from_uuid(uuid::Uuid::new_v4()),
        agent_id: DeviceId::from_uuid(uuid::Uuid::new_v4()),
        lease_expires_at: unix_ms() + 60_000,
        ice_servers: vec![],
        turn_status: TurnStatus::Unavailable,
    };
    let mut live = Live::new(
        &Config {
            backend_origin: "http://127.0.0.1".into(),
            identity_path: dir.path().into(),
            helper_socket: socket,
            seat: "seat0".into(),
            relay_only: false,
            sync_socket: None,
            ssh_port: None,
        },
        grant,
        boot_time_ms(),
        CancellationToken::new(),
    )
    .unwrap();
    let _helper = helper.join().unwrap();
    // Real peer signals arrive while the production network queue is almost full.
    for generation in 1..=32 {
        live.peer.display_unavailable(generation).unwrap();
    }
    let (tx, mut rx) = mpsc::channel(32);
    for generation in 0..32 {
        tx.try_send(DeviceCommand::IceInstalled {
            session_id: live.grant.session_id,
            generation,
        })
        .unwrap();
    }
    live.acknowledged = true;
    let mut received = Vec::new();
    for _ in 0..16 {
        live.poll(&tx)
            .expect("a temporary signaling backlog must not retire the session");
        assert!(
            !live.acknowledged,
            "input reset must run even while signaling is full"
        );
        while let Ok(command) = rx.try_recv() {
            if let DeviceCommand::Signal {
                message: SignalMessage::DisplayUnavailable { generation, .. },
                ..
            } = command
            {
                received.push(generation);
            }
        }
    }
    assert_eq!(received, (1..=32).collect::<Vec<_>>());
    assert!(!live.stop.is_cancelled());
}
