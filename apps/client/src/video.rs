use remotevnc_protocol::DisplayInfo;
use remotevnc_transport::DecodedFrame;
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
#[derive(Clone, Default)]
pub struct LatestFrame(Arc<Mutex<Option<DecodedFrame>>>);
impl LatestFrame {
    pub fn replace(&self, frame: DecodedFrame) {
        *self.0.lock().unwrap() = Some(frame);
    }
    pub fn take(&self) -> Option<DecodedFrame> {
        self.0.lock().unwrap().take()
    }
    pub fn clear(&self) {
        self.0.lock().unwrap().take();
    }
}
