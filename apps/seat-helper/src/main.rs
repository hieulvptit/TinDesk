use remotevnc_seat_helper::{
    Result,
    input::X11Input,
    ipc::{authorize_uid, peer_uid, serve},
    seat::SeatMonitor,
};
use std::{
    fs,
    io::ErrorKind,
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixListener,
    },
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("remotevnc-seat-helper: {error}");
        std::process::exit(1);
    }
}
fn run() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--input-guardian") {
        let mut args = std::env::args().skip(2);
        let socket = match args.next().as_deref() {
            None => PathBuf::from("/run/remotevnc-guardian/guardian.sock"),
            Some("--socket") => args.next().ok_or("missing guardian socket")?.into(),
            _ => return Err("usage: --input-guardian [--socket PATH]".into()),
        };
        if args.next().is_some() {
            return Err("unexpected guardian argument".into());
        }
        return remotevnc_seat_helper::recovery::run(&socket);
    }
    if std::env::args().nth(1).as_deref() == Some("--capture-worker") {
        return remotevnc_seat_helper::capture::worker_main();
    }
    let mut args = std::env::args().skip(1);
    let mut socket = PathBuf::from("/run/remotevnc/helper.sock");
    let mut agent_uid = None;
    let mut doctor = false;
    let mut output_selector = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "doctor" => doctor = true,
            "--output" => output_selector = Some(args.next().ok_or("missing --output value")?),
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
                    "usage: remotevnc-seat-helper [doctor] [--socket PATH] [--agent-uid UID] [--output RANDR_NAME]"
                        .into(),
                );
            }
        }
    }
    let mut monitor = SeatMonitor::with_output(output_selector);
    if doctor {
        match monitor.current()? {
            Some(target) => {
                let mut input = X11Input::connect(&target)?;
                input.release_all()?;
                println!("physical seat0 X11 available: {target:?}; XTest, XKB and XI2 ready");
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
    remotevnc_seat_helper::ipc::check_socket_traversal(parent, agent_uid)?;
    let listener = UnixListener::bind(&socket)?;
    let _cleanup = SocketCleanup(socket.clone());
    // Peer UID is checked as well. Agent can traverse the dedicated service directory.
    let path = std::ffi::CString::new(socket.as_os_str().as_encoded_bytes())?;
    if unsafe { libc::chown(path.as_ptr(), agent_uid, u32::MAX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    remotevnc_seat_helper::recovery::boot_time_ms()?;
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
                if serve(
                    &mut peer,
                    &mut |cancelled| monitor.current_with_cancel(cancelled),
                    &mut generation,
                    || remotevnc_seat_helper::recovery::boot_time_ms().unwrap_or(u64::MAX),
                    &stop,
                )
                .is_err()
                {
                    eprintln!(
                        "remotevnc-seat-helper: IPC session failed; capture stopped and input cleanup attempted"
                    );
                }
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
