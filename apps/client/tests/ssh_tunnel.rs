use binachicken_client::clipboard::*;
use binachicken_sync::{Busy, Mux};
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    path::PathBuf,
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

struct Empty;
impl Pasteboard for Empty {
    fn change_count(&self) -> i64 {
        0
    }
    fn read(&self) -> Option<Content> {
        None
    }
    fn write_text(&mut self, _: &str) -> io::Result<()> {
        Ok(())
    }
    fn write_files(&mut self, _: &[PathBuf]) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Queues {
    to_agent: VecDeque<Vec<u8>>,
    to_client: VecDeque<Vec<u8>>,
}
struct Link<'a>(&'a Mutex<Queues>);
impl SyncLink for Link<'_> {
    fn send(&self, f: &[u8]) -> Result<(), LinkError> {
        self.0.lock().unwrap().to_agent.push_back(f.to_vec());
        Ok(())
    }
    fn recv(&self) -> Option<Vec<u8>> {
        self.0.lock().unwrap().to_client.pop_front()
    }
}

fn echo() -> std::net::SocketAddr {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = l.local_addr().unwrap();
    thread::spawn(move || {
        for s in l.incoming().flatten() {
            thread::spawn(move || {
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

#[test]
fn ssh_switch_opens_a_loopback_port_that_reaches_the_agent_target_and_closes_again() {
    let mut controller = SyncController::new(std::env::temp_dir().join("bc-ssh-stage"), Empty);
    controller.set_tag("abcd1234");
    let mut agent = Mux::agent(echo());
    agent.set_agent_policy(None, "hieulv");
    let queues = Mutex::new(Queues::default());
    let mut step = |controller: &mut SyncController<Empty>| {
        controller.tick(&Link(&queues)).ok().unwrap();
        let mut out = vec![];
        agent.poll(&mut |f| {
            out.push(f.to_vec());
            Ok::<(), Busy>(())
        });
        let mut q = queues.lock().unwrap();
        q.to_client.extend(out);
        while let Some(f) = q.to_agent.pop_front() {
            agent.on_frame(&f).unwrap();
        }
        drop(q);
        thread::sleep(Duration::from_millis(1));
    };

    // Off by default: nothing listens, no command is offered.
    for _ in 0..30 {
        step(&mut controller);
    }
    assert!(!ssh_wanted());
    assert_eq!(ssh_state().port, None);
    assert_eq!(ssh_state().command, None);

    set_ssh_wanted(true);
    let end = Instant::now() + Duration::from_secs(5);
    while ssh_state().command.is_none() {
        step(&mut controller);
        assert!(Instant::now() < end, "no command offered");
    }
    let state = ssh_state();
    let port = state.port.unwrap();
    assert_eq!(
        state.command.unwrap(),
        format!("ssh -p {port} -o HostKeyAlias=binachicken-abcd1234 hieulv@127.0.0.1")
    );

    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(1))).unwrap();
    s.write_all(b"SSH-2.0-test\r\n").unwrap();
    let mut got = vec![0u8; 14];
    let mut n = 0;
    let end = Instant::now() + Duration::from_secs(5);
    while n < 14 {
        step(&mut controller);
        if let Ok(k) = s.read(&mut got[n..]) {
            n += k;
        }
        assert!(Instant::now() < end, "no echo through the tunnel");
    }
    assert_eq!(&got, b"SSH-2.0-test\r\n");

    // Switching off closes the port and the live connection.
    set_ssh_wanted(false);
    for _ in 0..50 {
        step(&mut controller);
    }
    assert!(
        TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_err(),
        "port must be closed"
    );
    let mut b = [0u8; 4];
    assert!(matches!(s.read(&mut b), Ok(0) | Err(_)));
}
