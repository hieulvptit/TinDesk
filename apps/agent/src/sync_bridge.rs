//! Relays opaque sync frames between the data channel and the logged-in user's sync
//! helper. The helper (running as that user) validates and applies them; the agent
//! only decides *who may connect* and *when*: the active seat0 user, never while locked.
use binachicken_protocol::{
    RefuseReason, RejectReason, SyncFrame, SyncMessage, TunnelFrame, decode_frame, decode_tunnel,
    encode_sync_control, encode_tunnel, is_tunnel_frame, valid_user,
};
use binachicken_sync::{Busy, FrameReader, Mux, write_frame};
use std::{
    collections::VecDeque,
    io::{self, ErrorKind, Read, Write},
    net::SocketAddr,
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Frames waiting for the data channel; beyond this we stop reading the helper, so
/// socket backpressure (not memory) absorbs a slow link.
const TO_PEER_LIMIT: usize = 32;
const TO_HELPER_LIMIT: usize = 1024 * 1024;
const AUTH_RECHECK: Duration = Duration::from_secs(5);

pub enum PortError {
    /// Transport backlog is full; retry on a later poll.
    Busy,
    Fatal,
}
/// The agent's view of the data channel; implemented for `Peer` and for tests.
pub trait SyncPort {
    fn send(&self, frame: &[u8]) -> Result<(), PortError>;
    fn recv(&self) -> Option<Vec<u8>>;
}

struct Conn {
    stream: UnixStream,
    uid: u32,
    reader: FrameReader,
    to_peer: VecDeque<Vec<u8>>,
    to_helper: Vec<u8>,
}

/// Who SSH may reach right now: `(None, account)` when allowed, else the refusal reason.
pub type TunnelPolicy = Box<dyn Fn() -> (Option<RefuseReason>, String) + Send>;

struct Tunnel {
    mux: Mux,
    policy: TunnelPolicy,
    next_policy: Instant,
}

pub struct SyncBridge {
    /// None for a tunnel-only bridge (no clipboard helper socket).
    listener: Option<UnixListener>,
    path: Option<PathBuf>,
    conn: Option<Conn>,
    /// Some(uid) of the active, unlocked seat user; None denies everyone.
    authorize: Box<dyn Fn() -> Option<u32> + Send>,
    next_check: Instant,
    tunnel: Option<Tunnel>,
}

/// Next non-tunnel frame. Tunnel frames are consumed here; with no tunnel configured they are
/// answered "disabled" so the controller's UI says so instead of waiting.
fn next_frame(
    tunnel: &mut Option<Tunnel>,
    port: &dyn SyncPort,
) -> Result<Option<Vec<u8>>, io::Error> {
    while let Some(frame) = port.recv() {
        if !is_tunnel_frame(&frame) {
            return Ok(Some(frame));
        }
        match tunnel {
            Some(t) => {
                if t.mux.on_frame(&frame).is_err() {
                    // A misbehaving controller loses its tunnels, not the session.
                    t.mux.reset();
                }
            }
            None => {
                let reply = match decode_tunnel(&frame) {
                    Ok(TunnelFrame::Hello) => encode_tunnel(&TunnelFrame::Info {
                        enabled: false,
                        user: String::new(),
                    }),
                    Ok(TunnelFrame::Open { conn }) => encode_tunnel(&TunnelFrame::Refused {
                        conn,
                        reason: RefuseReason::Disabled,
                    }),
                    _ => continue,
                };
                if let Ok(reply) = reply
                    && let Err(PortError::Fatal) = port.send(&reply)
                {
                    return Err(io::Error::other("sync channel closed"));
                }
            }
        }
    }
    Ok(None)
}

impl SyncBridge {
    pub fn bind(path: &Path, authorize: Box<dyn Fn() -> Option<u32> + Send>) -> io::Result<Self> {
        // The parent is agent-owned (systemd RuntimeDirectory); a stale file is ours.
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        // Connecting is open to local users; SO_PEERCRED plus `authorize` is the gate.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: Some(listener),
            path: Some(path.to_path_buf()),
            conn: None,
            authorize,
            next_check: Instant::now(),
            tunnel: None,
        })
    }
    /// A bridge that only serves the SSH tunnel (no clipboard helper).
    pub fn tunnel_only() -> Self {
        Self {
            listener: None,
            path: None,
            conn: None,
            authorize: Box::new(|| None),
            next_check: Instant::now(),
            tunnel: None,
        }
    }
    /// Serve SSH tunnels to `target` (the agent's own choice, e.g. 127.0.0.1:22) under `policy`.
    pub fn enable_tunnel(&mut self, target: SocketAddr, policy: TunnelPolicy) {
        self.tunnel = Some(Tunnel {
            mux: Mux::agent(target),
            policy,
            next_policy: Instant::now(),
        });
    }
    pub fn connected(&self) -> bool {
        self.conn.is_some()
    }
    /// New PeerConnection epoch: abort in-flight transfers on both ends.
    pub fn reset(&mut self, port: &dyn SyncPort) {
        self.conn = None;
        while port.recv().is_some() {}
        if let Some(t) = self.tunnel.as_mut() {
            t.mux.reset();
        }
    }
    fn poll_tunnel(&mut self, port: &dyn SyncPort) -> io::Result<()> {
        let Some(t) = self.tunnel.as_mut() else {
            return Ok(());
        };
        if t.next_policy <= Instant::now() {
            t.next_policy = Instant::now() + AUTH_RECHECK;
            let (deny, user) = (t.policy)();
            t.mux.set_agent_policy(deny, &user);
        }
        let mut fatal = false;
        t.mux.poll(&mut |frame| match port.send(frame) {
            Ok(()) => Ok(()),
            Err(PortError::Busy) => Err(Busy),
            Err(PortError::Fatal) => {
                fatal = true;
                Err(Busy)
            }
        });
        if fatal {
            Err(io::Error::other("sync channel closed"))
        } else {
            Ok(())
        }
    }
    pub fn poll(&mut self, port: &dyn SyncPort) -> io::Result<()> {
        self.poll_tunnel(port)?;
        if self.listener.is_some() && self.conn.is_none() {
            self.accept();
        }
        if self.conn.is_none() {
            self.refuse_without_helper(port)?;
            return Ok(());
        }
        if self.next_check <= Instant::now() {
            self.next_check = Instant::now() + AUTH_RECHECK;
            let uid = self.conn.as_ref().map(|c| c.uid);
            if (self.authorize)() != uid {
                self.conn = None;
                return Ok(());
            }
        }
        if self.pump(port)?.is_err() {
            self.conn = None;
        }
        Ok(())
    }
    fn accept(&mut self) {
        let Some(listener) = &self.listener else {
            return;
        };
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        let Ok(uid) = peer_uid(&stream) else { return };
        if (self.authorize)() != Some(uid) || stream.set_nonblocking(true).is_err() {
            return;
        }
        self.next_check = Instant::now() + AUTH_RECHECK;
        self.conn = Some(Conn {
            stream,
            uid,
            reader: FrameReader::default(),
            to_peer: VecDeque::new(),
            to_helper: Vec::new(),
        });
    }
    /// Nobody can apply a transfer: say so instead of letting the sender wait out its timeout.
    fn refuse_without_helper(&mut self, port: &dyn SyncPort) -> io::Result<()> {
        while let Some(frame) = next_frame(&mut self.tunnel, port)? {
            if let Ok(SyncFrame::Control(SyncMessage::Offer { id, .. })) = decode_frame(&frame)
                && let Ok(reply) = encode_sync_control(&SyncMessage::Reject {
                    id,
                    reason: RejectReason::Disabled,
                })
                && let Err(PortError::Fatal) = port.send(&reply)
            {
                return Err(io::Error::other("sync channel closed"));
            }
        }
        Ok(())
    }
    /// Inner Err means the helper connection is bad (drop it); outer Err is fatal for the peer.
    fn pump(&mut self, port: &dyn SyncPort) -> io::Result<Result<(), ()>> {
        let conn = self.conn.as_mut().unwrap();
        // Helper -> data channel.
        if conn.to_peer.len() < TO_PEER_LIMIT {
            let mut buf = [0u8; 16 * 1024];
            match conn.stream.read(&mut buf) {
                Ok(0) => return Ok(Err(())),
                Ok(n) => match conn.reader.push(&buf[..n]) {
                    Ok(frames) => conn.to_peer.extend(frames),
                    Err(_) => return Ok(Err(())),
                },
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(_) => return Ok(Err(())),
            }
        }
        while let Some(frame) = conn.to_peer.front() {
            match port.send(frame) {
                Ok(()) => {
                    conn.to_peer.pop_front();
                }
                Err(PortError::Busy) => break,
                Err(PortError::Fatal) => return Err(io::Error::other("sync channel closed")),
            }
        }
        // Data channel -> helper.
        while conn.to_helper.len() < TO_HELPER_LIMIT {
            let Some(frame) = next_frame(&mut self.tunnel, port)? else {
                break;
            };
            if write_frame(&mut conn.to_helper, &frame).is_err() {
                return Ok(Err(()));
            }
        }
        while !conn.to_helper.is_empty() {
            match conn.stream.write(&conn.to_helper) {
                Ok(0) => return Ok(Err(())),
                Ok(n) => {
                    conn.to_helper.drain(..n);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    break;
                }
                Err(_) => return Ok(Err(())),
            }
        }
        Ok(Ok(()))
    }
}
impl Drop for SyncBridge {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(peer: &UnixStream) -> io::Result<u32> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&cred) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            std::os::fd::AsRawFd::as_raw_fd(peer),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}
#[cfg(not(target_os = "linux"))]
fn peer_uid(peer: &UnixStream) -> io::Result<u32> {
    // Development hosts only; the service itself targets Linux.
    let _ = peer;
    Ok(unsafe { libc::geteuid() })
}

/// Parse `loginctl show-session -p User -p LockedHint` output: `Some(uid)` only for an
/// unlocked session. Anything unexpected denies.
pub fn parse_session(text: &str) -> Option<u32> {
    let (mut uid, mut locked) = (None, None);
    for line in text.lines() {
        match line.split_once('=')? {
            ("User", v) => uid = v.trim().parse::<u32>().ok(),
            ("LockedHint", v) => locked = Some(v.trim() == "yes"),
            _ => {}
        }
    }
    if locked? { None } else { uid }
}

/// Active seat0 user via logintd; fails closed on any error.
pub fn active_seat_user() -> Option<u32> {
    use std::process::Command;
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("loginctl").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let id = run(&["show-seat", "seat0", "-p", "ActiveSession", "--value"])?;
    let id = id.trim();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    parse_session(&run(&[
        "show-session",
        id,
        "-p",
        "User",
        "-p",
        "LockedHint",
    ])?)
}

/// Who holds seat0 right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seat {
    Active { uid: u32, name: String },
    Locked,
    Absent,
}

/// Parse `loginctl show-session -p User -p Name -p LockedHint`. Anything unexpected is `Absent`.
pub fn parse_seat(text: &str) -> Seat {
    let (mut uid, mut name, mut locked) = (None, None, None);
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Seat::Absent;
        };
        match key {
            "User" => uid = value.trim().parse::<u32>().ok(),
            "Name" => name = Some(value.trim().to_owned()),
            "LockedHint" => locked = Some(value.trim() == "yes"),
            _ => {}
        }
    }
    match (uid, name, locked) {
        (Some(_), _, Some(true)) => Seat::Locked,
        (Some(uid), Some(name), Some(false)) => Seat::Active { uid, name },
        _ => Seat::Absent,
    }
}

pub fn seat_state() -> Seat {
    use std::process::Command;
    let run = |args: &[&str]| -> Option<String> {
        let out = Command::new("loginctl").args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let Some(id) = run(&["show-seat", "seat0", "-p", "ActiveSession", "--value"]) else {
        return Seat::Absent;
    };
    let id = id.trim();
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Seat::Absent;
    }
    run(&[
        "show-session",
        id,
        "-p",
        "User",
        "-p",
        "Name",
        "-p",
        "LockedHint",
    ])
    .map(|t| parse_seat(&t))
    .unwrap_or(Seat::Absent)
}

/// SSH is offered only to the active, unlocked seat0 user, and only under a name that is safe
/// to show in a command line.
pub fn tunnel_policy() -> (Option<RefuseReason>, String) {
    match seat_state() {
        Seat::Active { name, .. } if valid_user(&name) => (None, name),
        Seat::Locked => (Some(RefuseReason::Locked), String::new()),
        _ => (Some(RefuseReason::Disabled), String::new()),
    }
}
