//! Joins the X11 clipboard to the agent's unix socket through one sync `Endpoint`.
use crate::x11clip::{ClipEvent, X11Clipboard};
use binachicken_protocol::{CLIPBOARD_TEXT_LIMIT, Policy};
use binachicken_sync::{Busy, Clipboard, Endpoint, FrameReader, write_frame};
use std::{
    cell::RefCell,
    io::{self, Read},
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::{Receiver, TryRecvError},
    time::Duration,
};

/// Beyond this many unsent batches, newer local clipboard changes are dropped.
const MAX_PENDING_BATCHES: usize = 8;

/// Remembers the last text written from the remote so it is not sent straight back.
struct Guarded {
    inner: X11Clipboard,
    last: Rc<RefCell<Option<String>>>,
}
impl Clipboard for Guarded {
    fn set_text(&mut self, text: String) -> io::Result<()> {
        *self.last.borrow_mut() = Some(text.clone());
        self.inner.set_text(text)
    }
    fn set_files(&mut self, paths: &[PathBuf]) -> io::Result<()> {
        self.inner.set_files(paths)
    }
}

/// Runs until the agent disconnects or violates the protocol.
pub fn run_session(
    mut stream: UnixStream,
    clipboard: X11Clipboard,
    events: &Receiver<ClipEvent>,
    staging: &Path,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_millis(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    // Anything copied before this session existed was never offered to the remote.
    while events.try_recv().is_ok() {}
    let last = Rc::new(RefCell::new(None));
    let mut endpoint = Endpoint::new(
        staging.to_path_buf(),
        Guarded {
            inner: clipboard,
            last: last.clone(),
        },
        Policy {
            clipboard: true,
            files: true,
        },
    );
    let mut reader = FrameReader::default();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                for frame in reader.push(&buf[..n])? {
                    endpoint
                        .on_frame(&frame)
                        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
        // Only the newest local change matters; older ones in the same tick are stale.
        let mut latest = None;
        loop {
            match events.try_recv() {
                Ok(event) => latest = Some(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::other("X11 clipboard thread stopped"));
                }
            }
        }
        if let Some(event) = latest
            && endpoint.outbound.pending_batches() < MAX_PENDING_BATCHES
        {
            match event {
                ClipEvent::Text(text) => {
                    if last.borrow().as_deref() != Some(text.as_str())
                        && text.len() as u64 <= CLIPBOARD_TEXT_LIMIT
                    {
                        let _ = endpoint.outbound.queue_text(text);
                    }
                }
                ClipEvent::Files(paths) => {
                    // Files we staged ourselves came from the remote: never loop them back.
                    if !paths.iter().any(|p| p.starts_with(staging)) {
                        endpoint.outbound.queue_files(paths);
                    }
                }
            }
        }
        let mut failure = None;
        endpoint.poll(&mut |frame| match write_frame(&mut stream, frame) {
            Ok(()) => Ok(()),
            Err(e) => {
                failure = Some(e);
                Err(Busy)
            }
        });
        if let Some(e) = failure {
            return Err(e);
        }
        // Notices carry no content; they are dropped here (the Mac client shows them).
        let _ = endpoint.take_notices();
    }
}
