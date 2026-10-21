use crate::{Result, capture::Capture, error, input::X11Input, seat::DisplayTarget};
use remotevnc_protocol::*;
use std::{
    io::{ErrorKind, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::atomic::{AtomicBool, Ordering},
};

/// Check kernel traversal as the configured identity, including supplementary
/// groups and ACLs. Never alter an existing ancestor's ownership or mode.
#[cfg(target_os = "linux")]
pub fn check_socket_traversal(parent: &std::path::Path, uid: u32) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut record: libc::passwd = unsafe { std::mem::zeroed() };
    let mut storage = vec![0u8; 65536];
    let mut found = std::ptr::null_mut();
    if unsafe {
        libc::getpwuid_r(
            uid,
            &mut record,
            storage.as_mut_ptr().cast(),
            storage.len(),
            &mut found,
        )
    } != 0
        || found.is_null()
    {
        return Err(error(
            "configured agent UID has no resolvable system identity",
        ));
    }
    let mut groups = vec![0; 1024];
    let mut count = groups.len() as libc::c_int;
    if unsafe {
        libc::getgrouplist(
            record.pw_name,
            record.pw_gid,
            groups.as_mut_ptr(),
            &mut count,
        )
    } < 0
    {
        return Err(error("configured agent group list unavailable"));
    }
    groups.truncate(count as usize);
    let gid = record.pw_gid;
    // Keep the supplied lookup route: canonicalization alone hides inaccessible
    // directories before a symlink. Absolute lexical paths also handle relative
    // CLI paths without testing the empty ancestor of a relative Path.
    let supplied = std::path::absolute(parent)?;
    let canonical = supplied.canonicalize()?;
    let ancestors = supplied
        .ancestors()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .chain(canonical.ancestors().collect::<Vec<_>>().into_iter().rev());
    for ancestor in ancestors {
        let mut command = std::process::Command::new("/usr/bin/test");
        command.arg("-x").arg(ancestor);
        if unsafe { libc::geteuid() } != uid {
            let groups = groups.clone();
            unsafe {
                command.pre_exec(move || {
                    if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        if !command.status()?.success() {
            return Err(format!("agent UID {uid} cannot traverse {}; choose a socket directory accessible to that identity", ancestor.display()).into());
        }
    }
    Ok(())
}
#[cfg(not(target_os = "linux"))]
pub fn check_socket_traversal(_: &std::path::Path, _: u32) -> Result<()> {
    Err(error("helper requires Linux identity checks"))
}

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
/// Reliable events stay ordered; one latest motion can run between text scalars.
/// Both lanes share the existing envelope cap and are cleared together.
#[derive(Default)]
struct PendingInput {
    reliable: std::collections::VecDeque<InputEnvelope>,
    motion: Option<InputEnvelope>,
    motion_turn: bool,
}
impl PendingInput {
    fn clear(&mut self) {
        self.reliable.clear();
        self.motion = None;
        self.motion_turn = false;
    }
    fn push(&mut self, event: InputEnvelope) -> Result<()> {
        if let InputEvent::Text { text } = &event.event {
            for ch in text.chars() {
                crate::input::unicode_keysym(ch)?;
            }
        }
        if matches!(event.event, InputEvent::PointerMove { .. }) {
            if self.reliable.len() >= 64 {
                return Err(error("pending input queue full"));
            }
            self.motion = Some(event);
            return Ok(());
        }
        // Position-bearing reliable controls supersede older queued motion, just
        // as InputGuard's admission floor rejects late network motion. Their own
        // coordinates remain authoritative when the reliable event is delivered.
        if matches!(
            event.event,
            InputEvent::Button { .. } | InputEvent::Scroll { .. }
        ) && self
            .motion
            .as_ref()
            .is_some_and(|motion| motion.sequence <= event.sequence)
        {
            self.motion = None;
        }
        let bytes: usize = self
            .reliable
            .iter()
            .map(|e| match &e.event {
                InputEvent::Text { text } => text.len(),
                _ => 0,
            })
            .sum();
        let added = match &event.event {
            InputEvent::Text { text } => text.len(),
            _ => 0,
        };
        if self.reliable.len() + usize::from(self.motion.is_some()) >= 64
            || bytes + added > CONTROL_PAYLOAD_LIMIT
        {
            return Err(error("pending input queue full"));
        }
        self.reliable.push_back(event);
        Ok(())
    }
    fn next(&mut self) -> Option<InputEnvelope> {
        // Alternate when both lanes have work: an unending motion stream cannot
        // starve text/reliable input, and text cannot starve the latest pointer.
        if self.motion.is_some() && (self.motion_turn || self.reliable.is_empty()) {
            self.motion_turn = false;
            return self.motion.take();
        }
        let mut event = self.reliable.pop_front()?;
        self.motion_turn = true;
        if let InputEvent::Text { text } = &mut event.event
            && let Some(ch) = text.chars().next()
        {
            let rest = text.split_off(ch.len_utf8());
            if !rest.is_empty() {
                let mut remaining = event.clone();
                remaining.event = InputEvent::Text { text: rest };
                self.reliable.push_front(remaining);
            }
        }
        Some(event)
    }
}
#[derive(Default)]
pub struct SessionPolicy {
    session: Option<(SessionId, u64, InputGuard)>,
    acknowledged: bool,
    deadline_ms: Option<u64>,
}
impl SessionPolicy {
    pub fn start(&mut self, id: SessionId, display: DisplayInfo, now: u64) -> Result<()> {
        self.start_until(id, display, now, now.saturating_add(LEASE_DURATION_MS))
    }
    pub fn start_until(
        &mut self,
        id: SessionId,
        display: DisplayInfo,
        now: u64,
        deadline: u64,
    ) -> Result<()> {
        validate_deadline(now, deadline)?;
        if self.session.is_some() {
            return Err(error("control session already active"));
        }
        self.session = Some((id, display.generation, InputGuard::new(id, display, now)?));
        self.acknowledged = false;
        self.deadline_ms = Some(deadline);
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
        if guard.expired(now) || self.expired(now) {
            return Err(error("control lease expired"));
        }
        Ok(())
    }
    pub fn update_display(&mut self, display: DisplayInfo) -> Result<()> {
        let (_, generation, guard) = self.session.as_mut().ok_or("no session")?;
        guard.update_display(display)?;
        *generation = display.generation;
        self.acknowledged = false;
        Ok(())
    }
    pub fn accept(&mut self, input: &InputEnvelope, now: u64) -> Result<()> {
        self.validate(input.session_id, input.generation, now)?;
        if !self.acknowledged
            && !matches!(input.event, InputEvent::DisplayAck | InputEvent::ReleaseAll)
        {
            return Err(error("display acknowledgement required"));
        }
        let (_, _, guard) = self
            .session
            .as_mut()
            .ok_or_else(|| error("no active session"))?;
        guard.accept(input, now)?;
        if matches!(input.event, InputEvent::DisplayAck) {
            self.acknowledged = true;
        }
        Ok(())
    }
    pub fn renew(&mut self, id: SessionId, generation: u64, now: u64) -> Result<()> {
        self.renew_until(id, generation, now, now.saturating_add(LEASE_DURATION_MS))
    }
    pub fn renew_until(
        &mut self,
        id: SessionId,
        generation: u64,
        now: u64,
        deadline: u64,
    ) -> Result<()> {
        self.validate(id, generation, now)?;
        validate_deadline(now, deadline)?;
        if self.deadline_ms.is_some_and(|old| deadline < old) {
            return Err(error("lease deadline cannot go backwards"));
        }
        self.session.as_mut().unwrap().2.renew_lease(now);
        self.deadline_ms = Some(deadline);
        Ok(())
    }
    pub fn expired(&self, now: u64) -> bool {
        self.deadline_ms.is_some_and(|deadline| now >= deadline)
    }
    pub fn stop(&mut self) -> Option<SessionId> {
        self.deadline_ms = None;
        self.session.take().map(|v| v.0)
    }
}

fn validate_deadline(now: u64, deadline: u64) -> Result<()> {
    if deadline <= now || deadline - now > LEASE_DURATION_MS {
        return Err(error("invalid helper lease deadline"));
    }
    Ok(())
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
    serve_with_guardian(
        peer,
        monitor,
        generation,
        now_ms,
        stop,
        std::path::Path::new("/run/remotevnc-guardian/guardian.sock"),
    )
}
/// Trusted embedding/tests may select a local guardian endpoint. No network
/// request or agent IPC can select it.
pub fn serve_with_guardian(
    peer: &mut UnixStream,
    monitor: &mut impl FnMut(&dyn Fn() -> bool) -> Result<Option<DisplayTarget>>,
    generation: &mut u64,
    now_ms: impl Fn() -> u64,
    stop: &AtomicBool,
    guardian_socket: &std::path::Path,
) -> Result<()> {
    let output = Output::new(peer.try_clone()?)?;
    output.event(HelperEvent::Ready)?;
    // Reverse local drop order also stops capture before input recovery on
    // every early `?`/EOF path, not only explicit cancellation branches.
    let mut input: Option<X11Input> = None;
    let mut capture: Option<Capture> = None;
    let mut recovering_video = false;
    let mut pending_input = PendingInput::default();
    let mut last_scan = std::time::Instant::now() - std::time::Duration::from_secs(1);
    peer.set_read_timeout(Some(std::time::Duration::from_millis(5)))?;
    let mut target = None;
    let mut policy = SessionPolicy::default();
    let mut frames = FrameDecoder::default();
    loop {
        if let Some(input) = input.as_mut()
            && input.poll_recovery().is_err()
        {
            capture.take();
            return Ok(());
        }
        // Cancellation is checked before discovery and while loginctl is waiting.
        let cancelled = || {
            stop.load(Ordering::Relaxed)
                || output.failed.load(Ordering::Relaxed)
                || policy.expired(now_ms())
                || input
                    .as_ref()
                    .is_some_and(|input| !input.guardian_healthy())
                || disconnected(peer)
        };
        if cancelled() {
            capture.take();
            input.take();
            if let Some(session_id) = policy.stop() {
                output.event(HelperEvent::Stopped { session_id })?;
            }
            return Ok(());
        }
        let current = if last_scan.elapsed() >= std::time::Duration::from_millis(100) {
            last_scan = std::time::Instant::now();
            monitor(&cancelled).unwrap_or(None)
        } else {
            target.clone()
        };
        // Sample after discovery: its result never extends a control lease.
        let now = now_ms();
        if stop.load(Ordering::Relaxed) || policy.expired(now) || disconnected(peer) {
            capture.take();
            input.take();
            if let Some(session_id) = policy.stop() {
                output.event(HelperEvent::Stopped { session_id })?;
            }
            return Ok(());
        }
        if current != target {
            pending_input.clear();
            capture.take();
            input.take();
            if let Some(session_id) = policy.stop() {
                output.event(HelperEvent::Stopped { session_id })?;
            }
            *generation = generation.checked_add(1).ok_or("generation overflow")?;
            target = current;
            output.generation.store(*generation, Ordering::Release);
            if let Some(t) = &target {
                output.event(HelperEvent::DisplayChanged(info(t, *generation)))?;
            } else {
                output.event(HelperEvent::DisplayUnavailable {
                    generation: *generation,
                })?;
            }
        }
        let mut capture_stopped = false;
        if let Some(capture) = capture.as_mut() {
            loop {
                match capture.frames().try_recv() {
                    Ok(frame) => {
                        if recovering_video && !frame.keyframe {
                            continue;
                        }
                        if output.video.try_send(frame).is_err() {
                            recovering_video = true;
                            capture.request_keyframe()?;
                            break;
                        }
                        recovering_video = false;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        capture_stopped = true;
                        break;
                    }
                }
            }
        }
        if capture_stopped {
            if let Some(capture) = capture.as_mut() {
                let failure = capture.failure();
                eprintln!("remotevnc-seat-helper: {failure}");
                output.event(HelperEvent::CaptureFailed(failure))?;
            }
            pending_input.clear();
            capture.take();
            input.take();
            if let Some(session_id) = policy.stop() {
                output.event(HelperEvent::Stopped { session_id })?;
            }
            *generation = generation.checked_add(1).ok_or("generation overflow")?;
            output.generation.store(*generation, Ordering::Release);
            target =
                monitor(&|| stop.load(Ordering::Relaxed) || disconnected(peer)).unwrap_or(None);
            last_scan = std::time::Instant::now();
            if let Some(t) = &target {
                output.event(HelperEvent::DisplayChanged(info(t, *generation)))?;
            } else {
                output.event(HelperEvent::DisplayUnavailable {
                    generation: *generation,
                })?;
            }
            continue;
        }
        // One scalar (including its full restoration) or one ordinary event per
        // turn. No accepted input can extend authority; recheck at delivery.
        if let Some(event) = pending_input.next() {
            policy.validate(event.session_id, event.generation, now_ms())?;
            input
                .as_mut()
                .ok_or("no active X11 input")?
                .apply_with_cancel(&event.event, || {
                    stop.load(Ordering::Relaxed) || policy.expired(now_ms()) || disconnected(peer)
                })?;
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
                    HelperRequest::StartCapture {
                        session_id,
                        generation: requested,
                        profile,
                    } => {
                        policy.validate(session_id, requested, now)?;
                        if capture.is_some() {
                            return Err(error("capture already running"));
                        }
                        capture = Some(Capture::start(
                            target.as_ref().ok_or("no display")?,
                            profile,
                            *generation,
                        )?);
                    }
                    HelperRequest::RequestKeyframe {
                        session_id,
                        generation: requested,
                    } => {
                        policy.validate(session_id, requested, now)?;
                        capture.as_ref().ok_or("no capture")?.request_keyframe()?;
                    }
                    HelperRequest::UpdateProfile {
                        session_id,
                        generation: requested,
                        profile,
                    } => {
                        policy.validate(session_id, requested, now)?;
                        profile.validate()?;
                        capture
                            .as_mut()
                            .ok_or("no capture")?
                            .update_profile(profile)?;
                    }
                    HelperRequest::Start {
                        session_id,
                        display,
                        deadline_boot_ms,
                    } => {
                        let t = target.as_ref().ok_or("no physical display")?;
                        if display != info(t, *generation) {
                            return Err("Start display must match current helper display".into());
                        }
                        policy.start_until(session_id, display, now, deadline_boot_ms)?;
                        input = Some(X11Input::connect_guarded(
                            t,
                            guardian_socket,
                            deadline_boot_ms,
                        )?);
                    }
                    HelperRequest::Input(event) => {
                        policy.accept(&event, now)?;
                        if matches!(event.event, InputEvent::ReleaseAll) {
                            pending_input.clear();
                            input.as_mut().ok_or("no input")?.release_all()?;
                        } else {
                            pending_input.push(event)?;
                        }
                    }
                    HelperRequest::RenewLease {
                        session_id,
                        generation,
                        deadline_boot_ms,
                    } => {
                        policy.renew_until(session_id, generation, now, deadline_boot_ms)?;
                        input
                            .as_mut()
                            .ok_or("no input guardian")?
                            .renew_guardian(deadline_boot_ms)?;
                    }
                    HelperRequest::ReleaseAll {
                        session_id,
                        generation,
                    } => {
                        policy.validate(session_id, generation, now)?;
                        pending_input.clear();
                        input.as_mut().ok_or("no input")?.release_all()?;
                    }
                    HelperRequest::Stop {
                        session_id,
                        generation,
                    } => {
                        policy.validate(session_id, generation, now)?;
                        pending_input.clear();
                        capture.take();
                        input.take();
                        policy.stop();
                        output.event(HelperEvent::Stopped { session_id })?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                // Errors describe supported operations; submitted text is never formatted.
                output.event(HelperEvent::Error {
                    message: format!("helper request rejected: {error}; control session stopped"),
                })?;
                return Err("helper request rejected".into());
            }
        }
    }
}

/// Single writer prevents interleaving. Control has its own reserved queue;
/// capture admission never waits, and write timeout closes a stalled stream.
struct Output {
    control: std::sync::mpsc::SyncSender<HelperEvent>,
    video: std::sync::mpsc::SyncSender<EncodedFrame>,
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    failed: std::sync::Arc<AtomicBool>,
    stop: std::sync::Arc<AtomicBool>,
    interrupt: UnixStream,
    finished: std::sync::mpsc::Receiver<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Output {
    fn new(mut peer: UnixStream) -> Result<Self> {
        use std::{
            sync::{Arc, mpsc},
            time::Duration,
        };
        let (control, controls) = mpsc::sync_channel(32);
        let (video, frames) = mpsc::sync_channel::<EncodedFrame>(2);
        let generation = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let current = generation.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let failure = failed.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        peer.set_write_timeout(Some(Duration::from_millis(100)))?;
        let interrupt = peer.try_clone()?;
        let (done, finished) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            loop {
                let next = if let Ok(event) = controls.try_recv() {
                    Some(HelperOutput::Event(event))
                } else if stopping.load(Ordering::Acquire) {
                    break;
                } else {
                    frames
                        .try_recv()
                        .ok()
                        .filter(|f| f.generation == current.load(Ordering::Acquire))
                        .map(HelperOutput::Video)
                };
                if let Some(value) = next {
                    if write_helper_output(&mut peer, &value).is_err() {
                        failure.store(true, Ordering::Release);
                        let _ = peer.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            let _ = done.send(());
        });
        Ok(Self {
            interrupt,
            finished,
            control,
            video,
            generation,
            failed,
            stop,
            worker: Some(worker),
        })
    }
    fn event(&self, event: HelperEvent) -> Result<()> {
        self.control
            .try_send(event)
            .map_err(|_| error("helper control output full/disconnected"))
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Give queued terminal events a finite grace period. A per-syscall
        // timeout does not bound write_all when the reader keeps making progress.
        if self
            .finished
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err()
        {
            let _ = self.interrupt.shutdown(std::net::Shutdown::Write);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queued_text_is_scalar_bounded_ordered_and_cancellable() {
        let event = |event| InputEnvelope {
            version: WIRE_VERSION,
            session_id: SessionId::from_uuid(uuid::Uuid::nil()),
            generation: 1,
            sequence: 1,
            event,
        };
        let mut queue = PendingInput::default();
        queue
            .push(event(InputEvent::Text {
                text: "ế😀".into()
            }))
            .unwrap();
        for x in 0..1000 {
            queue
                .push(event(InputEvent::PointerMove { x, y: 0 }))
                .unwrap();
        }
        queue
            .push(event(InputEvent::Key {
                keysym: 65,
                pressed: true,
            }))
            .unwrap();
        assert_eq!(queue.reliable.len(), 2);
        assert!(queue.motion.is_some());
        assert!(matches!(queue.next().unwrap().event,InputEvent::Text{text} if text=="ế"));
        assert!(matches!(
            queue.next().unwrap().event,
            InputEvent::PointerMove { x: 999, .. }
        ));
        assert!(matches!(queue.next().unwrap().event,InputEvent::Text{text} if text=="😀"));
        assert!(matches!(
            queue.next().unwrap().event,
            InputEvent::Key { .. }
        ));
        queue
            .push(event(InputEvent::PointerMove { x: 1, y: 2 }))
            .unwrap();
        queue.clear();
        assert!(queue.next().is_none());
        for _ in 0..64 {
            queue
                .push(event(InputEvent::Key {
                    keysym: 65,
                    pressed: true,
                }))
                .unwrap();
        }
        assert!(
            queue
                .push(event(InputEvent::Key {
                    keysym: 65,
                    pressed: false
                }))
                .is_err()
        );
    }
    #[test]
    fn latest_motion_is_fair_bounded_and_respects_control_floor() {
        let event = |sequence, event| InputEnvelope {
            version: WIRE_VERSION,
            session_id: SessionId::from_uuid(uuid::Uuid::nil()),
            generation: 1,
            sequence,
            event,
        };
        let mut queue = PendingInput::default();
        queue
            .push(event(1, InputEvent::Text { text: "abc".into() }))
            .unwrap();
        for sequence in 2..5 {
            queue
                .push(event(sequence, InputEvent::PointerMove { x: 9, y: 8 }))
                .unwrap();
            assert!(matches!(
                queue.next().unwrap().event,
                InputEvent::Text { .. }
            ));
            assert!(matches!(
                queue.next().unwrap().event,
                InputEvent::PointerMove { .. }
            ));
        }
        queue
            .push(event(5, InputEvent::PointerMove { x: 1, y: 2 }))
            .unwrap();
        queue
            .push(event(
                6,
                InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                    x: 7,
                    y: 8,
                },
            ))
            .unwrap();
        assert!(
            queue.motion.is_none(),
            "older motion must not follow a positioned control"
        );
        assert!(matches!(
            queue.next().unwrap().event,
            InputEvent::Button { x: 7, y: 8, .. }
        ));
        assert!(queue.next().is_none());
        // A newer motion is independent; it cannot remove the reliable click.
        queue
            .push(event(
                7,
                InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: false,
                    x: 7,
                    y: 8,
                },
            ))
            .unwrap();
        queue
            .push(event(8, InputEvent::PointerMove { x: 9, y: 10 }))
            .unwrap();
        queue
            .push(event(
                7,
                InputEvent::Scroll {
                    delta_x: 0,
                    delta_y: 1,
                    x: 7,
                    y: 8,
                },
            ))
            .unwrap();
        assert_eq!(queue.motion.as_ref().unwrap().sequence, 8);
        queue.clear();
        queue
            .push(event(9, InputEvent::PointerMove { x: 0, y: 0 }))
            .unwrap();
        for sequence in 10..73 {
            queue
                .push(event(
                    sequence,
                    InputEvent::Key {
                        keysym: 65,
                        pressed: true,
                    },
                ))
                .unwrap();
        }
        assert!(
            queue
                .push(event(
                    73,
                    InputEvent::Key {
                        keysym: 65,
                        pressed: false
                    }
                ))
                .is_err()
        );
        // Replacing latest motion does not allocate another envelope.
        queue
            .push(event(74, InputEvent::PointerMove { x: 2, y: 3 }))
            .unwrap();
        assert_eq!(
            queue.reliable.len() + usize::from(queue.motion.is_some()),
            64
        );
    }
    #[test]
    fn stalled_video_writer_admission_and_shutdown_are_bounded() {
        use std::time::{Duration, Instant};
        let (peer, _stalled) = UnixStream::pair().unwrap();
        let output = Output::new(peer).unwrap();
        let started = Instant::now();
        for _ in 0..20 {
            let _ = output.video.try_send(EncodedFrame {
                generation: 0,
                pts_ns: 0,
                keyframe: true,
                data: vec![0; ENCODED_FRAME_LIMIT],
            });
        }
        output.event(HelperEvent::Ready).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !output.failed.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        drop(output);
        assert!(started.elapsed() < Duration::from_millis(500));
    }
    #[test]
    fn slow_draining_video_writer_drop_interrupts_active_message() {
        use std::{net::Shutdown, sync::mpsc, thread, time::Duration};
        let (peer, mut reader) = UnixStream::pair().unwrap();
        let size: libc::c_int = 8192;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let rescue = peer.try_clone().unwrap();
        let output = Output::new(peer).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let draining = thread::spawn(move || {
            let mut started = Some(started_tx);
            let mut bytes = [0; 4096];
            while let Ok(n) = reader.read(&mut bytes) {
                if n == 0 {
                    break;
                }
                if let Some(tx) = started.take() {
                    let _ = tx.send(());
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        output
            .video
            .try_send(EncodedFrame {
                generation: 0,
                pts_ns: 0,
                keyframe: true,
                data: vec![0; ENCODED_FRAME_LIMIT],
            })
            .unwrap();
        let started = started_rx.recv_timeout(Duration::from_secs(2));
        thread::sleep(Duration::from_millis(200));
        let was_active = !output.worker.as_ref().unwrap().is_finished();
        let (finished_tx, finished_rx) = mpsc::channel();
        let dropping = thread::spawn(move || {
            drop(output);
            let _ = finished_tx.send(());
        });
        let finished = finished_rx.recv_timeout(Duration::from_millis(500));
        // Always rescue the old implementation before asserting, so a RED run
        // cannot leave either the writer join or reader blocked indefinitely.
        let _ = rescue.shutdown(Shutdown::Both);
        dropping.join().unwrap();
        draining.join().unwrap();
        assert!(started.is_ok(), "reader never received the active frame");
        assert!(was_active, "fixture did not keep write_all active");
        assert!(
            finished.is_ok(),
            "Output::drop waited for a slow reader beyond its teardown bound"
        );
    }

    #[test]
    fn output_drop_delivers_queued_terminal_event_within_grace() {
        let (peer, mut reader) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let output = Output::new(peer).unwrap();
        let session_id = SessionId::from_uuid(uuid::Uuid::nil());
        output.event(HelperEvent::Stopped { session_id }).unwrap();
        drop(output);
        assert!(
            matches!(read_helper_output(&mut reader).unwrap(),HelperOutput::Event(HelperEvent::Stopped {session_id:id}) if id==session_id)
        );
    }
}
