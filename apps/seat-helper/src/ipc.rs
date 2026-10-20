use crate::{Result, error, input::X11Input, seat::DisplayTarget};
use remotevnc_protocol::*;
use std::{
    io::{ErrorKind, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::atomic::{AtomicBool, Ordering},
};

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

fn disconnected(peer: &UnixStream) -> bool {
    let mut byte = 0u8;
    unsafe {
        libc::recv(
            peer.as_raw_fd(),
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        ) == 0
    }
}
fn info(target: &DisplayTarget, generation: u64) -> DisplayInfo {
    DisplayInfo {
        generation,
        width: target.width,
        height: target.height,
    }
}
/// Serve one already-authorized peer. Discovery must honor cancellation while
/// waiting; the clock supplies monotonic milliseconds for the local lease.
/// The listener configures bounded read/write timeouts before entering this loop.
pub fn serve(
    peer: &mut UnixStream,
    monitor: &mut impl FnMut(&dyn Fn() -> bool) -> Result<Option<DisplayTarget>>,
    generation: &mut u64,
    now_ms: impl Fn() -> u64,
    stop: &AtomicBool,
) -> Result<()> {
    send_event(peer, &HelperEvent::Ready)?;
    let mut target = None;
    let mut input: Option<X11Input> = None;
    let mut policy = SessionPolicy::default();
    let mut frames = FrameDecoder::default();
    loop {
        // Cancellation is checked before discovery and while loginctl is waiting.
        let cancelled =
            || stop.load(Ordering::Relaxed) || policy.expired(now_ms()) || disconnected(peer);
        if cancelled() {
            input.take();
            if let Some(session_id) = policy.stop() {
                send_event(peer, &HelperEvent::Stopped { session_id })?;
            }
            return Ok(());
        }
        let current = monitor(&cancelled).unwrap_or(None);
        // Sample after discovery: its result never extends a control lease.
        let now = now_ms();
        if stop.load(Ordering::Relaxed) || policy.expired(now) || disconnected(peer) {
            input.take();
            if let Some(session_id) = policy.stop() {
                send_event(peer, &HelperEvent::Stopped { session_id })?;
            }
            return Ok(());
        }
        if current != target {
            input.take();
            if let Some(session_id) = policy.stop() {
                send_event(peer, &HelperEvent::Stopped { session_id })?;
            }
            *generation = generation.checked_add(1).ok_or("generation overflow")?;
            target = current;
            if let Some(t) = &target {
                send_event(peer, &HelperEvent::DisplayChanged(info(t, *generation)))?;
            } else {
                send_event(
                    peer,
                    &HelperEvent::DisplayUnavailable {
                        generation: *generation,
                    },
                )?;
            }
        }
        let mut bytes = [0u8; 4096];
        let count = match peer.read(&mut bytes) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        for frame in frames.push(&bytes[..count])? {
            let request: HelperRequest = decode_control(&frame)?;
            let now = now_ms();
            let result = (|| -> Result<()> {
                match request {
                    HelperRequest::Start {
                        session_id,
                        display,
                    } => {
                        let t = target.as_ref().ok_or("no physical display")?;
                        if display != info(t, *generation) {
                            return Err("Start display must match current helper display".into());
                        }
                        policy.start(session_id, display, now)?;
                        input = Some(X11Input::connect(t)?);
                    }
                    HelperRequest::Input(event) => {
                        policy.accept(&event, now)?;
                        input
                            .as_mut()
                            .ok_or("no active X11 input")?
                            .apply_with_cancel(&event.event, || {
                                stop.load(Ordering::Relaxed)
                                    || policy.expired(now_ms())
                                    || disconnected(peer)
                            })?;
                    }
                    HelperRequest::RenewLease {
                        session_id,
                        generation,
                    } => {
                        policy.renew(session_id, generation, now)?;
                    }
                    HelperRequest::ReleaseAll {
                        session_id,
                        generation,
                    } => {
                        policy.validate(session_id, generation, now)?;
                        input.as_mut().ok_or("no input")?.release_all()?;
                    }
                    HelperRequest::Stop {
                        session_id,
                        generation,
                    } => {
                        policy.validate(session_id, generation, now)?;
                        input.take();
                        policy.stop();
                        send_event(peer, &HelperEvent::Stopped { session_id })?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                // Errors describe supported operations; submitted text is never formatted.
                send_event(
                    peer,
                    &HelperEvent::Error {
                        message: format!(
                            "helper request rejected: {error}; control session stopped"
                        ),
                    },
                )?;
                return Err("helper request rejected".into());
            }
        }
    }
}
