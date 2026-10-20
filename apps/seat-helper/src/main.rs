use remotevnc_protocol::*;
use remotevnc_seat_helper::{
    Result,
    input::X11Input,
    ipc::{FrameDecoder, SessionPolicy, authorize_uid, peer_uid, send_event},
    seat::{DisplayTarget, SeatMonitor},
};
use std::{
    fs,
    io::{ErrorKind, Read},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn main() {
    if let Err(error) = run() {
        eprintln!("remotevnc-seat-helper: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut socket = PathBuf::from("/run/remotevnc/helper.sock");
    let mut agent_uid = None;
    let mut doctor = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "doctor" => doctor = true,
            "--socket" => socket = args.next().ok_or("missing --socket value")?.into(),
            "--agent-uid" => {
                agent_uid = Some(
                    args.next()
                        .ok_or("missing --agent-uid value")?
                        .parse::<u32>()?,
                )
            }
            _ => {
                return Err(
                    "usage: remotevnc-seat-helper [doctor] [--socket PATH] [--agent-uid UID]"
                        .into(),
                );
            }
        }
    }
    let mut monitor = SeatMonitor::default();
    if doctor {
        match monitor.current()? {
            Some(target) => {
                let mut input = X11Input::connect(&target)?;
                input.release_all()?;
                println!("physical seat0 X11 available: {target:?}; XTest and XKB ready");
            }
            None => return Err("no active local physical seat0 X11 session".into()),
        }
        return Ok(());
    }
    let agent_uid = agent_uid.ok_or("--agent-uid is required")?;
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    let parent = socket
        .parent()
        .ok_or("socket requires a parent directory")?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o711))?;
    }
    let metadata = fs::metadata(parent)?;
    // A writable directory would permit replacing the helper endpoint.
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(
            "socket parent must be owned by helper UID and not group/world writable".into(),
        );
    }
    let listener = UnixListener::bind(&socket)?;
    let _cleanup = SocketCleanup(socket.clone());
    // Peer UID is checked as well. Agent can traverse the dedicated service directory.
    let path = std::ffi::CString::new(socket.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::chown(path.as_ptr(), agent_uid, u32::MAX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    let start = Instant::now();
    let mut generation = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((mut peer, _)) => {
                if authorize_uid(peer_uid(&peer)?, agent_uid).is_err() {
                    continue;
                }
                peer.set_read_timeout(Some(Duration::from_millis(100)))?;
                peer.set_write_timeout(Some(Duration::from_millis(100)))?;
                // Errors/disconnects drop the X11 input owner and release its tracked state.
                let _ = serve(&mut peer, &mut monitor, &mut generation, start, &stop);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
struct SocketCleanup(PathBuf);
impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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
fn info(target: &DisplayTarget, generation: u64) -> DisplayInfo {
    DisplayInfo {
        generation,
        width: target.width,
        height: target.height,
    }
}
fn serve(
    peer: &mut UnixStream,
    monitor: &mut SeatMonitor,
    generation: &mut u64,
    start: Instant,
    stop: &AtomicBool,
) -> Result<()> {
    send_event(peer, &HelperEvent::Ready)?;
    let mut target = None;
    let mut input: Option<X11Input> = None;
    let mut policy = SessionPolicy::default();
    let mut frames = FrameDecoder::default();
    let mut last_renewal = Instant::now();
    loop {
        let now = start.elapsed().as_millis() as u64;
        let current = monitor.current().unwrap_or(None);
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
        if stop.load(Ordering::Relaxed) || policy.expired(now) {
            input.take();
            if let Some(session_id) = policy.stop() {
                send_event(peer, &HelperEvent::Stopped { session_id })?;
            }
            return Ok(());
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
            let now = start.elapsed().as_millis() as u64;
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
                        last_renewal = Instant::now();
                    }
                    HelperRequest::Input(event) => {
                        policy.accept(&event, now)?;
                        input
                            .as_mut()
                            .ok_or("no active X11 input")?
                            .apply_with_cancel(&event.event, || {
                                stop.load(Ordering::Relaxed)
                                    || last_renewal.elapsed()
                                        >= Duration::from_millis(LEASE_DURATION_MS)
                                    || disconnected(peer)
                            })?;
                    }
                    HelperRequest::RenewLease {
                        session_id,
                        generation,
                    } => {
                        policy.renew(session_id, generation, now)?;
                        last_renewal = Instant::now();
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
