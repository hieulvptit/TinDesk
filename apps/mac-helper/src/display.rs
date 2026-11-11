//! Main-display discovery and macOS permission probes.
use core_graphics::display::CGDisplay;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTarget {
    pub id: u32,
    /// Capture/encode size in physical pixels (what the client sees).
    pub width: u32,
    pub height: u32,
    /// Global-coordinate bounds in points (what CGEvent uses).
    pub origin_x: i64,
    pub origin_y: i64,
    pub points_w: u32,
    pub points_h: u32,
}

/// The main display, or `None` when none is attached. A sleeping or locked
/// display still counts: a session wakes it (see `KeepAwake`) so the user can
/// type the password at the lock screen.
pub fn main_display() -> Option<DisplayTarget> {
    let display = CGDisplay::main();
    if !display.is_online() {
        return None;
    }
    let bounds = display.bounds();
    let (width, height) = (display.pixels_wide() as u32, display.pixels_high() as u32);
    if width == 0 || height == 0 || bounds.size.width < 1.0 || bounds.size.height < 1.0 {
        return None;
    }
    Some(DisplayTarget {
        id: display.id,
        width: width & !1,
        height: height & !1,
        origin_x: bounds.origin.x as i64,
        origin_y: bounds.origin.y as i64,
        points_w: bounds.size.width as u32,
        points_h: bounds.size.height as u32,
    })
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: core_foundation::dictionary::CFDictionaryRef)
    -> bool;
}
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Accessibility permission, required to inject keyboard/mouse events.
pub fn input_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}
/// Screen Recording permission, required to capture the display.
pub fn capture_allowed() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}
/// Shows the system Screen Recording prompt once; returns the current state.
pub fn request_capture() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Wakes the display and keeps it (and the system) awake while a session is
/// live; dropping it lets the Mac sleep again.
pub struct KeepAwake(Option<std::process::Child>);
impl KeepAwake {
    pub fn new() -> Self {
        use std::process::{Command, Stdio};
        let quiet = |c: &mut Command| {
            c.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()
        };
        // Declares user activity once, which wakes a sleeping display.
        let _ = quiet(Command::new("/usr/bin/caffeinate").args(["-u", "-t", "5"]));
        let pid = std::process::id().to_string();
        Self(quiet(
            Command::new("/usr/bin/caffeinate").args(["-d", "-i", "-w", &pid]),
        ))
    }
}
impl Default for KeepAwake {
    fn default() -> Self {
        Self::new()
    }
}
impl Drop for KeepAwake {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Shows the system Accessibility prompt (with an "Open System Settings"
/// button) when permission is missing; returns the current state.
pub fn request_input() -> bool {
    use core_foundation::{
        base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, string::CFString,
    };
    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let options =
        CFDictionary::from_CFType_pairs(&[(key.as_CFType(), CFBoolean::true_value().as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
}
/// Asks for every missing permission so the user only has to click Allow.
/// Returns true when both are already granted.
pub fn request_permissions() -> bool {
    let capture = capture_allowed() || request_capture();
    let input = input_trusted() || request_input();
    capture && input
}
