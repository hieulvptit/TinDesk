use remotevnc_protocol::*;
use remotevnc_seat_helper::ipc::{FrameDecoder, SessionPolicy, authorize_uid};
use uuid::Uuid;

#[test]
fn unauthorized_peer_uid_is_rejected() {
    assert!(authorize_uid(1001, 1001).is_ok());
    assert!(authorize_uid(0, 1001).is_err());
}
#[test]
fn framing_rejects_oversize_before_receiving_payload() {
    let mut decoder = FrameDecoder::default();
    assert!(
        decoder
            .push(&((CONTROL_PAYLOAD_LIMIT + 1) as u32).to_be_bytes())
            .is_err()
    );
}
#[test]
fn framing_accepts_fragmented_and_coalesced_messages() {
    let data = encode_control(&HelperEvent::Ready).unwrap();
    let mut bytes = (data.len() as u32).to_be_bytes().to_vec();
    bytes.extend(&data);
    let mut decoder = FrameDecoder::default();
    assert!(decoder.push(&bytes[..2]).unwrap().is_empty());
    let result = decoder.push(&bytes[2..]).unwrap();
    assert_eq!(result, vec![data]);
}
#[test]
fn stale_generation_and_expired_renewals_fail_closed() {
    let id = SessionId(Uuid::new_v4());
    let display = DisplayInfo {
        generation: 2,
        width: 800,
        height: 600,
    };
    let mut policy = SessionPolicy::default();
    policy.start(id, display, 0).unwrap();
    assert!(policy.renew(id, 1, 1).is_err());
    assert!(policy.renew(id, 2, LEASE_DURATION_MS).is_err());
    assert!(policy.expired(LEASE_DURATION_MS));
    assert!(policy.start(SessionId(Uuid::new_v4()), display, 1).is_err());
}
#[test]
fn lease_extension_requires_explicit_renewal() {
    let id = SessionId(Uuid::new_v4());
    let display = DisplayInfo {
        generation: 2,
        width: 800,
        height: 600,
    };
    let mut policy = SessionPolicy::default();
    policy.start(id, display, 0).unwrap();
    policy.renew(id, 2, 15_000).unwrap();
    assert!(!policy.expired(60_000));
    assert!(policy.expired(75_000));
}

#[test]
fn new_helper_wire_messages_round_trip() {
    let id = SessionId(Uuid::new_v4());
    let req = HelperRequest::RenewLease {
        session_id: id,
        generation: 7,
        deadline_boot_ms: 60_000,
    };
    assert!(matches!(
        decode_control::<HelperRequest>(&encode_control(&req).unwrap()).unwrap(),
        HelperRequest::RenewLease { generation: 7, .. }
    ));
    let event = HelperEvent::DisplayUnavailable { generation: 8 };
    assert!(matches!(
        decode_control::<HelperEvent>(&encode_control(&event).unwrap()).unwrap(),
        HelperEvent::DisplayUnavailable { generation: 8 }
    ));
}

#[test]
#[cfg(target_os = "linux")]
fn peer_credentials_come_from_kernel_and_wrong_uid_is_rejected() {
    use remotevnc_seat_helper::ipc::peer_uid;
    use std::os::unix::net::UnixStream;
    let (a, _b) = UnixStream::pair().unwrap();
    let actual = peer_uid(&a).unwrap();
    assert_eq!(actual, unsafe { libc::geteuid() });
    assert!(authorize_uid(actual, actual.wrapping_add(1)).is_err());
}

#[test]
#[cfg(target_os = "linux")]
fn helper_rejects_oversize_frame_and_removes_socket_on_sigterm() {
    use std::{
        fs,
        io::{Read, Write},
        os::unix::{fs::PermissionsExt, net::UnixStream},
        process::{Command, Stdio},
        thread,
        time::Duration,
    };
    let dir = std::env::temp_dir().join(format!("remotevnc-ipc-{}", Uuid::new_v4()));
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    let socket = dir.join("helper.sock");
    let mut child = Command::new(env!("CARGO_BIN_EXE_remotevnc-seat-helper"))
        .arg("--socket")
        .arg(&socket)
        .arg("--agent-uid")
        .arg(unsafe { libc::geteuid() }.to_string())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let mut peer = UnixStream::connect(&socket).unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut header = [0u8; 4];
    peer.read_exact(&mut header).unwrap();
    let mut payload = vec![0; u32::from_be_bytes(header) as usize];
    peer.read_exact(&mut payload).unwrap();
    assert!(matches!(
        decode_control::<HelperEvent>(&payload).unwrap(),
        HelperEvent::Ready
    ));
    peer.write_all(&((CONTROL_PAYLOAD_LIMIT + 1) as u32).to_be_bytes())
        .unwrap();
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    assert!(child.wait().unwrap().success());
    assert!(!socket.exists());
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o755
    );
    fs::remove_dir(dir).unwrap();
}

#[test]
#[ignore = "requires scripts/test-x11.sh Xvfb fixture"]
fn stalled_discovery_cannot_delay_responsive_input_cleanup() {
    use remotevnc_seat_helper::{
        ipc::serve_with_guardian as serve,
        seat::{DisplayTarget, ServerIdentity, command_output},
        x11,
    };
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };
    use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
    fn receive(peer: &mut UnixStream) -> HelperEvent {
        let mut header = [0; 4];
        peer.read_exact(&mut header).unwrap();
        let mut data = vec![0; u32::from_be_bytes(header) as usize];
        peer.read_exact(&mut data).unwrap();
        decode_control(&data).unwrap()
    }
    for reason in ["lease", "disconnect", "signal"] {
        let t = DisplayTarget {
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
            width: 800,
            height: 600,
        };
        let c = x11::connect(&t).unwrap();
        let root = c.setup().roots[0].root;
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        peer.set_write_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let boot_now = remotevnc_seat_helper::recovery::boot_time_ms().unwrap();
        let now = Arc::new(AtomicU64::new(boot_now));
        let guardian_path = std::env::temp_dir().join(format!(
            "remotevnc-stalled-guardian-{}.sock",
            Uuid::new_v4()
        ));
        let guardian_listener = std::os::unix::net::UnixListener::bind(&guardian_path).unwrap();
        let guardian_worker = thread::spawn(move || {
            let (mut peer, _) = guardian_listener.accept().unwrap();
            remotevnc_seat_helper::recovery::serve(&mut peer)
        });
        let helper_guardian_path = guardian_path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_now = now.clone();
        let thread_stop = stop.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut first = true;
            let mut discovery = |cancelled: &dyn Fn() -> bool| {
                if first {
                    first = false;
                    return Ok(Some(t.clone()));
                }
                started_tx.send(()).unwrap();
                command_output(
                    Command::new("/bin/sleep").arg("5"),
                    Duration::from_secs(5),
                    cancelled,
                )?;
                Ok(Some(t.clone()))
            };
            serve(
                &mut peer,
                &mut discovery,
                &mut 0,
                || thread_now.load(Ordering::Relaxed),
                &thread_stop,
                &helper_guardian_path,
            )
        });
        assert!(matches!(receive(&mut client), HelperEvent::Ready));
        let display = match receive(&mut client) {
            HelperEvent::DisplayChanged(d) => d,
            _ => panic!("expected display"),
        };
        let id = SessionId(Uuid::new_v4());
        // Coalesce Start + held inputs so discovery starts only after they are owned.
        let mut frames = Vec::new();
        for request in [
            HelperRequest::Start {
                session_id: id,
                display,
                deadline_boot_ms: now.load(Ordering::Relaxed) + LEASE_DURATION_MS,
            },
            HelperRequest::Input(InputEnvelope {
                version: WIRE_VERSION,
                session_id: id,
                generation: display.generation,
                sequence: 0,
                event: InputEvent::DisplayAck,
            }),
            HelperRequest::Input(InputEnvelope {
                version: WIRE_VERSION,
                session_id: id,
                generation: display.generation,
                sequence: 1,
                event: InputEvent::Key {
                    keysym: 0xffe1,
                    pressed: true,
                },
            }),
            HelperRequest::Input(InputEnvelope {
                version: WIRE_VERSION,
                session_id: id,
                generation: display.generation,
                sequence: 2,
                event: InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                    x: 1,
                    y: 1,
                },
            }),
        ] {
            let bytes = encode_control(&request).unwrap();
            frames.extend((bytes.len() as u32).to_be_bytes());
            frames.extend(bytes);
        }
        client.write_all(&frames).unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_ne!(
            u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100,
            0
        );
        assert!(
            c.query_keymap()
                .unwrap()
                .reply()
                .unwrap()
                .keys
                .iter()
                .any(|k| *k != 0)
        );
        let began_cleanup = Instant::now();
        match reason {
            "lease" => now.store(boot_now + LEASE_DURATION_MS, Ordering::Relaxed),
            "signal" => stop.store(true, Ordering::Relaxed),
            "disconnect" => {
                client.shutdown(std::net::Shutdown::Both).unwrap();
            }
            _ => unreachable!(),
        }
        while !worker.is_finished() {
            assert!(
                began_cleanup.elapsed() < Duration::from_secs(1),
                "{reason} waited for stalled discovery"
            );
            thread::sleep(Duration::from_millis(5));
        }
        let _ = worker.join().unwrap(); // disconnected peer may make Stopped notification fail.
        guardian_worker.join().unwrap().unwrap();
        std::fs::remove_file(guardian_path).unwrap();
        assert!(
            c.query_keymap()
                .unwrap()
                .reply()
                .unwrap()
                .keys
                .iter()
                .all(|k| *k == 0)
        );
        assert_eq!(
            u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100,
            0,
            "{reason} left button held"
        );
    }
}

#[test]
fn display_reset_requires_ack_and_does_not_renew_lease() {
    let id = SessionId(Uuid::nil());
    let mut policy = SessionPolicy::default();
    let display = DisplayInfo {
        generation: 1,
        width: 800,
        height: 600,
    };
    policy.start(id, display, 0).unwrap();
    let mut input = InputEnvelope {
        version: WIRE_VERSION,
        session_id: id,
        generation: 1,
        sequence: 0,
        event: InputEvent::PointerMove { x: 1, y: 1 },
    };
    assert!(policy.accept(&input, 1).is_err());
    input.event = InputEvent::DisplayAck;
    policy.accept(&input, 2).unwrap();
    policy
        .update_display(DisplayInfo {
            generation: 2,
            ..display
        })
        .unwrap();
    input.generation = 2;
    input.event = InputEvent::PointerMove { x: 1, y: 1 };
    assert!(policy.accept(&input, 3).is_err());
    input.event = InputEvent::DisplayAck;
    policy.accept(&input, 4).unwrap();
    input.event = InputEvent::PointerMove { x: 1, y: 1 };
    policy.accept(&input, 5).unwrap();
    assert!(policy.expired(LEASE_DURATION_MS));
}

#[test]
fn explicit_deadline_survives_display_restart_and_rejects_extension_by_traffic() {
    let id = SessionId(Uuid::new_v4());
    let display = DisplayInfo {
        generation: 1,
        width: 800,
        height: 600,
    };
    let mut policy = SessionPolicy::default();
    assert!(policy.start_until(id, display, 1_000, 1_000).is_err());
    assert!(policy.start_until(id, display, 1_000, 61_001).is_err());
    policy.start_until(id, display, 1_000, 3_000).unwrap();
    assert!(!policy.expired(2_999));
    assert!(policy.expired(3_000));
    policy.stop();
    policy
        .start_until(
            id,
            DisplayInfo {
                generation: 2,
                ..display
            },
            2_500,
            3_000,
        )
        .unwrap();
    assert!(policy.renew_until(id, 2, 2_600, 2_999).is_err());
    assert!(
        policy.expired(3_000),
        "display restart must preserve the existing absolute deadline"
    );
    assert!(
        policy.renew_until(id, 2, 3_000, 63_000).is_err(),
        "expired sessions cannot be revived"
    );
}
