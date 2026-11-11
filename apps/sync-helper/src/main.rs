use binachicken_sync::purge_older_than;
use binachicken_sync_helper::{bridge, x11clip};
use std::{os::unix::net::UnixStream, path::PathBuf, thread, time::Duration};

fn staging_default() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("binachicken/clipboard-files")
}

fn main() {
    let mut socket = PathBuf::from("/run/binachicken-agent/sync.sock");
    let mut staging = staging_default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match (arg.as_str(), args.next()) {
            ("--socket", Some(v)) => socket = v.into(),
            ("--staging", Some(v)) => staging = v.into(),
            _ => {
                eprintln!("usage: binachicken-sync-helper [--socket PATH] [--staging DIR]");
                std::process::exit(2);
            }
        }
    }
    purge_older_than(&staging, Duration::from_secs(24 * 3600));
    let (clipboard, events) = match x11clip::start() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("binachicken-sync-helper: no usable X11 clipboard: {e}");
            std::process::exit(1);
        }
    };
    loop {
        // The agent only listens while a remote session is active; keep retrying quietly.
        if let Ok(stream) = UnixStream::connect(&socket)
            && let Err(e) = bridge::run_session(stream, clipboard.clone(), &events, &staging)
        {
            eprintln!("binachicken-sync-helper: session ended: {e}");
        }
        thread::sleep(Duration::from_secs(2));
    }
}
