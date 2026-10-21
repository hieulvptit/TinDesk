//! Capture runs in a killable child: Xauthority is per-child and no encoder or
//! pipe write can hold the parent's input/lease loop. The worker has no network.
use crate::{Result, error, seat::DisplayTarget};
#[cfg(any(target_os = "linux", test))]
use gst::prelude::*;
use remotevnc_protocol::*;
use std::{
    io::{Read, Write},
    os::fd::AsRawFd,
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, Ordering},
        mpsc::Receiver,
    },
    thread::{self, JoinHandle},
};

#[derive(serde::Serialize, serde::Deserialize)]
struct WorkerConfig {
    target: DisplayTarget,
    profile: VideoProfile,
    generation: u64,
}
struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
pub struct Capture {
    child: Worker,
    commands: Arc<Mutex<ChildStdin>>,
    frames: Receiver<EncodedFrame>,
    reader: Option<JoinHandle<()>>,
    encoder: Arc<AtomicU8>,
    profile: VideoProfile,
}
impl Capture {
    /// Spawn returns promptly; worker initialization failure disconnects frames.
    pub fn start(target: &DisplayTarget, profile: VideoProfile, generation: u64) -> Result<Self> {
        Self::start_with_worker(&std::env::current_exe()?, target, profile, generation)
    }
    /// Trusted embedding/test callers supply the installed helper executable.
    /// Never accept this executable path from an IPC client.
    pub fn start_with_worker(
        worker: &Path,
        target: &DisplayTarget,
        profile: VideoProfile,
        generation: u64,
    ) -> Result<Self> {
        profile.validate()?;
        validate_target(target)?;
        let config = serde_json::to_vec(&WorkerConfig {
            target: target.clone(),
            profile,
            generation,
        })?;
        if config.len() + 4 > 4096 {
            return Err(error("capture configuration too large"));
        }
        let mut child = Worker(
            Command::new(worker)
                .arg("--capture-worker")
                .env("DISPLAY", &target.display)
                .env("XAUTHORITY", &target.authority)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut commands = child.0.stdin.take().ok_or("worker stdin missing")?;
        nonblocking(commands.as_raw_fd())?;
        let mut packet = (config.len() as u32).to_be_bytes().to_vec();
        packet.extend_from_slice(&config);
        commands.write_all(&packet)?;
        let commands = Arc::new(Mutex::new(commands));
        let mut output = child.0.stdout.take().ok_or("worker stdout missing")?;
        let (tx, frames) = frame_channel();
        let encoder = Arc::new(AtomicU8::new(0));
        let selected = encoder.clone();
        let request = commands.clone();
        let reader = thread::Builder::new()
            .name("remotevnc-capture-reader".into())
            .spawn(move || {
                let mut header = [0];
                if output.read_exact(&mut header).is_err() {
                    return;
                }
                if !matches!(header[0], 1..=3) {
                    return;
                }
                selected.store(header[0], Ordering::Release);
                let mut recovering = true;
                while let Ok(frame) = read_video_frame(&mut output) {
                    if frame.generation != generation {
                        break;
                    }
                    if recovering && !frame.keyframe {
                        continue;
                    }
                    match tx.try_send(frame) {
                        Ok(()) => recovering = false,
                        Err(FrameSendError::Full(_)) => {
                            recovering = true;
                            let _ = request.lock().unwrap().write_all(b"K");
                        }
                        Err(_) => break,
                    }
                }
            })?;
        Ok(Self {
            child,
            commands,
            frames,
            reader: Some(reader),
            encoder,
            profile,
        })
    }
    pub fn update_profile(&mut self, profile: VideoProfile) -> Result<()> {
        profile.validate()?;
        if profile.encoder != self.profile.encoder {
            return Err(error("encoder change requires capture restart"));
        }
        let bytes = serde_json::to_vec(&profile)?;
        let mut command = vec![b'P'];
        command.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        command.extend_from_slice(&bytes);
        // Under PIPE_BUF: nonblocking pipe writes are atomic, never partial.
        self.commands.lock().unwrap().write_all(&command)?;
        self.profile = profile;
        Ok(())
    }
    pub fn frames(&mut self) -> &mut Receiver<EncodedFrame> {
        &mut self.frames
    }
    pub fn request_keyframe(&self) -> Result<()> {
        self.commands.lock().unwrap().write_all(b"K")?;
        Ok(())
    }
    pub fn selected_encoder(&self) -> Option<VideoEncoder> {
        match self.encoder.load(Ordering::Acquire) {
            1 => Some(VideoEncoder::X264),
            2 => Some(VideoEncoder::Va),
            3 => Some(VideoEncoder::Nv),
            _ => None,
        }
    }
    pub fn stop(self) {
        drop(self);
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.child.0.kill();
        let _ = self.child.0.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
fn nonblocking(fd: i32) -> Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn validate_target(t: &DisplayTarget) -> Result<()> {
    t.socket_path()?;
    if !t.authority.is_absolute()
        || t.width > 7680
        || t.height > 4320
        || t.width == 0
        || t.height == 0
        || t.origin_x.checked_add(t.width).is_none_or(|v| v > 32767)
        || t.origin_y.checked_add(t.height).is_none_or(|v| v > 32767)
    {
        return Err(error("invalid capture output geometry/authority"));
    }
    Ok(())
}
#[cfg(any(target_os = "linux", test))]
fn encoder_description(encoder: VideoEncoder, p: VideoProfile) -> String {
    match encoder {
        VideoEncoder::X264 => format!(
            "x264enc name=encoder tune=zerolatency speed-preset=ultrafast bframes=0 key-int-max={} bitrate={}",
            p.fps, p.bitrate_kbps
        ),
        VideoEncoder::Va => format!(
            "vah264enc name=encoder b-frames=0 key-int-max={} bitrate={}",
            p.fps, p.bitrate_kbps
        ),
        VideoEncoder::Nv => format!(
            "nvh264enc name=encoder zerolatency=true bframes=0 gop-size={} bitrate={}",
            p.fps, p.bitrate_kbps
        ),
        VideoEncoder::Auto => unreachable!(),
    }
}
#[cfg(any(target_os = "linux", test))]
fn pipeline(source: &str, p: VideoProfile, encoder: VideoEncoder) -> Result<gst::Pipeline> {
    // Drop raw frames before inter-frame encoding. Never leak encoded deltas.
    let text = format!(
        "{source} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! videoconvert ! videoscale ! videorate drop-only=true ! capsfilter name=profile caps-change-mode=delayed caps=\"video/x-raw,format=I420,width={},height={},framerate={}/1\" ! {} ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au,profile=constrained-baseline ! appsink name=encoded max-buffers=2 drop=false sync=false",
        p.width,
        p.height,
        p.fps,
        encoder_description(encoder, p)
    );
    gst::parse::launch(&text)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| error("capture pipeline type"))
}
#[cfg(any(target_os = "linux", test))]
fn choose_encoder(p: VideoProfile) -> Result<VideoEncoder> {
    let choices: &[VideoEncoder] = match p.encoder {
        VideoEncoder::Auto => &[VideoEncoder::Va, VideoEncoder::Nv, VideoEncoder::X264],
        VideoEncoder::Va => &[VideoEncoder::Va],
        VideoEncoder::Nv => &[VideoEncoder::Nv],
        VideoEncoder::X264 => &[VideoEncoder::X264],
    };
    for encoder in choices {
        let Ok(probe) = pipeline(
            "videotestsrc is-live=true",
            VideoProfile {
                width: 320,
                height: 240,
                ..p
            },
            *encoder,
        ) else {
            continue;
        };
        // A candidate must encode and accept a live resize/bitrate change.
        // Hardware presence alone is insufficient (driver/device may be unusable).
        let success = (|| -> Result<bool> {
            let encoder = probe.by_name("encoder").ok_or("missing encoder")?;
            if !encoder
                .find_property("bitrate")
                .is_some_and(|p| p.flags().contains(gst::PARAM_FLAG_MUTABLE_PLAYING))
            {
                return Ok(false);
            }
            probe.set_state(gst::State::Playing)?;
            let sink = probe.by_name("encoded").ok_or("missing probe sink")?;
            let sample: Option<gst::Sample> =
                sink.emit_by_name("try-pull-sample", &[&2_000_000_000u64]);
            if sample.is_none() {
                return Ok(false);
            }
            apply_profile(
                &probe,
                VideoProfile {
                    width: 160,
                    height: 120,
                    ..p
                },
            )?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while std::time::Instant::now() < deadline {
                let sample: Option<gst::Sample> =
                    sink.emit_by_name("try-pull-sample", &[&200_000_000u64]);
                if let Some(sample) = sample
                    && sample
                        .caps()
                        .and_then(|c| c.structure(0))
                        .and_then(|s| s.get::<i32>("width").ok())
                        == Some(160)
                    && sample.buffer().is_some_and(|b| {
                        !b.flags().contains(gst::BufferFlags::DELTA_UNIT) && b.pts() == b.dts()
                    })
                {
                    return Ok(true);
                }
            }
            Ok(false)
        })()
        .unwrap_or(false);
        let _ = probe.set_state(gst::State::Null);
        if success {
            return Ok(*encoder);
        }
    }
    Err(error("no usable H264 encoder"))
}
#[cfg(any(target_os = "linux", test))]
fn keyframe(pipeline: &gst::Pipeline) -> Result<()> {
    let structure = gst::Structure::builder("GstForceKeyUnit")
        .field("all-headers", true)
        .field("count", 0u32)
        .build();
    if !pipeline.send_event(gst::event::CustomUpstream::new(structure)) {
        return Err(error("keyframe request rejected"));
    }
    Ok(())
}
#[cfg(any(target_os = "linux", test))]
fn apply_profile(pipeline: &gst::Pipeline, profile: VideoProfile) -> Result<()> {
    pipeline
        .by_name("encoder")
        .ok_or("missing encoder")?
        .set_property("bitrate", profile.bitrate_kbps);
    let caps = gst::Caps::builder("video/x-raw")
        .field("format", "I420")
        .field("width", profile.width as i32)
        .field("height", profile.height as i32)
        .field("framerate", gst::Fraction::new(profile.fps as i32, 1))
        .build();
    pipeline
        .by_name("profile")
        .ok_or("missing profile filter")?
        .set_property("caps", caps);
    keyframe(pipeline)
}
/// Called only by main before signal handlers, GStreamer, or any Rust threads.
pub fn worker_main() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        #[link(name = "X11")]
        unsafe extern "C" {
            fn XInitThreads() -> libc::c_int;
        }
        if unsafe { XInitThreads() } == 0 {
            return Err(error("XInitThreads failed"));
        }
    }
    #[cfg(not(target_os = "linux"))]
    return Err(error("physical X11 capture requires Linux"));
    #[cfg(target_os = "linux")]
    {
        let mut input = std::io::stdin();
        let mut size = [0; 4];
        input.read_exact(&mut size)?;
        let length = u32::from_be_bytes(size) as usize;
        if length == 0 || length > CONTROL_PAYLOAD_LIMIT {
            return Err(error("invalid worker configuration length"));
        }
        let mut bytes = vec![0; length];
        input.read_exact(&mut bytes)?;
        let config: WorkerConfig = serde_json::from_slice(&bytes)?;
        config.profile.validate()?;
        validate_target(&config.target)?;
        if std::env::var_os("XAUTHORITY").as_deref() != Some(config.target.authority.as_os_str())
            || std::env::var("DISPLAY").ok().as_deref() != Some(&config.target.display)
        {
            return Err(error("worker environment does not match display"));
        }
        gst::init()?;
        let encoder = choose_encoder(config.profile)?;
        let t = &config.target;
        let source = format!(
            "ximagesrc display-name={} use-damage=false show-pointer=true startx={} starty={} endx={} endy={}",
            t.display,
            t.origin_x,
            t.origin_y,
            t.origin_x + t.width - 1,
            t.origin_y + t.height - 1
        );
        let pipeline = pipeline(&source, config.profile, encoder)?;
        let sink = pipeline.by_name("encoded").ok_or("missing capture sink")?;
        nonblocking(input.as_raw_fd())?;
        let mut output = std::io::stdout();
        output.write_all(&[match encoder {
            VideoEncoder::X264 => 1,
            VideoEncoder::Va => 2,
            VideoEncoder::Nv => 3,
            _ => unreachable!(),
        }])?;
        pipeline.set_state(gst::State::Playing)?;
        let mut base = None;
        let mut pending = Vec::new();
        let result = (|| -> Result<()> {
            loop {
                let mut commands = [0; 512];
                match input.read(&mut commands) {
                    Ok(0) => return Ok(()),
                    Ok(n) => pending.extend_from_slice(&commands[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
                while !pending.is_empty() {
                    if pending[0] == b'K' {
                        pending.remove(0);
                        keyframe(&pipeline)?;
                        continue;
                    }
                    if pending[0] != b'P' {
                        return Err(error("invalid capture command"));
                    }
                    if pending.len() < 5 {
                        break;
                    }
                    let n = u32::from_be_bytes(pending[1..5].try_into()?) as usize;
                    if n > 256 {
                        return Err(error("capture command too large"));
                    }
                    if pending.len() < n + 5 {
                        break;
                    }
                    let profile: VideoProfile = serde_json::from_slice(&pending[5..5 + n])?;
                    profile.validate()?;
                    if profile.encoder != config.profile.encoder {
                        return Err(error("encoder change requires restart"));
                    }
                    apply_profile(&pipeline, profile)?;
                    pending.drain(..n + 5);
                }
                if let Some(bus) = pipeline.bus() {
                    for msg in bus.iter() {
                        if matches!(
                            msg.view(),
                            gst::MessageView::Error(_) | gst::MessageView::Eos(_)
                        ) {
                            return Err(error("capture pipeline stopped"));
                        }
                    }
                }
                let sample: Option<gst::Sample> =
                    sink.emit_by_name("try-pull-sample", &[&100_000_000u64]);
                let Some(sample) = sample else {
                    continue;
                };
                let buffer = sample.buffer().ok_or("capture buffer missing")?;
                let pts = buffer.pts().ok_or("capture timestamp missing")?.nseconds();
                let origin = *base.get_or_insert(pts);
                let data = buffer.map_readable()?;
                let frame = EncodedFrame {
                    generation: config.generation,
                    pts_ns: pts
                        .checked_sub(origin)
                        .ok_or("nonmonotonic capture timestamp")?,
                    keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
                    data: data.as_slice().to_vec(),
                };
                write_video_frame(&mut output, &frame)?;
            }
        })();
        let _ = pipeline.set_state(gst::State::Null);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_pattern_encoder_has_config_keyframes_and_no_reordering() {
        gst::init().unwrap();
        let p = VideoProfile {
            width: 320,
            height: 240,
            encoder: VideoEncoder::X264,
            ..VideoProfile::default()
        };
        let selected = choose_encoder(p).unwrap();
        assert_eq!(selected, VideoEncoder::X264);
        let pipe = pipeline(
            "videotestsrc is-live=true num-buffers=10 pattern=red",
            p,
            selected,
        )
        .unwrap();
        pipe.set_state(gst::State::Playing).unwrap();
        let sink = pipe.by_name("encoded").unwrap();
        let mut last = None;
        for index in 0..10 {
            let sample: Option<gst::Sample> =
                sink.emit_by_name("try-pull-sample", &[&3_000_000_000u64]);
            let sample = sample.unwrap();
            let b = sample.buffer().unwrap();
            assert_eq!(b.pts(), b.dts());
            if let Some(last) = last {
                assert!(b.pts().unwrap() > last);
            }
            last = b.pts();
            if index == 0 {
                assert!(!b.flags().contains(gst::BufferFlags::DELTA_UNIT));
                let bytes = b.map_readable().unwrap();
                for nal in [7, 8, 5] {
                    assert!(
                        bytes
                            .windows(4)
                            .any(|v| v[..3] == [0, 0, 1] && v[3] & 31 == nal)
                    );
                }
            }
        }
        apply_profile(&pipe, p.downshift()).unwrap();
        pipe.set_state(gst::State::Null).unwrap();
    }
}
