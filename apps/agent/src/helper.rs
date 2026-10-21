//! Bounded local IPC. A separate shutdown descriptor interrupts even a partial 8 MiB frame.
use crate::Result;
use remotevnc_protocol::*;
use std::{
    io::Write,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};
use tokio::sync::mpsc as async_mpsc;
use tokio_util::sync::CancellationToken;

/// Local queue metadata is never transmitted over IPC.
pub struct ReceivedVideo {
    pub frame: EncodedFrame,
    pub recovery_revision: u64,
}
#[derive(Default)]
struct VideoRecovery {
    generation: Option<u64>,
    recovering: bool,
    started: Option<Instant>,
    pending: bool,
    dropped: u64,
    revision: u64,
}
impl VideoRecovery {
    fn dropped(&mut self, keyframe: bool) {
        self.dropped = self.dropped.saturating_add(1);
        self.revision = self.revision.wrapping_add(1);
        self.pending |= !self.recovering || keyframe;
        self.recovering = true;
        self.started.get_or_insert_with(Instant::now);
    }
    fn rejected(&mut self, generation: u64, now: Instant) {
        if self.generation == Some(generation) {
            self.started = Some(self.started.map_or(now, |started| started.min(now)));
        }
    }
    fn health(&mut self, now: Instant) -> Result<(u64, bool)> {
        if self
            .started
            .is_some_and(|t| now.saturating_duration_since(t) >= Duration::from_secs(20))
        {
            return Err("encoded video recovery timed out".into());
        }
        Ok((self.dropped, std::mem::take(&mut self.pending)))
    }
}

pub struct Helper {
    pub events: async_mpsc::Receiver<HelperEvent>,
    pub video: async_mpsc::Receiver<ReceivedVideo>,
    recovery: Arc<Mutex<VideoRecovery>>,
    pub failed: CancellationToken,
    socket: Arc<UnixStream>,
    commands: mpsc::SyncSender<HelperRequest>,
}
impl Helper {
    pub fn connect(path: &Path) -> Result<Self> {
        let socket = Arc::new(UnixStream::connect(path)?);
        let mut reader = socket.try_clone()?;
        let mut writer = socket.try_clone()?;
        writer
            .set_write_timeout(Some(Duration::from_millis(100)))
            .map_err(|e| format!("helper write timeout setup failed: {e}"))?;
        let (events_tx, events) = async_mpsc::channel(32);
        let (video_tx, video) = async_mpsc::channel(2);
        let (commands, requests) = mpsc::sync_channel::<HelperRequest>(32);
        let failed = CancellationToken::new();
        let recovery = Arc::new(Mutex::new(VideoRecovery::default()));
        let video_recovery = recovery.clone();
        let failure = failed.clone();
        std::thread::Builder::new()
            .name("agent-helper-read".into())
            .spawn(move || {
                while let Ok(output) = read_helper_output(&mut reader) {
                    match output {
                        HelperOutput::Event(event) => {
                            if events_tx.try_send(event).is_err() {
                                break;
                            }
                        }
                        HelperOutput::Video(frame) => {
                            // Parsing validates size/metadata before ordinary stale-frame handling.
                            let mut recovery = video_recovery.lock().unwrap();
                            if recovery.generation != Some(frame.generation) {
                                continue;
                            }
                            let config = frame.is_config_keyframe();
                            if recovery.recovering && !config {
                                recovery.dropped = recovery.dropped.saturating_add(1);
                                continue;
                            }
                            let received = ReceivedVideo {
                                frame,
                                recovery_revision: recovery.revision,
                            };
                            match video_tx.try_send(received) {
                                Ok(()) if config => recovery.recovering = false,
                                Ok(()) => {}
                                Err(async_mpsc::error::TrySendError::Full(_)) => {
                                    recovery.dropped(config)
                                }
                                Err(async_mpsc::error::TrySendError::Closed(_)) => break,
                            }
                        }
                    }
                }
                failure.cancel();
            })?;
        let failure = failed.clone();
        std::thread::Builder::new()
            .name("agent-helper-write".into())
            .spawn(move || {
                while let Ok(request) = requests.recv() {
                    let result = (|| -> Result<()> {
                        let bytes = encode_control(&request)?;
                        writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
                        writer.write_all(&bytes)?;
                        Ok(())
                    })();
                    if result.is_err() {
                        failure.cancel();
                        break;
                    }
                }
            })?;
        Ok(Self {
            events,
            video,
            recovery,
            failed,
            socket,
            commands,
        })
    }
    pub fn send(&self, request: HelperRequest) -> Result<()> {
        let generation = match &request {
            HelperRequest::StartCapture { generation, .. } => Some(*generation),
            _ => None,
        };
        let mut recovery = self.recovery.lock().unwrap();
        self.commands
            .try_send(request)
            .map_err(|_| "helper control queue unavailable")?;
        if let Some(generation) = generation
            && recovery.generation != Some(generation)
        {
            recovery.generation = Some(generation);
            recovery.recovering = false;
            recovery.started = None;
            recovery.pending = false;
            recovery.revision = recovery.revision.wrapping_add(1);
        }
        Ok(())
    }
    pub fn retire_video(&self) {
        let mut recovery = self.recovery.lock().unwrap();
        recovery.generation = None;
        recovery.recovering = false;
        recovery.started = None;
        recovery.pending = false;
        recovery.revision = recovery.revision.wrapping_add(1);
    }
    /// One bounded notice per recovery attempt; the absolute deadline is not renewed.
    pub fn video_health(&self, now: Instant) -> Result<(u64, bool)> {
        self.recovery.lock().unwrap().health(now)
    }
    /// Downstream rejection shares the earliest budget with local encoded loss.
    pub(crate) fn video_rejected(&self, generation: u64, now: Instant) {
        self.recovery.lock().unwrap().rejected(generation, now);
    }
    pub fn video_accepted(&self, generation: u64, revision: u64) {
        let mut recovery = self.recovery.lock().unwrap();
        if recovery.generation == Some(generation)
            && recovery.revision == revision
            && !recovery.recovering
        {
            recovery.started = None;
            recovery.pending = false;
        }
    }
    pub fn disconnect(&self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
    pub(crate) fn shutdown_socket(&self) -> Arc<UnixStream> {
        self.socket.clone()
    }
}
impl Drop for Helper {
    fn drop(&mut self) {
        self.disconnect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn helper_and_transport_loss_share_the_earliest_generation_budget() {
        let mut state = VideoRecovery {
            generation: Some(7),
            ..Default::default()
        };
        let start = Instant::now();
        state.rejected(8, start);
        assert!(
            state.started.is_none(),
            "stale generation cannot start current recovery"
        );
        state.rejected(7, start - Duration::from_secs(5));
        state.dropped(false);
        state.rejected(7, start);
        assert_eq!(state.started, Some(start - Duration::from_secs(5)));
        assert!(state.health(start + Duration::from_secs(15)).is_err());
        state.started = None;
        state.dropped(false);
        let first_helper_drop = state.started.unwrap();
        state.rejected(7, first_helper_drop + Duration::from_secs(5));
        assert_eq!(state.started, Some(first_helper_drop));
    }
    #[test]
    fn repeated_pressure_cannot_extend_recovery_deadline() {
        let mut state = VideoRecovery::default();
        state.dropped(false);
        let started = state.started.unwrap();
        assert!(state.health(started).unwrap().1);
        state.dropped(false);
        assert!(!state.health(started).unwrap().1);
        state.dropped(true);
        assert!(
            state.health(started).unwrap().1,
            "lost recovery IDR requests another attempt"
        );
        assert_eq!(state.started, Some(started));
        assert!(state.health(started + Duration::from_secs(20)).is_err());
    }
}
