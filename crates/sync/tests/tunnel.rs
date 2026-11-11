use binachicken_protocol::*;
use binachicken_sync::{Busy, Mux};
use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Echo server: copies until EOF, then closes its write side (a half-closing peer like sshd).
fn echo() -> SocketAddr {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let (mut r, mut w) = (stream.try_clone().unwrap(), stream);
                let mut buf = [0u8; 8192];
                while let Ok(n) = r.read(&mut buf) {
                    if n == 0 || w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = w.shutdown(Shutdown::Write);
            });
        }
    });
    addr
}

struct Rig {
    client: Mux,
    agent: Mux,
    to_agent: VecDeque<Vec<u8>>,
    to_client: VecDeque<Vec<u8>>,
}
impl Rig {
    fn new(target: SocketAddr) -> Self {
        let mut client = Mux::client(0);
        let mut agent = Mux::agent(target);
        client.set_enabled(true).unwrap();
        agent.set_agent_policy(None, "hieulv");
        Self {
            client,
            agent,
            to_agent: VecDeque::new(),
            to_client: VecDeque::new(),
        }
    }
    fn step(&mut self) {
        let (mut a, mut c) = (vec![], vec![]);
        self.client.poll(&mut |f| {
            a.push(f.to_vec());
            Ok::<(), Busy>(())
        });
        self.agent.poll(&mut |f| {
            c.push(f.to_vec());
            Ok::<(), Busy>(())
        });
        self.to_agent.extend(a);
        self.to_client.extend(c);
        while let Some(f) = self.to_agent.pop_front() {
            self.agent.on_frame(&f).unwrap();
        }
        while let Some(f) = self.to_client.pop_front() {
            self.client.on_frame(&f).unwrap();
        }
        thread::sleep(Duration::from_micros(300));
    }
    fn until(&mut self, what: &str, mut done: impl FnMut(&Rig) -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while !done(self) {
            self.step();
            assert!(Instant::now() < end, "timed out: {what}");
        }
    }
    fn port(&self) -> u16 {
        self.client.local_port().unwrap()
    }
    fn ready(&mut self) {
        self.until("info", |r| r.client.info().is_some_and(|i| i.enabled));
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + i / 251) % 256) as u8).collect()
}
fn spawn_client(port: u16, data: Vec<u8>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut w = s.try_clone().unwrap();
        let writer = thread::spawn(move || {
            w.write_all(&data).unwrap();
            w.shutdown(Shutdown::Write).unwrap(); // half-close: still expect the echo back
        });
        let mut got = vec![];
        s.read_to_end(&mut got).unwrap();
        writer.join().unwrap();
        got
    })
}

#[test]
fn info_reaches_the_controller_and_large_data_round_trips_with_half_close() {
    let mut rig = Rig::new(echo());
    rig.ready();
    assert_eq!(rig.client.info().unwrap().user, "hieulv");
    let data = pattern(2 * 1024 * 1024);
    let handle = spawn_client(rig.port(), data.clone());
    rig.until("echo", |_| handle.is_finished());
    assert_eq!(handle.join().unwrap(), data);
    rig.until("cleanup", |r| {
        r.client.connections() == 0 && r.agent.connections() == 0
    });
}

#[test]
fn several_connections_run_concurrently_and_stay_independent() {
    let mut rig = Rig::new(echo());
    rig.ready();
    let jobs: Vec<_> = (1..=4)
        .map(|i| {
            let d = pattern(300_000 + i * 1000);
            (d.clone(), spawn_client(rig.port(), d))
        })
        .collect();
    rig.until("all done", |_| jobs.iter().all(|(_, h)| h.is_finished()));
    for (d, h) in jobs {
        assert_eq!(h.join().unwrap(), d);
    }
}

#[test]
fn disabled_agent_refuses_and_the_local_connection_is_closed_at_once() {
    let mut rig = Rig::new(echo());
    rig.agent.set_agent_policy(Some(RefuseReason::Disabled), "");
    rig.until("info", |r| r.client.info().is_some_and(|i| !i.enabled));
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, rig.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let mut buf = [0u8; 8];
    loop {
        rig.step();
        match s.read(&mut buf) {
            Ok(0) => break,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
            _ => {}
        }
        assert!(Instant::now() < end, "connection should have been dropped");
    }
    assert_eq!(rig.agent.connections(), 0);
}

#[test]
fn locking_the_session_tears_down_live_tunnels() {
    let mut rig = Rig::new(echo());
    rig.ready();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, rig.port())).unwrap();
    rig.until("open", |r| {
        r.agent.connections() == 1 && r.client.connections() == 1
    });
    s.write_all(b"hello").unwrap();
    rig.until("echo", |_| {
        let mut b = [0u8; 5];
        s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
        s.read_exact(&mut b).is_ok()
    });
    rig.agent
        .set_agent_policy(Some(RefuseReason::Locked), "hieulv");
    rig.until("closed", |r| {
        r.client.connections() == 0 && r.agent.connections() == 0
    });
    // New attempts stay refused while locked.
    rig.until("info disabled", |r| {
        r.client.info().is_some_and(|i| !i.enabled)
    });
}

#[test]
fn connection_count_is_capped() {
    let mut rig = Rig::new(echo());
    rig.ready();
    let socks: Vec<_> = (0..MAX_TUNNEL_CONNS + 3)
        .map(|_| TcpStream::connect((Ipv4Addr::LOCALHOST, rig.port())).unwrap())
        .collect();
    for _ in 0..300 {
        rig.step();
    }
    assert!(rig.agent.connections() <= MAX_TUNNEL_CONNS);
    assert!(rig.client.connections() <= MAX_TUNNEL_CONNS);
    drop(socks);
}

#[test]
fn the_controller_can_neither_choose_the_target_nor_exceed_the_window() {
    let target = echo();
    let mut agent = Mux::agent(target);
    agent.set_agent_policy(None, "u");
    // Open carries only an id: there is no field through which a destination could travel.
    agent
        .on_frame(&encode_tunnel(&TunnelFrame::Open { conn: 1 }).unwrap())
        .unwrap();
    assert_eq!(agent.connections(), 1);
    let chunk = vec![0u8; TUNNEL_CHUNK];
    let frame = encode_tunnel(&TunnelFrame::Data {
        conn: 1,
        data: &chunk,
    })
    .unwrap();
    let mut accepted = 0;
    while agent.on_frame(&frame).is_ok() {
        accepted += 1;
        assert!(accepted < 100, "window not enforced");
    }
    assert_eq!(accepted as u64 * TUNNEL_CHUNK as u64, TUNNEL_WINDOW);
    // Roles are not interchangeable.
    assert!(
        agent
            .on_frame(
                &encode_tunnel(&TunnelFrame::Info {
                    enabled: true,
                    user: "x".into()
                })
                .unwrap()
            )
            .is_err()
    );
    let mut client = Mux::client(0);
    assert!(
        client
            .on_frame(&encode_tunnel(&TunnelFrame::Open { conn: 1 }).unwrap())
            .is_err()
    );
    assert!(
        client
            .on_frame(&encode_tunnel(&TunnelFrame::Hello).unwrap())
            .is_err()
    );
}

#[test]
fn unreachable_target_is_reported_and_reset_clears_everything() {
    let dead = {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap()
    };
    let mut rig = Rig::new(dead);
    rig.ready();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, rig.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    rig.until("refused", |r| {
        !r.client.connections() > 0 || r.client.connections() == 0
    });
    for _ in 0..50 {
        rig.step();
    }
    assert!(
        rig.client
            .take_refusals()
            .contains(&RefuseReason::Unreachable)
    );
    let mut b = [0u8; 4];
    assert!(matches!(s.read(&mut b), Ok(0) | Err(_)));
    rig.client.reset();
    assert!(rig.client.info().is_none());
}
