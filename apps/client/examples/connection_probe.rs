//! Own-account connection diagnostic. Never injects user input or exports frame pixels.
use binachicken_client::{
    app::{ClientRuntime, Config, ViewState},
    video::LatestFrame,
};
use binachicken_protocol::DeviceRole;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn main() -> binachicken_client::Result<()> {
    #[cfg(target_os = "macos")]
    binachicken_client::media_runtime::initialize_bundled()?;
    let state = Arc::new(Mutex::new(ViewState::default()));
    let frames = LatestFrame::default();
    let mut config = Config::new("https://amio.vnoffice.io.vn")?;
    config.relay_only = std::env::args().any(|arg| arg == "--relay-only");
    let runtime = ClientRuntime::start(config, state.clone(), frames.clone())?;
    let until = Instant::now() + Duration::from_secs(35);
    let mut requested = false;
    let mut last = String::new();
    let mut stable_since = None;
    let mut stable_session = None;
    let mut stable_video = false;
    while Instant::now() < until && !runtime.is_finished() {
        {
            let s = state.lock().unwrap();
            let report = format!(
                "status={} ready={} connected={} epoch={} rx_bytes={:?} decoded_dimensions={:?}",
                s.status,
                s.ready,
                s.connected,
                s.epoch,
                s.stats.native.bytes_received,
                frames.metrics().0
            );
            if report != last {
                println!("{report}");
                last = report;
            }
            if s.enrollment.is_some() {
                return Err(
                    "Diagnostic needs an existing Google enrollment; use the normal app to sign in"
                        .into(),
                );
            }
            if !requested && s.signed_in {
                let agents: Vec<_> = s
                    .presence
                    .iter()
                    .filter(|p| p.online && p.role == DeviceRole::Agent)
                    .collect();
                if agents.len() > 1 {
                    return Err("Diagnostic requires exactly one online agent".into());
                }
                if let Some(agent) = agents.first() {
                    requested = runtime.connect(agent.device_id);
                }
            }
            let (dimensions, fps) = frames.metrics();
            if s.ready && s.connected && dimensions.is_some() && fps.is_some_and(|fps| fps > 0.) {
                if stable_session != s.session_id {
                    stable_since = None;
                    stable_session = s.session_id;
                }
                stable_video = stable_since.get_or_insert_with(Instant::now).elapsed()
                    >= Duration::from_secs(10);
            } else {
                stable_since = None;
                stable_video = false;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(runtime);
    if stable_video {
        println!("Live decoded video remained ready in one session for at least 10 seconds: PASS");
        Ok(())
    } else {
        Err("No live decoded video received during diagnostic window".into())
    }
}
