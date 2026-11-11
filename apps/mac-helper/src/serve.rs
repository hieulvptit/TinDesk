//! One authorized agent connection. Mirrors the Linux seat helper's contract:
//! Ready, DisplayChanged, lease-bounded control, generation-tagged video.
use crate::{
    Result,
    capture::Capture,
    clock_ms,
    display::{DisplayTarget, KeepAwake},
    error,
    input::MacInput,
};
use binachicken_protocol::*;
use binachicken_seat_helper::ipc::{FrameDecoder, SessionPolicy};
use std::{
    io::{ErrorKind, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn info(target: &DisplayTarget, generation: u64) -> DisplayInfo {
    DisplayInfo {
        generation,
        width: target.width,
        height: target.height,
    }
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

/// Single writer so events and video never interleave; a stalled reader fails
/// the connection instead of blocking capture or the lease loop.
struct Output {
    control: mpsc::SyncSender<HelperEvent>,
    video: mpsc::SyncSender<EncodedFrame>,
    generation: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Output {
    fn new(mut peer: UnixStream) -> Result<Self> {
        let (control, controls) = mpsc::sync_channel(32);
        let (video, frames) = mpsc::sync_channel::<EncodedFrame>(FRAME_QUEUE_CAPACITY);
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let failed = Arc::new(AtomicBool::new(false));
        let failure = failed.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        peer.set_write_timeout(Some(Duration::from_millis(100)))?;
        let worker = thread::spawn(move || {
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
                match next {
                    Some(value) => {
                        if write_helper_output(&mut peer, &value).is_err() {
                            failure.store(true, Ordering::Release);
                            let _ = peer.shutdown(std::net::Shutdown::Both);
                            break;
                        }
                    }
                    None => thread::sleep(Duration::from_millis(1)),
                }
            }
        });
        Ok(Self {
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
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn serve(
    peer: &mut UnixStream,
    monitor: &mut impl FnMut() -> Option<DisplayTarget>,
    generation: &mut u64,
    stop: &AtomicBool,
) -> Result<()> {
    let output = Output::new(peer.try_clone()?)?;
    output.event(HelperEvent::Ready)?;
    // Locals drop in reverse order: capture stops before input is released.
    let mut awake: Option<KeepAwake> = None;
    let mut input: Option<MacInput> = None;
    let mut capture: Option<Capture> = None;
    let mut recovering_video = false;
    let mut last_scan = Instant::now() - Duration::from_secs(1);
    peer.set_read_timeout(Some(Duration::from_millis(5)))?;
    let mut target: Option<DisplayTarget> = None;
    let mut policy = SessionPolicy::default();
    let mut frames = FrameDecoder::default();
    loop {
        if stop.load(Ordering::Relaxed)
            || output.failed.load(Ordering::Relaxed)
            || policy.expired(clock_ms())
            || disconnected(peer)
        {
            capture.take();
            input.take();
            awake.take();
            if let Some(session_id) = policy.stop() {
                output.event(HelperEvent::Stopped { session_id })?;
            }
            return Ok(());
        }
        if last_scan.elapsed() >= Duration::from_millis(250) {
            last_scan = Instant::now();
            let current = monitor();
            if current != target {
                capture.take();
                input.take();
                awake.take();
                if let Some(session_id) = policy.stop() {
                    output.event(HelperEvent::Stopped { session_id })?;
                }
                *generation = generation.checked_add(1).ok_or("generation overflow")?;
                target = current;
                output.generation.store(*generation, Ordering::Release);
                match &target {
                    Some(t) => output.event(HelperEvent::DisplayChanged(info(t, *generation)))?,
                    None => output.event(HelperEvent::DisplayUnavailable {
                        generation: *generation,
                    })?,
                }
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
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        capture_stopped = true;
                        break;
                    }
                }
            }
        }
        if capture_stopped {
            if let Some(capture) = capture.as_mut() {
                let failure = capture.failure();
                eprintln!("binachicken-mac-helper: {failure}");
                output.event(HelperEvent::CaptureFailed(failure))?;
            }
            capture.take();
            input.take();
            awake.take();
            if let Some(session_id) = policy.stop() {
                output.event(HelperEvent::Stopped { session_id })?;
            }
            continue;
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
            let now = clock_ms();
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
                        let t = target.as_ref().ok_or("no display")?;
                        if display != info(t, *generation) {
                            return Err("Start display must match current helper display".into());
                        }
                        policy.start_until(session_id, display, now, deadline_boot_ms)?;
                        awake = Some(KeepAwake::new());
                        input = Some(MacInput::connect(t)?);
                    }
                    HelperRequest::Input(event) => {
                        policy.accept(&event, now)?;
                        let active = input.as_mut().ok_or("no input")?;
                        active.apply(&event.event)?;
                    }
                    HelperRequest::RenewLease {
                        session_id,
                        generation,
                        deadline_boot_ms,
                    } => {
                        policy.renew_until(session_id, generation, now, deadline_boot_ms)?;
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
                        capture.take();
                        input.take();
                        awake.take();
                        policy.stop();
                        output.event(HelperEvent::Stopped { session_id })?;
                    }
                }
                Ok(())
            })();
            if let Err(error) = result {
                output.event(HelperEvent::Error {
                    message: format!("helper request rejected: {error}; control session stopped"),
                })?;
                return Err("helper request rejected".into());
            }
        }
    }
}
