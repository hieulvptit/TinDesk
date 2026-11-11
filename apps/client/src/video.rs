use binachicken_protocol::DisplayInfo;
use binachicken_transport::DecodedFrame;
use eframe::egui;
use std::sync::{Arc, Mutex};

/// All geometry here is in egui logical points, already divided by native Retina scale.
#[derive(Clone, Copy)]
pub struct Viewport {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}
impl Viewport {
    pub fn fit(bounds: Self, width: u32, height: u32) -> Self {
        let scale = (bounds.width / width.max(1) as f32).min(bounds.height / height.max(1) as f32);
        let (width, height) = (width as f32 * scale, height as f32 * scale);
        Self {
            left: bounds.left + (bounds.width - width) / 2.,
            top: bounds.top + (bounds.height - height) / 2.,
            width,
            height,
        }
    }
    /// Align to physical pixels so sampling never straddles two screen pixels,
    /// and show a near-native fit at exactly one image pixel per screen pixel.
    pub fn snap(self, width: u32, height: u32, pixels_per_point: f32) -> Self {
        if !pixels_per_point.is_finite() || pixels_per_point <= 0. {
            return self;
        }
        let (mut w, mut h) = (
            self.width * pixels_per_point,
            self.height * pixels_per_point,
        );
        if (w - width as f32).abs() <= 2. && (h - height as f32).abs() <= 2. {
            (w, h) = (width as f32, height as f32);
        }
        let center = (
            (self.left + self.width / 2.) * pixels_per_point,
            (self.top + self.height / 2.) * pixels_per_point,
        );
        Self {
            left: (center.0 - w.round() / 2.).round() / pixels_per_point,
            top: (center.1 - h.round() / 2.).round() / pixels_per_point,
            width: w.round() / pixels_per_point,
            height: h.round() / pixels_per_point,
        }
    }
    pub fn position(self, x: f32, y: f32, display: DisplayInfo, clamp: bool) -> Option<(u32, u32)> {
        if !x.is_finite()
            || !y.is_finite()
            || self.width <= 0.
            || self.height <= 0.
            || display.validate().is_err()
        {
            return None;
        }
        let (x, y) = ((x - self.left) / self.width, (y - self.top) / self.height);
        if !clamp && (!(0.0..1.0).contains(&x) || !(0.0..1.0).contains(&y)) {
            return None;
        }
        Some((
            (x.clamp(0., 1.) * display.width as f32) as u32,
            (y.clamp(0., 1.) * display.height as f32) as u32,
        ))
        .map(|(x, y)| (x.min(display.width - 1), y.min(display.height - 1)))
    }
}
/// A single replaceable frame; a slow UI never accumulates decoded images.
#[derive(Default)]
struct FrameState {
    frame: Option<DecodedFrame>,
    dimensions: Option<(u32, u32)>,
    arrivals: std::collections::VecDeque<std::time::Instant>,
    repaint: Option<egui::Context>,
}
#[derive(Clone, Default)]
pub struct LatestFrame(Arc<Mutex<FrameState>>);
impl LatestFrame {
    pub fn replace(&self, frame: DecodedFrame) {
        let mut state = self.0.lock().unwrap();
        let now = std::time::Instant::now();
        state.dimensions = Some((frame.width, frame.height));
        state.arrivals.push_back(now);
        while state.arrivals.len() > 128
            || state
                .arrivals
                .front()
                .is_some_and(|t| now.duration_since(*t).as_secs_f64() > 2.)
        {
            state.arrivals.pop_front();
        }
        state.frame = Some(frame);
        let repaint = state.repaint.clone();
        drop(state);
        // Show a decoded frame immediately rather than at the next idle repaint.
        if let Some(ctx) = repaint {
            ctx.request_repaint();
        }
    }
    pub fn set_repaint(&self, ctx: &egui::Context) {
        let mut state = self.0.lock().unwrap();
        if state.repaint.is_none() {
            state.repaint = Some(ctx.clone());
        }
    }
    /// Observed decoded arrivals over at most two seconds, never target FPS.
    pub fn metrics(&self) -> (Option<(u32, u32)>, Option<f64>) {
        let state = self.0.lock().unwrap();
        let fps = state
            .arrivals
            .back()
            .filter(|t| t.elapsed().as_secs_f64() <= 2.)
            .and_then(|last| {
                state
                    .arrivals
                    .front()
                    .filter(|first| *first < last)
                    .map(|first| {
                        (state.arrivals.len() - 1) as f64
                            / last.duration_since(*first).as_secs_f64()
                    })
            });
        (state.dimensions, fps)
    }
    pub fn take(&self) -> Option<DecodedFrame> {
        self.0.lock().unwrap().frame.take()
    }
    pub fn clear(&self) {
        let mut state = self.0.lock().unwrap();
        *state = FrameState {
            repaint: state.repaint.take(),
            ..FrameState::default()
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn near_native_fit_snaps_to_whole_physical_pixels() {
        let fitted = Viewport {
            left: 0.3,
            top: 10.7,
            width: 960.6,
            height: 540.4,
        }
        .snap(1920, 1080, 2.);
        assert_eq!((fitted.width, fitted.height), (960., 540.));
        assert_eq!((fitted.left * 2.).fract(), 0.);
        assert_eq!((fitted.top * 2.).fract(), 0.);
        let scaled = Viewport {
            left: 0.,
            top: 0.,
            width: 1512.,
            height: 850.5,
        }
        .snap(1920, 1080, 2.);
        assert_eq!((scaled.width, scaled.height), (1512., 850.5));
    }
    use std::time::{Duration, Instant};
    #[test]
    fn observed_rate_is_unavailable_without_samples_and_after_stall() {
        let frames = LatestFrame::default();
        assert_eq!(frames.metrics(), (None, None));
        let now = Instant::now();
        {
            let mut state = frames.0.lock().unwrap();
            state.dimensions = Some((320, 180));
            state
                .arrivals
                .extend([now - Duration::from_millis(100), now]);
        }
        assert_eq!(frames.metrics(), (Some((320, 180)), Some(10.)));
        frames.0.lock().unwrap().arrivals = std::collections::VecDeque::from([
            now - Duration::from_secs(4),
            now - Duration::from_secs(3),
        ]);
        assert_eq!(frames.metrics(), (Some((320, 180)), None));
        frames.clear();
        assert_eq!(frames.metrics(), (None, None));
    }
}
