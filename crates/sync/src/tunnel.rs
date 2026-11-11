//! TCP tunnel multiplexer for the `sync` channel. The controller side listens on a loopback
//! port; the agent side connects to one fixed local address it chose itself (its sshd). Data
//! moves in windowed frames, so queues stay bounded however slow either network or socket is.
use crate::Busy;
use binachicken_protocol::*;
use std::{
    collections::{HashMap, VecDeque},
    io::{self, ErrorKind, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    time::Duration,
};

/// Frames generated but not yet accepted by the transport; caps per-poll memory.
const PENDING_LIMIT: usize = 16;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelInfo {
    pub enabled: bool,
    /// Account that SSH will reach; empty when nobody can be reached.
    pub user: String,
}

enum Side {
    Client {
        preferred_port: u16,
        listener: Option<TcpListener>,
        info: Option<TunnelInfo>,
    },
    Agent {
        target: SocketAddr,
        /// None = allowed; otherwise the reason new connections are refused.
        deny: Option<RefuseReason>,
        user: String,
    },
}

struct Conn {
    stream: TcpStream,
    open: bool,
    send: SendWindow,
    recv: RecvFlow,
    /// Received from the peer, waiting to be written to the local socket.
    out: VecDeque<u8>,
    /// We read EOF locally and told the peer (no more data from us).
    sent_close: bool,
    /// The peer told us it has no more data.
    got_close: bool,
    shut_write: bool,
}

pub struct Mux {
    side: Side,
    conns: HashMap<u32, Conn>,
    next_id: u32,
    pending: VecDeque<Vec<u8>>,
    refused: Vec<RefuseReason>,
}

fn nonblocking(stream: &TcpStream) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)
}

impl Mux {
    /// Controller side. Nothing listens until `set_enabled(true)`.
    pub fn client(preferred_port: u16) -> Self {
        let mut mux = Self::new(Side::Client {
            preferred_port,
            listener: None,
            info: None,
        });
        mux.push(&TunnelFrame::Hello);
        mux
    }
    /// Agent side: `target` is where every accepted tunnel leads, chosen by local configuration.
    pub fn agent(target: SocketAddr) -> Self {
        Self::new(Side::Agent {
            target,
            deny: Some(RefuseReason::Disabled),
            user: String::new(),
        })
    }
    fn new(side: Side) -> Self {
        Self {
            side,
            conns: HashMap::new(),
            next_id: 1,
            pending: VecDeque::new(),
            refused: Vec::new(),
        }
    }
    fn push(&mut self, frame: &TunnelFrame<'_>) {
        if let Ok(bytes) = encode_tunnel(frame) {
            self.pending.push_back(bytes);
        }
    }

    // ---- controller side ----
    /// Start or stop listening on 127.0.0.1. Returns the bound port when enabled.
    pub fn set_enabled(&mut self, on: bool) -> io::Result<Option<u16>> {
        if !on {
            self.close_all();
            if let Side::Client { listener, .. } = &mut self.side {
                *listener = None;
            }
            return Ok(None);
        }
        let Side::Client {
            preferred_port,
            listener,
            ..
        } = &mut self.side
        else {
            return Ok(None);
        };
        if listener.is_none() {
            let bound = TcpListener::bind((Ipv4Addr::LOCALHOST, *preferred_port))
                .or_else(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0)))?;
            bound.set_nonblocking(true)?;
            *listener = Some(bound);
        }
        Ok(self.local_port())
    }
    pub fn local_port(&self) -> Option<u16> {
        match &self.side {
            Side::Client {
                listener: Some(l), ..
            } => l.local_addr().ok().map(|a| a.port()),
            _ => None,
        }
    }
    pub fn info(&self) -> Option<&TunnelInfo> {
        match &self.side {
            Side::Client { info, .. } => info.as_ref(),
            _ => None,
        }
    }
    pub fn take_refusals(&mut self) -> Vec<RefuseReason> {
        std::mem::take(&mut self.refused)
    }
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    // ---- agent side ----
    /// Owner re-evaluates this regularly (seat user, lock state). Denying closes live tunnels.
    pub fn set_agent_policy(&mut self, deny: Option<RefuseReason>, user: &str) {
        let user = if valid_user(user) { user } else { "" };
        let changed = match &mut self.side {
            Side::Agent {
                deny: d, user: u, ..
            } => {
                let changed = *d != deny || u != user;
                *d = deny;
                *u = user.to_owned();
                changed
            }
            _ => false,
        };
        if deny.is_some() {
            self.close_all();
        }
        if changed {
            self.push_info();
        }
    }
    fn push_info(&mut self) {
        if let Side::Agent { deny, user, .. } = &self.side {
            let frame = TunnelFrame::Info {
                enabled: deny.is_none(),
                user: user.clone(),
            };
            self.push(&frame);
        }
    }

    // ---- both ----
    /// Drop every connection and tell the peer.
    pub fn close_all(&mut self) {
        let ids: Vec<u32> = self.conns.keys().copied().collect();
        for conn in ids {
            self.push(&TunnelFrame::Close { conn });
        }
        self.conns.clear();
    }
    /// New PeerConnection epoch: nothing survives, and a controller asks again.
    pub fn reset(&mut self) {
        self.conns.clear();
        self.pending.clear();
        if let Side::Client { info, .. } = &mut self.side {
            *info = None;
            self.push(&TunnelFrame::Hello);
        }
    }

    pub fn on_frame(&mut self, bytes: &[u8]) -> Result<(), TunnelError> {
        match decode_tunnel(bytes)? {
            TunnelFrame::Hello => {
                if matches!(self.side, Side::Client { .. }) {
                    return Err(TunnelError::Malformed);
                }
                self.push_info();
            }
            TunnelFrame::Info { enabled, user } => {
                let Side::Client { info, .. } = &mut self.side else {
                    return Err(TunnelError::Malformed);
                };
                *info = Some(TunnelInfo { enabled, user });
                if !enabled {
                    self.close_all();
                }
            }
            TunnelFrame::Open { conn } => self.open(conn)?,
            TunnelFrame::Accepted { conn } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.open = true;
                }
            }
            TunnelFrame::Refused { conn, reason } => {
                if self.conns.remove(&conn).is_some() {
                    self.refused.push(reason);
                }
            }
            TunnelFrame::Data { conn, data } => {
                // Late frames for a connection we already dropped are expected.
                let Some(c) = self.conns.get_mut(&conn) else {
                    return Ok(());
                };
                if !c.open || c.got_close {
                    return Err(TunnelError::Malformed);
                }
                c.recv.on_data(data.len())?;
                c.out.extend(data);
            }
            TunnelFrame::Credit { conn, bytes } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.send.grant(bytes)?;
                }
            }
            TunnelFrame::Close { conn } => {
                if let Some(c) = self.conns.get_mut(&conn) {
                    c.got_close = true;
                    if !c.open {
                        self.conns.remove(&conn);
                    }
                }
            }
        }
        Ok(())
    }

    fn open(&mut self, conn: u32) -> Result<(), TunnelError> {
        let Side::Agent { target, deny, .. } = &self.side else {
            return Err(TunnelError::Malformed);
        };
        let (target, deny) = (*target, *deny);
        let refuse = |mux: &mut Self, reason| mux.push(&TunnelFrame::Refused { conn, reason });
        if conn == 0 || self.conns.contains_key(&conn) {
            refuse(self, RefuseReason::Invalid);
        } else if let Some(reason) = deny {
            refuse(self, reason);
        } else if self.conns.len() >= MAX_TUNNEL_CONNS {
            refuse(self, RefuseReason::TooMany);
        } else {
            match TcpStream::connect_timeout(&target, CONNECT_TIMEOUT)
                .and_then(|s| nonblocking(&s).map(|_| s))
            {
                Ok(stream) => {
                    self.conns.insert(conn, Conn::new(stream, true));
                    self.push(&TunnelFrame::Accepted { conn });
                }
                Err(_) => refuse(self, RefuseReason::Unreachable),
            }
        }
        Ok(())
    }

    /// Move bytes between sockets and frames; call every few milliseconds.
    pub fn poll(&mut self, send: &mut dyn FnMut(&[u8]) -> Result<(), Busy>) {
        self.accept_local();
        let ids: Vec<u32> = self.conns.keys().copied().collect();
        for id in ids {
            self.pump(id);
        }
        while let Some(frame) = self.pending.front() {
            if send(frame).is_err() {
                return;
            }
            self.pending.pop_front();
        }
    }

    fn accept_local(&mut self) {
        let Side::Client {
            listener: Some(listener),
            info,
            ..
        } = &self.side
        else {
            return;
        };
        let usable = info.as_ref().is_some_and(|i| i.enabled);
        let mut accepted = Vec::new();
        while let Ok((stream, _)) = listener.accept() {
            accepted.push(stream);
        }
        for stream in accepted {
            // Not ready, disabled or full: refuse at once instead of leaving ssh hanging.
            if !usable || self.conns.len() >= MAX_TUNNEL_CONNS || nonblocking(&stream).is_err() {
                continue;
            }
            let conn = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            self.conns.insert(conn, Conn::new(stream, false));
            self.push(&TunnelFrame::Open { conn });
        }
    }

    fn pump(&mut self, id: u32) {
        let room = self.pending.len() < PENDING_LIMIT;
        let Some(c) = self.conns.get_mut(&id) else {
            return;
        };
        let mut frames: Vec<TunnelFrame<'static>> = Vec::new();
        let mut data: Option<Vec<u8>> = None;
        let mut dead = false;
        // Peer data -> local socket.
        while !c.out.is_empty() {
            let (chunk, _) = c.out.as_slices();
            match c.stream.write(chunk) {
                Ok(0) => {
                    dead = true;
                    break;
                }
                Ok(n) => {
                    c.out.drain(..n);
                    if let Some(bytes) = c.recv.consumed(n) {
                        frames.push(TunnelFrame::Credit { conn: id, bytes });
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    break;
                }
                Err(_) => {
                    dead = true;
                    break;
                }
            }
        }
        if c.got_close && c.out.is_empty() && !c.shut_write && !dead {
            let _ = c.stream.shutdown(Shutdown::Write);
            c.shut_write = true;
        }
        // Local socket -> peer, only within the window.
        if c.open && !c.sent_close && !dead && room && c.send.avail() > 0 {
            let want = (c.send.avail() as usize).min(TUNNEL_CHUNK);
            let mut buf = vec![0u8; want];
            match c.stream.read(&mut buf) {
                Ok(0) => {
                    c.sent_close = true;
                    frames.push(TunnelFrame::Close { conn: id });
                }
                Ok(n) => {
                    buf.truncate(n);
                    c.send.consume(n);
                    data = Some(buf);
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(_) => dead = true,
            }
        }
        let finished = c.sent_close && c.got_close && c.out.is_empty();
        if dead && !c.sent_close {
            frames.push(TunnelFrame::Close { conn: id });
        }
        let remove = dead || finished;
        if let Some(buf) = data
            && let Ok(bytes) = encode_tunnel(&TunnelFrame::Data {
                conn: id,
                data: &buf,
            })
        {
            self.pending.push_back(bytes);
        }
        for frame in &frames {
            self.push(frame);
        }
        if remove {
            self.conns.remove(&id);
        }
    }
}

impl Conn {
    fn new(stream: TcpStream, open: bool) -> Self {
        Self {
            stream,
            open,
            send: SendWindow::new(),
            recv: RecvFlow::default(),
            out: VecDeque::new(),
            sent_close: false,
            got_close: false,
            shut_write: false,
        }
    }
}
