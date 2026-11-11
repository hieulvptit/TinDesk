//! TCP tunnel frames carried on the `sync` data channel (frame tag 2).
//!
//! The destination is never part of the protocol: the agent alone decides where a tunnel
//! leads (its configured local sshd port), so a controller cannot steer it elsewhere.
//! Every connection has its own credit window in each direction, bounding queued data.
use crate::SYNC_FRAME_LIMIT;

pub const TUNNEL_TAG: u8 = 2;
pub const TUNNEL_CHUNK: usize = 16 * 1024;
/// Bytes a sender may have in flight, per connection and direction.
pub const TUNNEL_WINDOW: u64 = 256 * 1024;
/// A receiver grants credit back once this much has been written out.
pub const CREDIT_STEP: u64 = 64 * 1024;
pub const MAX_TUNNEL_CONNS: usize = 8;
pub const USER_NAME_LIMIT: usize = 64;

const HEADER: usize = 1 + 1 + 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RefuseReason {
    Disabled = 1,
    Locked = 2,
    TooMany = 3,
    Unreachable = 4,
    Invalid = 5,
}
impl RefuseReason {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Disabled,
            2 => Self::Locked,
            3 => Self::TooMany,
            4 => Self::Unreachable,
            _ => Self::Invalid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelFrame<'a> {
    /// Controller asks what the agent offers.
    Hello,
    /// Agent's answer; `user` is the account SSH will reach (validated, safe to display).
    Info {
        enabled: bool,
        user: String,
    },
    Open {
        conn: u32,
    },
    Accepted {
        conn: u32,
    },
    Refused {
        conn: u32,
        reason: RefuseReason,
    },
    Data {
        conn: u32,
        data: &'a [u8],
    },
    Credit {
        conn: u32,
        bytes: u32,
    },
    Close {
        conn: u32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TunnelError {
    #[error("malformed tunnel frame")]
    Malformed,
    #[error("tunnel data exceeds its window")]
    WindowExceeded,
    #[error("invalid account name")]
    InvalidUser,
}

fn head(kind: u8, conn: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + 16);
    out.push(TUNNEL_TAG);
    out.push(kind);
    out.extend_from_slice(&conn.to_be_bytes());
    out
}

/// POSIX-portable account names only: this string ends up in a command the user copies.
pub fn valid_user(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= USER_NAME_LIMIT
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

pub fn encode_tunnel(frame: &TunnelFrame<'_>) -> Result<Vec<u8>, TunnelError> {
    Ok(match frame {
        TunnelFrame::Hello => head(1, 0),
        TunnelFrame::Info { enabled, user } => {
            if !user.is_empty() && !valid_user(user) {
                return Err(TunnelError::InvalidUser);
            }
            let mut out = head(2, 0);
            out.push(*enabled as u8);
            out.extend_from_slice(user.as_bytes());
            out
        }
        TunnelFrame::Open { conn } => head(3, *conn),
        TunnelFrame::Accepted { conn } => head(4, *conn),
        TunnelFrame::Refused { conn, reason } => {
            let mut out = head(5, *conn);
            out.push(*reason as u8);
            out
        }
        TunnelFrame::Data { conn, data } => {
            if data.len() > TUNNEL_CHUNK || HEADER + data.len() > SYNC_FRAME_LIMIT {
                return Err(TunnelError::Malformed);
            }
            let mut out = head(6, *conn);
            out.extend_from_slice(data);
            out
        }
        TunnelFrame::Credit { conn, bytes } => {
            let mut out = head(7, *conn);
            out.extend_from_slice(&bytes.to_be_bytes());
            out
        }
        TunnelFrame::Close { conn } => head(8, *conn),
    })
}

pub fn is_tunnel_frame(bytes: &[u8]) -> bool {
    bytes.first() == Some(&TUNNEL_TAG)
}

pub fn decode_tunnel(bytes: &[u8]) -> Result<TunnelFrame<'_>, TunnelError> {
    if bytes.len() < HEADER || bytes[0] != TUNNEL_TAG || bytes.len() > SYNC_FRAME_LIMIT {
        return Err(TunnelError::Malformed);
    }
    let conn = u32::from_be_bytes(bytes[2..6].try_into().unwrap());
    let body = &bytes[HEADER..];
    Ok(match (bytes[1], body.len()) {
        (1, 0) => TunnelFrame::Hello,
        (2, n) if n >= 1 => {
            let user = std::str::from_utf8(&body[1..]).map_err(|_| TunnelError::Malformed)?;
            if !user.is_empty() && !valid_user(user) {
                return Err(TunnelError::InvalidUser);
            }
            TunnelFrame::Info {
                enabled: body[0] == 1,
                user: user.to_owned(),
            }
        }
        (3, 0) => TunnelFrame::Open { conn },
        (4, 0) => TunnelFrame::Accepted { conn },
        (5, 1) => TunnelFrame::Refused {
            conn,
            reason: RefuseReason::from_u8(body[0]),
        },
        (6, n) if n <= TUNNEL_CHUNK => TunnelFrame::Data { conn, data: body },
        (7, 4) => TunnelFrame::Credit {
            conn,
            bytes: u32::from_be_bytes(body.try_into().unwrap()),
        },
        (8, 0) => TunnelFrame::Close { conn },
        _ => return Err(TunnelError::Malformed),
    })
}

/// How much more this side may send on one connection.
#[derive(Debug, Clone, Copy)]
pub struct SendWindow {
    avail: u64,
}
impl Default for SendWindow {
    fn default() -> Self {
        Self::new()
    }
}
impl SendWindow {
    pub fn new() -> Self {
        Self {
            avail: TUNNEL_WINDOW,
        }
    }
    pub fn avail(&self) -> u64 {
        self.avail
    }
    pub fn consume(&mut self, n: usize) {
        self.avail = self.avail.saturating_sub(n as u64);
    }
    /// A peer cannot grant more than it was ever sent: that is a protocol violation.
    pub fn grant(&mut self, bytes: u32) -> Result<(), TunnelError> {
        let total = self.avail + bytes as u64;
        if total > TUNNEL_WINDOW {
            return Err(TunnelError::WindowExceeded);
        }
        self.avail = total;
        Ok(())
    }
}

/// Tracks received-but-not-yet-written bytes and decides when to return credit.
#[derive(Debug, Clone, Copy, Default)]
pub struct RecvFlow {
    held: u64,
    owed: u64,
}
impl RecvFlow {
    pub fn on_data(&mut self, n: usize) -> Result<(), TunnelError> {
        let held = self.held + n as u64;
        if held > TUNNEL_WINDOW {
            return Err(TunnelError::WindowExceeded);
        }
        self.held = held;
        Ok(())
    }
    /// Bytes were written to the local socket; returns credit to send, if enough accumulated.
    pub fn consumed(&mut self, n: usize) -> Option<u32> {
        self.held = self.held.saturating_sub(n as u64);
        self.owed += n as u64;
        if self.owed >= CREDIT_STEP {
            let grant = self.owed as u32;
            self.owed = 0;
            Some(grant)
        } else {
            None
        }
    }
}
