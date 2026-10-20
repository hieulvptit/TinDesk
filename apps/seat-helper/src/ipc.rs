use crate::{Result, error};
use remotevnc_protocol::*;
use std::io::Write;

pub fn authorize_uid(actual: u32, allowed: u32) -> Result<()> {
    if actual != allowed {
        return Err(error("unauthorized Unix peer UID"));
    }
    Ok(())
}
#[derive(Default)]
pub struct FrameDecoder {
    pending: Vec<u8>,
    length: Option<usize>,
}
impl FrameDecoder {
    /// Caller reads bounded chunks. Never allocates from an unvalidated wire length.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        for byte in bytes {
            self.pending.push(*byte);
            if self.length.is_none() && self.pending.len() == 4 {
                let n = u32::from_be_bytes(self.pending[..].try_into()?) as usize;
                if n == 0 || n > CONTROL_PAYLOAD_LIMIT {
                    return Err(error("invalid IPC frame length"));
                }
                self.pending.clear();
                self.length = Some(n);
            } else if self.length == Some(self.pending.len()) {
                frames.push(std::mem::take(&mut self.pending));
                self.length = None;
            }
        }
        Ok(frames)
    }
}
pub fn send_event(writer: &mut impl Write, event: &HelperEvent) -> Result<()> {
    let data = encode_control(event)?;
    writer.write_all(&(data.len() as u32).to_be_bytes())?;
    writer.write_all(&data)?;
    Ok(())
}
#[derive(Default)]
pub struct SessionPolicy {
    session: Option<(SessionId, u64, InputGuard)>,
}
impl SessionPolicy {
    pub fn start(&mut self, id: SessionId, display: DisplayInfo, now: u64) -> Result<()> {
        if self.session.is_some() {
            return Err(error("control session already active"));
        }
        self.session = Some((id, display.generation, InputGuard::new(id, display, now)?));
        Ok(())
    }
    pub fn validate(&self, id: SessionId, generation: u64, now: u64) -> Result<()> {
        let (session, current, guard) = self
            .session
            .as_ref()
            .ok_or_else(|| error("no active control session"))?;
        if *session != id || *current != generation {
            return Err(error("wrong session or display generation"));
        }
        if guard.expired(now) {
            return Err(error("control lease expired"));
        }
        Ok(())
    }
    pub fn accept(&mut self, input: &InputEnvelope, now: u64) -> Result<()> {
        let (_, _, guard) = self
            .session
            .as_mut()
            .ok_or_else(|| error("no active session"))?;
        guard.accept(input, now)?;
        Ok(())
    }
    pub fn renew(&mut self, id: SessionId, generation: u64, now: u64) -> Result<()> {
        self.validate(id, generation, now)?;
        self.session.as_mut().unwrap().2.renew_lease(now);
        Ok(())
    }
    pub fn expired(&self, now: u64) -> bool {
        self.session
            .as_ref()
            .is_some_and(|(_, _, g)| g.expired(now))
    }
    pub fn stop(&mut self) -> Option<SessionId> {
        self.session.take().map(|v| v.0)
    }
}

#[cfg(target_os = "linux")]
pub fn peer_uid(peer: &std::os::unix::net::UnixStream) -> Result<u32> {
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
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(cred.uid)
}
#[cfg(not(target_os = "linux"))]
pub fn peer_uid(_peer: &std::os::unix::net::UnixStream) -> Result<u32> {
    Err("helper service requires Linux SO_PEERCRED".into())
}
