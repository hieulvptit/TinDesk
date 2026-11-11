//! Wires a controller Mux to an agent Mux in one process, to try the tunnel against a real sshd:
//! `cargo run -p binachicken-sync --example ssh_loop -- 22 <user> <seconds>` then `ssh -p <port> ...`.
use binachicken_sync::{Busy, Mux};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    time::{Duration, Instant},
};

fn main() {
    let mut args = std::env::args().skip(1);
    let sshd: u16 = args.next().unwrap().parse().unwrap();
    let user = args.next().unwrap();
    let seconds: u64 = args.next().unwrap().parse().unwrap();
    let mut client = Mux::client(0);
    let mut agent = Mux::agent(SocketAddr::from(([127, 0, 0, 1], sshd)));
    agent.set_agent_policy(None, &user);
    let port = client.set_enabled(true).unwrap().unwrap();
    println!("PORT={port}");
    let (mut to_agent, mut to_client): (VecDeque<Vec<u8>>, VecDeque<Vec<u8>>) = Default::default();
    let end = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < end {
        client.poll(&mut |f| {
            to_agent.push_back(f.to_vec());
            Ok::<(), Busy>(())
        });
        agent.poll(&mut |f| {
            to_client.push_back(f.to_vec());
            Ok::<(), Busy>(())
        });
        while let Some(f) = to_agent.pop_front() {
            agent.on_frame(&f).unwrap();
        }
        while let Some(f) = to_client.pop_front() {
            client.on_frame(&f).unwrap();
        }
        std::thread::sleep(Duration::from_micros(200));
    }
}
