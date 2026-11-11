use binachicken_agent::sync_bridge::*;
use binachicken_protocol::*;
use binachicken_sync::{FrameReader, write_frame};
use std::{
    cell::RefCell,
    collections::VecDeque,
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
struct Port {
    inbound: RefCell<VecDeque<Vec<u8>>>,
    sent: RefCell<Vec<Vec<u8>>>,
    busy: RefCell<bool>,
}
impl SyncPort for Port {
    fn send(&self, frame: &[u8]) -> Result<(), PortError> {
        if *self.busy.borrow() {
            return Err(PortError::Busy);
        }
        self.sent.borrow_mut().push(frame.to_vec());
        Ok(())
    }
    fn recv(&self) -> Option<Vec<u8>> {
        self.inbound.borrow_mut().pop_front()
    }
}
fn me() -> u32 {
    unsafe { libc::geteuid() }
}
fn dir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bc-agent-sync-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn control(m: &SyncMessage) -> Vec<u8> {
    encode_sync_control(m).unwrap()
}
fn until(bridge: &mut SyncBridge, port: &Port, mut done: impl FnMut(&SyncBridge) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while !done(bridge) {
        bridge.poll(port).unwrap();
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn frames_flow_both_ways_for_the_authorized_user() {
    let path = dir().join("a.sock");
    let mut bridge = SyncBridge::bind(&path, Box::new(|| Some(me()))).unwrap();
    let port = Port::default();
    let mut helper = UnixStream::connect(&path).unwrap();
    helper
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let offer = control(&SyncMessage::Offer {
        id: 1,
        kind: SyncKind::Text,
        name: String::new(),
        size: 2,
    });
    write_frame(&mut helper, &offer).unwrap();
    until(&mut bridge, &port, |_| !port.sent.borrow().is_empty());
    assert_eq!(port.sent.borrow()[0], offer);
    let reply = control(&SyncMessage::Done { id: 9 });
    port.inbound.borrow_mut().push_back(reply.clone());
    let mut reader = FrameReader::default();
    let mut got = vec![];
    until(&mut bridge, &port, |_| {
        let mut buf = [0u8; 256];
        if let Ok(n) = helper.read(&mut buf) {
            got.extend(reader.push(&buf[..n]).unwrap());
        }
        !got.is_empty()
    });
    assert_eq!(got[0], reply);
}

#[test]
fn other_users_and_locked_sessions_are_refused() {
    let path = dir().join("b.sock");
    let allowed = Arc::new(AtomicU32::new(me() + 1)); // someone else is the active user
    let a = allowed.clone();
    let mut bridge =
        SyncBridge::bind(&path, Box::new(move || Some(a.load(Ordering::Relaxed)))).unwrap();
    let port = Port::default();
    let _intruder = UnixStream::connect(&path).unwrap();
    for _ in 0..20 {
        bridge.poll(&port).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!bridge.connected());
    // Now the real user is active, then the session locks mid-connection.
    allowed.store(me(), Ordering::Relaxed);
    let mut helper = UnixStream::connect(&path).unwrap();
    until(&mut bridge, &port, |b| b.connected());
    allowed.store(me() + 1, Ordering::Relaxed);
    // The periodic re-check drops the connection; force it by waiting past the interval.
    std::thread::sleep(Duration::from_millis(5100));
    bridge.poll(&port).unwrap();
    assert!(!bridge.connected(), "lock/user change must drop the helper");
    let _ = helper.set_read_timeout(Some(Duration::from_millis(200)));
    let mut b = [0u8; 8];
    assert!(matches!(helper.read(&mut b), Ok(0) | Err(_)));
}

#[test]
fn offers_are_refused_immediately_when_no_helper_is_connected() {
    let path = dir().join("c.sock");
    let mut bridge = SyncBridge::bind(&path, Box::new(|| None)).unwrap();
    let port = Port::default();
    port.inbound
        .borrow_mut()
        .push_back(control(&SyncMessage::Offer {
            id: 7,
            kind: SyncKind::File,
            name: "f".into(),
            size: 1,
        }));
    port.inbound
        .borrow_mut()
        .push_back(encode_chunk(7, 0, b"x").unwrap());
    bridge.poll(&port).unwrap();
    let sent = port.sent.borrow();
    assert_eq!(
        sent.len(),
        1,
        "only the offer gets an answer; stray chunks are dropped"
    );
    assert!(matches!(
        decode_frame(&sent[0]).unwrap(),
        SyncFrame::Control(SyncMessage::Reject {
            id: 7,
            reason: RejectReason::Disabled
        })
    ));
}

#[test]
fn backpressure_keeps_frames_and_oversize_or_garbage_drops_only_the_helper() {
    let path = dir().join("d.sock");
    let mut bridge = SyncBridge::bind(&path, Box::new(|| Some(me()))).unwrap();
    let port = Port::default();
    *port.busy.borrow_mut() = true;
    let mut helper = UnixStream::connect(&path).unwrap();
    let frame = control(&SyncMessage::Cancel { id: 3 });
    write_frame(&mut helper, &frame).unwrap();
    for _ in 0..30 {
        bridge.poll(&port).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(port.sent.borrow().is_empty());
    *port.busy.borrow_mut() = false;
    until(&mut bridge, &port, |_| !port.sent.borrow().is_empty());
    assert_eq!(port.sent.borrow().as_slice(), &[frame]);
    // A length prefix beyond the limit is a bad helper: drop it, keep the session.
    helper
        .write_all(&((SYNC_FRAME_LIMIT as u32 + 1).to_be_bytes()))
        .unwrap();
    until(&mut bridge, &port, |b| !b.connected());
}

#[test]
fn session_parsing_fails_closed() {
    assert_eq!(parse_session("User=1000\nLockedHint=no\n"), Some(1000));
    assert_eq!(parse_session("User=1000\nLockedHint=yes\n"), None);
    assert_eq!(
        parse_session("User=1000\n"),
        None,
        "unknown lock state denies"
    );
    assert_eq!(parse_session("LockedHint=no\n"), None);
    assert_eq!(parse_session("User=abc\nLockedHint=no\n"), None);
    assert_eq!(parse_session("garbage"), None);
}

// ---- SSH tunnel through the bridge ----
use binachicken_sync::{Busy, Mux};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

fn echo_server() -> std::net::SocketAddr {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            std::thread::spawn(move || {
                let (mut r, mut w) = (s.try_clone().unwrap(), s);
                let mut b = [0u8; 4096];
                while let Ok(n) = r.read(&mut b) {
                    if n == 0 || w.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}
/// Controller side wired to the bridge through the fake port.
fn drive(bridge: &mut SyncBridge, port: &Port, client: &mut Mux, rounds: usize) {
    for _ in 0..rounds {
        let mut out = vec![];
        client.poll(&mut |f| {
            out.push(f.to_vec());
            Ok::<(), Busy>(())
        });
        port.inbound.borrow_mut().extend(out);
        bridge.poll(port).unwrap();
        let sent: Vec<_> = port.sent.borrow_mut().drain(..).collect();
        for f in sent {
            client.on_frame(&f).unwrap();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn tunnel_only_bridge_serves_the_active_user_and_relays_bytes() {
    let mut bridge = SyncBridge::tunnel_only();
    bridge.enable_tunnel(echo_server(), Box::new(|| (None, "hieulv".into())));
    let port = Port::default();
    let mut client = Mux::client(0);
    client.set_enabled(true).unwrap();
    drive(&mut bridge, &port, &mut client, 50);
    let info = client.info().expect("agent answered Hello");
    assert!(info.enabled && info.user == "hieulv");
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, client.local_port().unwrap())).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    s.write_all(b"ping through ssh tunnel").unwrap();
    let mut got = vec![0u8; 23];
    let mut read = 0;
    for _ in 0..500 {
        drive(&mut bridge, &port, &mut client, 1);
        if let Ok(n) = s.read(&mut got[read..]) {
            read += n;
        }
        if read == 23 {
            break;
        }
    }
    assert_eq!(&got, b"ping through ssh tunnel");
}

#[test]
fn locked_or_absent_seat_denies_and_unconfigured_tunnel_says_disabled() {
    // Locked screen: info reports disabled; no connection is made.
    let mut bridge = SyncBridge::tunnel_only();
    bridge.enable_tunnel(
        echo_server(),
        Box::new(|| (Some(RefuseReason::Locked), String::new())),
    );
    let port = Port::default();
    let mut client = Mux::client(0);
    client.set_enabled(true).unwrap();
    drive(&mut bridge, &port, &mut client, 50);
    assert!(client.info().is_some_and(|i| !i.enabled));

    // Bridge without any tunnel configured (e.g. clipboard-only install).
    let path = dir().join("t.sock");
    let mut plain = SyncBridge::bind(&path, Box::new(|| None)).unwrap();
    let mut client = Mux::client(0);
    client.set_enabled(true).unwrap();
    drive(&mut plain, &port, &mut client, 50);
    assert!(client.info().is_some_and(|i| !i.enabled));
}

#[test]
fn seat_parsing_requires_a_user_a_name_and_a_known_lock_state() {
    assert_eq!(
        parse_seat("User=1000\nName=hieulv\nLockedHint=no\n"),
        Seat::Active {
            uid: 1000,
            name: "hieulv".into()
        }
    );
    assert_eq!(
        parse_seat("User=1000\nName=hieulv\nLockedHint=yes\n"),
        Seat::Locked
    );
    assert_eq!(parse_seat("User=1000\nName=hieulv\n"), Seat::Absent);
    assert_eq!(parse_seat("Name=x\nLockedHint=no\n"), Seat::Absent);
    assert_eq!(parse_seat("garbage"), Seat::Absent);
}
