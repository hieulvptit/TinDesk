#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("binachicken-mac-helper only runs on macOS");
    std::process::exit(1);
}
#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = host::run() {
        eprintln!("binachicken-mac-helper: {error}");
        std::process::exit(1);
    }
}
#[cfg(target_os = "macos")]
mod host {
    use binachicken_mac_helper::{Result, display, serve::serve};
    use std::{
        fs,
        io::ErrorKind,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::Duration,
    };

    const USAGE: &str = "usage: binachicken-mac-helper [doctor] [--socket PATH]";
    pub fn run() -> Result<()> {
        let mut socket = default_socket();
        let mut doctor = false;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "doctor" => doctor = true,
                "--socket" => socket = args.next().ok_or("missing --socket value")?.into(),
                _ => return Err(USAGE.into()),
            }
        }
        if doctor {
            return doctor_report();
        }
        // Missing permissions are requested at startup; the prompts only appear
        // while they are still ungranted.
        display::request_permissions();
        let stop = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
        let parent = socket
            .parent()
            .ok_or("socket requires a parent directory")?;
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        // A previous run may have left its endpoint behind.
        let _ = fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        let _cleanup = Cleanup(socket.clone());
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let me = unsafe { libc::geteuid() };
        let mut generation = 0u64;
        while !stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut peer, _)) => {
                    if peer_uid(&peer) != Some(me) {
                        continue;
                    }
                    peer.set_nonblocking(false)?;
                    if serve(
                        &mut peer,
                        &mut display::main_display,
                        &mut generation,
                        &stop,
                    )
                    .is_err()
                    {
                        eprintln!(
                            "binachicken-mac-helper: IPC session failed; capture stopped and input released"
                        );
                    }
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn peer_uid(peer: &std::os::unix::net::UnixStream) -> Option<u32> {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0u32, 0u32);
        (unsafe { libc::getpeereid(peer.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
    }
    fn default_socket() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join("Library/Application Support/BinaChicken/helper.sock")
    }
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    fn doctor_report() -> Result<()> {
        let display = display::main_display();
        match &display {
            Some(d) => println!(
                "main display: {}x{} px ({}x{} pt)",
                d.width, d.height, d.points_w, d.points_h
            ),
            None => println!("main display: none active"),
        }
        let capture = display::capture_allowed();
        let input = display::input_trusted();
        println!(
            "Screen Recording permission: {}",
            if capture { "granted" } else { "MISSING" }
        );
        println!(
            "Accessibility permission: {}",
            if input { "granted" } else { "MISSING" }
        );
        if !(capture && input) {
            // Shows the system prompts so the user only has to click Allow.
            display::request_permissions();
        }
        if display.is_none() || !capture || !input {
            return Err("host is not ready; grant the missing permissions in System Settings > Privacy & Security".into());
        }
        println!("ready");
        Ok(())
    }
}
