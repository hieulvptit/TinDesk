//! Screen capture through AVFoundation + a hardware/software H.264 encoder.
//! Output is the same Annex-B access-unit stream the Linux capture worker emits.
use crate::{Result, display::DisplayTarget, error};
use binachicken_protocol::*;
use gst::prelude::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

const KEYFRAME_FALLBACK_SECS: u32 = 10;

fn encoder_description(encoder: VideoEncoder, p: VideoProfile) -> String {
    let gop = p.fps.saturating_mul(KEYFRAME_FALLBACK_SECS);
    match encoder {
        VideoEncoder::X264 => format!(
            "x264enc name=encoder tune=zerolatency speed-preset=superfast bframes=0 key-int-max={gop} pass=qual quantizer=20 vbv-buf-capacity=300 bitrate={}",
            p.bitrate_kbps
        ),
        // Auto/Va/Nv all mean "VideoToolbox" on a Mac.
        _ => format!(
            "vtenc_h264 name=encoder realtime=true allow-frame-reordering=false max-keyframe-interval={gop} bitrate={}",
            p.bitrate_kbps
        ),
    }
}
fn pipeline(
    display: &DisplayTarget,
    p: VideoProfile,
    encoder: VideoEncoder,
) -> Result<gst::Pipeline> {
    let h264_profile = if encoder == VideoEncoder::X264 {
        "high"
    } else {
        "constrained-baseline"
    };
    let text = format!(
        "avfvideosrc name=source capture-screen=true capture-screen-cursor=true capture-screen-mouse-clicks=true device-index={} ! queue max-size-buffers=2 max-size-bytes=0 max-size-time=0 leaky=downstream ! videoconvert n-threads=2 ! videoscale n-threads=2 ! videorate drop-only=true ! capsfilter name=profile caps-change-mode=delayed caps=\"video/x-raw,format=I420,width={},height={},framerate={}/1\" ! {} ! h264parse config-interval=-1 ! video/x-h264,stream-format=byte-stream,alignment=au,profile={h264_profile} ! appsink name=encoded max-buffers=2 drop=false sync=false",
        display_index(display),
        p.width,
        p.height,
        p.fps,
        encoder_description(encoder, p),
    );
    gst::parse::launch(&text)?
        .downcast::<gst::Pipeline>()
        .map_err(|_| error("capture pipeline type"))
}
/// AVFoundation numbers screens in CGGetActiveDisplayList order; main is first.
fn display_index(_: &DisplayTarget) -> u32 {
    0
}
fn choose_encoder(p: VideoProfile) -> VideoEncoder {
    if p.encoder == VideoEncoder::X264
        || gst::ElementFactory::find("vtenc_h264").is_none()
        || std::env::var_os("BINACHICKEN_MAC_SOFTWARE_ENCODER").is_some()
    {
        VideoEncoder::X264
    } else {
        VideoEncoder::Va
    }
}
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

pub struct Capture {
    pipeline: gst::Pipeline,
    frames: Receiver<EncodedFrame>,
    profile: VideoProfile,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<CaptureFailure>>,
    thread: Option<JoinHandle<()>>,
}
impl Capture {
    pub fn start(target: &DisplayTarget, profile: VideoProfile, generation: u64) -> Result<Self> {
        profile.validate()?;
        gst::init()?;
        let encoder = choose_encoder(profile);
        let pipeline = pipeline(target, profile, encoder)?;
        let sink = pipeline.by_name("encoded").ok_or("missing capture sink")?;
        let (tx, frames) = sync_channel(binachicken_protocol::FRAME_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(CaptureFailure::new(CapturePhase::Startup)));
        pipeline.set_state(gst::State::Playing).map_err(|_| {
            error("cannot start capture; grant Screen Recording permission to the helper")
        })?;
        let thread = thread::Builder::new()
            .name("binachicken-mac-capture".into())
            .spawn({
                let pipeline = pipeline.clone();
                let stop = stop.clone();
                let failure = failure.clone();
                move || pump(pipeline, sink, tx, stop, failure, generation)
            })?;
        Ok(Self {
            pipeline,
            frames,
            profile,
            stop,
            failure,
            thread: Some(thread),
        })
    }
    pub fn frames(&mut self) -> &mut Receiver<EncodedFrame> {
        &mut self.frames
    }
    pub fn failure(&mut self) -> CaptureFailure {
        self.failure.lock().unwrap().clone()
    }
    pub fn request_keyframe(&self) -> Result<()> {
        keyframe(&self.pipeline)
    }
    pub fn update_profile(&mut self, profile: VideoProfile) -> Result<()> {
        profile.validate()?;
        apply_profile(&self.pipeline, profile)?;
        self.profile = profile;
        Ok(())
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn pump(
    pipeline: gst::Pipeline,
    sink: gst::Element,
    tx: SyncSender<EncodedFrame>,
    stop: Arc<AtomicBool>,
    failure: Arc<Mutex<CaptureFailure>>,
    generation: u64,
) {
    let bus = pipeline.bus();
    let mut base = None;
    while !stop.load(Ordering::Acquire) {
        if let Some(bus) = &bus {
            while let Some(msg) = bus.pop() {
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        let err = e.error();
                        let element = match e.src().map(|s| s.name()).as_deref() {
                            Some("source") => CaptureElement::Source,
                            Some("encoder") => CaptureElement::Encoder,
                            Some("profile") => CaptureElement::Profile,
                            Some("encoded") => CaptureElement::Encoded,
                            _ => CaptureElement::Unknown,
                        };
                        *failure.lock().unwrap() = CaptureFailure {
                            phase: CapturePhase::Stream,
                            domain: CaptureDomain::Resource,
                            code: err.code(),
                            element,
                            exit: CaptureExit::Unknown,
                        };
                        return;
                    }
                    gst::MessageView::Eos(_) => {
                        *failure.lock().unwrap() = CaptureFailure::new(CapturePhase::Eos);
                        return;
                    }
                    _ => {}
                }
            }
        }
        let sample: Option<gst::Sample> = sink.emit_by_name("try-pull-sample", &[&100_000_000u64]);
        let Some(sample) = sample else { continue };
        let Some(buffer) = sample.buffer() else {
            continue;
        };
        let Some(pts) = buffer.pts() else { continue };
        let origin = *base.get_or_insert(pts.nseconds());
        let Ok(map) = buffer.map_readable() else {
            continue;
        };
        let frame = EncodedFrame {
            generation,
            pts_ns: pts.nseconds().saturating_sub(origin),
            keyframe: !buffer.flags().contains(gst::BufferFlags::DELTA_UNIT),
            data: map.as_slice().to_vec(),
        };
        // A full queue means the writer is behind; the serve loop requests a
        // keyframe on its own backpressure, so bounded waiting here is enough.
        let mut frame = Some(frame);
        while let Some(f) = frame.take() {
            match tx.try_send(f) {
                Ok(()) => {}
                Err(std::sync::mpsc::TrySendError::Full(f)) => {
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(2));
                    frame = Some(f);
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return,
            }
        }
    }
}
