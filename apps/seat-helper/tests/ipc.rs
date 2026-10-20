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
