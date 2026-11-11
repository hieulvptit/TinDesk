//! Clipboard and file sync for the controller. The platform pasteboard sits behind a
//! trait so scanning, echo suppression and the on/off switch are testable anywhere.
use binachicken_protocol::{Policy, RefuseReason, SYNC_FRAME_LIMIT, is_tunnel_frame};
use binachicken_sync::{Busy, Clipboard, Endpoint, Mux, Notice};
use std::{
    io,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Files(Vec<PathBuf>),
}

pub trait Pasteboard: Send {
    /// Increases whenever any application changes the clipboard.
    fn change_count(&self) -> i64;
    /// Current content; `None` for empty, unsupported, or marked-secret (password manager) data.
    fn read(&self) -> Option<Content>;
    fn write_text(&mut self, text: &str) -> io::Result<()>;
    fn write_files(&mut self, paths: &[PathBuf]) -> io::Result<()>;
}

#[derive(Debug)]
pub enum LinkError {
    Busy,
    Fatal,
}
/// The data channel from the session's point of view.
pub trait SyncLink {
    fn send(&self, frame: &[u8]) -> Result<(), LinkError>;
    fn recv(&self) -> Option<Vec<u8>>;
}

const SCAN_INTERVAL: Duration = Duration::from_millis(250);
/// Receive at most this many frames per tick so the UI loop is never starved.
const RECV_BATCH: usize = 64;

/// Shared by the Endpoint (writes from the remote) and the scanner (reads local changes).
pub struct Board<P: Pasteboard> {
    inner: Arc<Mutex<P>>,
    /// changeCount produced by our own writes; the scanner must not send those back.
    own: Arc<AtomicI64>,
}
impl<P: Pasteboard> Clipboard for Board<P> {
    fn set_text(&mut self, text: String) -> io::Result<()> {
        let mut board = self.inner.lock().unwrap();
        board.write_text(&text)?;
        self.own.store(board.change_count(), Ordering::SeqCst);
        Ok(())
    }
    fn set_files(&mut self, paths: &[PathBuf]) -> io::Result<()> {
        let mut board = self.inner.lock().unwrap();
        board.write_files(paths)?;
        self.own.store(board.change_count(), Ordering::SeqCst);
        Ok(())
    }
}

pub struct SyncController<P: Pasteboard> {
    endpoint: Endpoint<Board<P>>,
    board: Arc<Mutex<P>>,
    own: Arc<AtomicI64>,
    seen: i64,
    next_scan: Instant,
    staging: PathBuf,
    /// SSH tunnel: listens on loopback only while the user has switched it on.
    mux: Mux,
    ssh_on: bool,
    ssh_tag: String,
}
impl<P: Pasteboard + 'static> SyncController<P> {
    /// Whatever is on the clipboard right now was copied before this session and is not sent.
    pub fn new(staging: PathBuf, pasteboard: P) -> Self {
        let seen = pasteboard.change_count();
        let board = Arc::new(Mutex::new(pasteboard));
        let own = Arc::new(AtomicI64::new(i64::MIN));
        let endpoint = Endpoint::new(
            staging.clone(),
            Board {
                inner: board.clone(),
                own: own.clone(),
            },
            Policy {
                clipboard: enabled(),
                files: enabled(),
            },
        );
        Self {
            endpoint,
            board,
            own,
            seen,
            next_scan: Instant::now(),
            staging,
            mux: Mux::client(SSH_PREFERRED_PORT),
            ssh_on: false,
            ssh_tag: String::from("binachicken"),
        }
    }
    /// Distinguishes this remote computer's SSH host key from other things on the same local port.
    pub fn set_tag(&mut self, tag: &str) {
        self.ssh_tag = format!("binachicken-{tag}");
    }
    /// Fresh PeerConnection epoch: drop everything in flight.
    pub fn reset(&mut self) {
        self.endpoint.reset();
        self.mux.reset();
    }
    fn tick_ssh(&mut self, link: &dyn SyncLink) -> Result<(), LinkError> {
        let want = ssh_wanted();
        if want != self.ssh_on {
            self.ssh_on = want;
            if let Err(e) = self.mux.set_enabled(want) {
                self.ssh_on = false;
                set_ssh_wanted(false);
                set_ssh_message(&format!("Could not open a local port: {e}"));
            }
        }
        let mut fatal = false;
        self.mux.poll(&mut |frame| match link.send(frame) {
            Ok(()) => Ok(()),
            Err(LinkError::Busy) => Err(Busy),
            Err(LinkError::Fatal) => {
                fatal = true;
                Err(Busy)
            }
        });
        for reason in self.mux.take_refusals() {
            set_ssh_message(match reason {
                RefuseReason::Disabled => "The remote computer does not offer SSH right now",
                RefuseReason::Locked => "SSH is unavailable while the remote screen is locked",
                RefuseReason::TooMany => "Too many SSH connections are already open",
                RefuseReason::Unreachable => "The remote computer's SSH server did not answer",
                RefuseReason::Invalid => "The remote computer refused the SSH connection",
            });
        }
        let info = self.mux.info();
        let mut state = SSH_STATE.lock().unwrap();
        state.port = self.mux.local_port();
        state.remote_enabled = info.map(|i| i.enabled);
        state.command = match (state.port, info) {
            (Some(port), Some(i)) if i.enabled && !i.user.is_empty() => Some(format!(
                "ssh -p {port} -o HostKeyAlias={} {}@127.0.0.1",
                self.ssh_tag, i.user
            )),
            _ => None,
        };
        state.connections = self.mux.connections();
        if fatal { Err(LinkError::Fatal) } else { Ok(()) }
    }
    pub fn tick(&mut self, link: &dyn SyncLink) -> Result<(), LinkError> {
        let on = enabled();
        self.endpoint.set_policy(Policy {
            clipboard: on,
            files: on,
        });
        for _ in 0..RECV_BATCH {
            let Some(frame) = link.recv() else { break };
            if is_tunnel_frame(&frame) {
                if self.mux.on_frame(&frame).is_err() {
                    self.mux.reset();
                    set_ssh_message("SSH tunnel reset: invalid data from the remote computer");
                }
                continue;
            }
            if frame.len() > SYNC_FRAME_LIMIT || self.endpoint.on_frame(&frame).is_err() {
                // A misbehaving peer loses its transfers, not the whole session.
                self.endpoint.reset();
                set_status("Sync stopped: invalid data from the remote computer");
                break;
            }
        }
        if on && self.next_scan <= Instant::now() {
            self.next_scan = Instant::now() + SCAN_INTERVAL;
            self.scan();
        }
        let mut fatal = false;
        self.endpoint.poll(&mut |frame| match link.send(frame) {
            Ok(()) => Ok(()),
            Err(LinkError::Busy) => Err(Busy),
            Err(LinkError::Fatal) => {
                fatal = true;
                Err(Busy)
            }
        });
        for notice in self.endpoint.take_notices() {
            set_status(&describe(&notice));
        }
        let tunnel = self.tick_ssh(link);
        if fatal || tunnel.is_err() {
            Err(LinkError::Fatal)
        } else {
            Ok(())
        }
    }
    fn scan(&mut self) {
        let (count, content) = {
            let board = self.board.lock().unwrap();
            let count = board.change_count();
            if count == self.seen {
                return;
            }
            (count, board.read())
        };
        self.seen = count;
        if count == self.own.load(Ordering::SeqCst) {
            return;
        }
        match content {
            Some(Content::Text(text)) => {
                let queued = self.endpoint.outbound.queue_text(text);
                if queued.is_err() {
                    set_status("Clipboard text is too large to share");
                }
            }
            Some(Content::Files(paths)) => {
                // Files we staged for the remote's benefit are not re-sent.
                if paths.iter().any(|p| p.starts_with(&self.staging)) {
                    return;
                }
                let queued = self.endpoint.outbound.queue_files(paths);
                if queued.queued == 0 {
                    set_status("Nothing to send: folders and oversized files are not supported");
                } else if queued.skipped > 0 {
                    set_status(&format!(
                        "Sending {} file(s); {} skipped (folders or over 512 MiB)",
                        queued.queued, queued.skipped
                    ));
                }
            }
            None => {}
        }
    }
}

pub fn describe(notice: &Notice) -> String {
    use binachicken_protocol::RejectReason::*;
    match notice {
        Notice::ReceivedText => "Copied text from the remote computer".into(),
        Notice::ReceivedFiles(n) => format!("Received {n} file(s) from the remote computer"),
        Notice::SentText => "Sent clipboard text to the remote computer".into(),
        Notice::SentFiles(n) => format!("Sent {n} file(s) to the remote computer"),
        Notice::Refused(reason) => match reason {
            Disabled => "The remote computer cannot receive now (nobody signed in, screen locked, or sync is off)".into(),
            TooLarge => "The remote computer refused: too large".into(),
            TooMany => "The remote computer is busy with other transfers".into(),
            InvalidName => "The remote computer refused the file name".into(),
            InvalidState | IntegrityFailed | StorageFailed => {
                "The transfer failed on the remote computer".into()
            }
        },
        Notice::Failed(why) => format!("Transfer failed: {why}"),
    }
}

// --- user switch and status, shared between the UI and the session worker ---

static ENABLED: AtomicBool = AtomicBool::new(true);
static LOADED: AtomicBool = AtomicBool::new(false);
static STATUS: Mutex<Option<String>> = Mutex::new(None);

fn marker() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("HOME")?)
            .join("Library/Application Support/BinaChicken/clipboard-sync-off"),
    )
}
/// On by default; turning it off persists across launches.
pub fn enabled() -> bool {
    if !LOADED.swap(true, Ordering::SeqCst) {
        let off = marker().is_some_and(|m| m.exists());
        ENABLED.store(!off, Ordering::SeqCst);
    }
    ENABLED.load(Ordering::SeqCst)
}
pub fn set_enabled(on: bool) {
    LOADED.store(true, Ordering::SeqCst);
    ENABLED.store(on, Ordering::SeqCst);
    if let Some(marker) = marker() {
        if on {
            let _ = std::fs::remove_file(marker);
        } else if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir).and_then(|_| std::fs::write(marker, b""));
        }
    }
}
pub fn set_status(text: &str) {
    *STATUS.lock().unwrap() = Some(text.to_owned());
}
pub fn status() -> Option<String> {
    STATUS.lock().unwrap().clone()
}

// --- SSH tunnel switch and state. Off at every launch: it never persists. ---

const SSH_PREFERRED_PORT: u16 = 2222;
static SSH_WANTED: AtomicBool = AtomicBool::new(false);
static SSH_STATE: Mutex<SshState> = Mutex::new(SshState::new());

#[derive(Clone, Debug)]
pub struct SshState {
    pub port: Option<u16>,
    /// `None` until the remote answered; `Some(false)` when it does not offer SSH now.
    pub remote_enabled: Option<bool>,
    /// Ready-to-run command, only when the remote offers SSH and named its user.
    pub command: Option<String>,
    pub connections: usize,
    pub message: Option<String>,
}
impl SshState {
    const fn new() -> Self {
        Self {
            port: None,
            remote_enabled: None,
            command: None,
            connections: 0,
            message: None,
        }
    }
}
pub fn ssh_wanted() -> bool {
    SSH_WANTED.load(Ordering::SeqCst)
}
pub fn set_ssh_wanted(on: bool) {
    SSH_WANTED.store(on, Ordering::SeqCst);
    if !on {
        *SSH_STATE.lock().unwrap() = SshState::new();
    }
}
pub fn ssh_state() -> SshState {
    SSH_STATE.lock().unwrap().clone()
}
fn set_ssh_message(text: &str) {
    SSH_STATE.lock().unwrap().message = Some(text.to_owned());
}

pub fn staging_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library/Caches/BinaChicken/clipboard-files")
}

#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "macos")]
pub use mac::MacPasteboard;
