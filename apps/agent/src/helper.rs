//! Bounded local IPC. A separate shutdown descriptor interrupts even a partial 8 MiB frame.
use crate::Result;
use remotevnc_protocol::*;
use std::{
    io::Write,
    net::Shutdown,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio::sync::mpsc as async_mpsc;
use tokio_util::sync::CancellationToken;

pub struct Helper {
    pub events: async_mpsc::Receiver<HelperEvent>,
    pub video: async_mpsc::Receiver<EncodedFrame>,
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
                        // Never discard an encoded reference and continue its dependent deltas.
                        // Saturation closes the session; controller can establish a fresh one.
                        HelperOutput::Video(frame) => {
                            if video_tx.try_send(frame).is_err() {
                                break;
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
            failed,
            socket,
            commands,
        })
    }
    pub fn send(&self, request: HelperRequest) -> Result<()> {
        self.commands
            .try_send(request)
            .map_err(|_| "helper control queue unavailable".into())
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
