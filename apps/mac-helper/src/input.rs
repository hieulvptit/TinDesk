//! Keyboard/mouse injection through Quartz events. Needs Accessibility permission.
use crate::{Result, display::DisplayTarget, error, keymap};
use binachicken_protocol::{InputEvent, MouseButton};
use core_graphics::{
    event::{CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, EventField},
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

pub struct MacInput {
    source: CGEventSource,
    target: DisplayTarget,
    flags: u64,
    keys: BTreeSet<u16>,
    unicode_keys: BTreeSet<u32>,
    buttons: BTreeSet<u8>,
    last_pos: (u32, u32),
    last_click: Option<(Instant, u8, (u32, u32))>,
    click_count: i64,
}
fn slot(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}
impl MacInput {
    pub fn connect(target: &DisplayTarget) -> Result<Self> {
        if !crate::display::input_trusted() {
            return Err(error(
                "Accessibility permission is required to control this Mac",
            ));
        }
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| error("cannot create event source"))?;
        Ok(Self {
            source,
            target: target.clone(),
            flags: 0,
            keys: BTreeSet::new(),
            unicode_keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            last_pos: (0, 0),
            last_click: None,
            click_count: 1,
        })
    }
    fn point(&self, x: u32, y: u32) -> CGPoint {
        let sx = f64::from(self.target.points_w) / f64::from(self.target.width);
        let sy = f64::from(self.target.points_h) / f64::from(self.target.height);
        CGPoint::new(
            self.target.origin_x as f64 + f64::from(x.min(self.target.width - 1)) * sx,
            self.target.origin_y as f64 + f64::from(y.min(self.target.height - 1)) * sy,
        )
    }
    fn post(&self, event: CGEvent) {
        event.set_flags(CGEventFlags::from_bits_retain(self.flags));
        event.post(CGEventTapLocation::HID);
    }
    fn mouse(&mut self, kind: CGEventType, button: u8, x: u32, y: u32) -> Result<()> {
        let cg_button = match button {
            0 => CGMouseButton::Left,
            1 => CGMouseButton::Right,
            _ => CGMouseButton::Center,
        };
        let event =
            CGEvent::new_mouse_event(self.source.clone(), kind, self.point(x, y), cg_button)
                .map_err(|_| error("cannot create mouse event"))?;
        if button >= 2 {
            let number = match button {
                2 => 2,
                other => i64::from(other),
            };
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
        }
        event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, self.click_count);
        self.post(event);
        self.last_pos = (x, y);
        Ok(())
    }
    fn move_to(&mut self, x: u32, y: u32) -> Result<()> {
        let kind = match self.buttons.iter().next() {
            Some(0) => CGEventType::LeftMouseDragged,
            Some(1) => CGEventType::RightMouseDragged,
            Some(_) => CGEventType::OtherMouseDragged,
            None => CGEventType::MouseMoved,
        };
        let button = self.buttons.iter().next().copied().unwrap_or(0);
        self.mouse(kind, button, x, y)
    }
    fn button(&mut self, button: MouseButton, pressed: bool, x: u32, y: u32) -> Result<()> {
        let id = slot(button);
        let kind = match (id, pressed) {
            (0, true) => CGEventType::LeftMouseDown,
            (0, false) => CGEventType::LeftMouseUp,
            (1, true) => CGEventType::RightMouseDown,
            (1, false) => CGEventType::RightMouseUp,
            (_, true) => CGEventType::OtherMouseDown,
            (_, false) => CGEventType::OtherMouseUp,
        };
        if pressed {
            let now = Instant::now();
            self.click_count = match self.last_click {
                Some((at, b, pos))
                    if b == id
                        && now.duration_since(at) < Duration::from_millis(400)
                        && pos.0.abs_diff(x) < 8
                        && pos.1.abs_diff(y) < 8 =>
                {
                    self.click_count + 1
                }
                _ => 1,
            };
            self.last_click = Some((now, id, (x, y)));
            self.buttons.insert(id);
        } else {
            self.buttons.remove(&id);
        }
        // Make sure the cursor is where the click lands before the press.
        if pressed {
            self.mouse(CGEventType::MouseMoved, 0, x, y)?;
        }
        self.mouse(kind, id, x, y)
    }
    fn key_event(&self, code: u16, down: bool) -> Result<()> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), code, down)
            .map_err(|_| error("cannot create key event"))?;
        self.post(event);
        Ok(())
    }
    fn unicode_char(&self, ch: char, down: bool) -> Result<()> {
        let event = CGEvent::new_keyboard_event(self.source.clone(), 0, down)
            .map_err(|_| error("cannot create key event"))?;
        let mut buf = [0u8; 4];
        event.set_string(ch.encode_utf8(&mut buf));
        self.post(event);
        Ok(())
    }
    fn key(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        if let Some(flag) = keymap::modifier_flag(keysym) {
            if pressed {
                self.flags |= flag;
            } else {
                self.flags &= !flag;
            }
        }
        if let Some(code) = keymap::keycode(keysym) {
            // Plain modifier keys carry their own flag change.
            if pressed {
                self.keys.insert(code);
            } else {
                self.keys.remove(&code);
            }
            return self.key_event(code, pressed);
        }
        if let Some(ch) = keymap::unicode(keysym) {
            if pressed {
                self.unicode_keys.insert(keysym);
            } else {
                self.unicode_keys.remove(&keysym);
            }
            return self.unicode_char(ch, pressed);
        }
        Ok(())
    }
    fn text(&mut self, text: &str) -> Result<()> {
        for ch in text.chars() {
            self.unicode_char(ch, true)?;
            self.unicode_char(ch, false)?;
        }
        Ok(())
    }
    fn scroll(&mut self, dx: i32, dy: i32, x: u32, y: u32) -> Result<()> {
        self.mouse(CGEventType::MouseMoved, 0, x, y)?;
        let event =
            CGEvent::new(self.source.clone()).map_err(|_| error("cannot create scroll event"))?;
        event.set_type(CGEventType::ScrollWheel);
        // Wire and Quartz agree: positive Y scrolls content up, positive X left.
        event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1, i64::from(dy));
        event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2, i64::from(dx));
        event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 0);
        self.post(event);
        Ok(())
    }
    pub fn apply(&mut self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::PointerMove { x, y } => self.move_to(*x, *y),
            InputEvent::Button {
                button,
                pressed,
                x,
                y,
            } => self.button(*button, *pressed, *x, *y),
            InputEvent::Key { keysym, pressed } => self.key(*keysym, *pressed),
            InputEvent::Text { text } => self.text(text),
            InputEvent::Scroll {
                delta_x,
                delta_y,
                x,
                y,
            } => self.scroll(*delta_x, *delta_y, *x, *y),
            InputEvent::ReleaseAll => self.release_all(),
            InputEvent::VideoProfile { .. } | InputEvent::DisplayAck => Ok(()),
        }
    }
    /// Releases everything this session pressed; safe to call repeatedly.
    pub fn release_all(&mut self) -> Result<()> {
        let (x, y) = self.last_pos;
        let buttons: Vec<u8> = self.buttons.iter().copied().collect();
        for b in buttons {
            let kind = match b {
                0 => CGEventType::LeftMouseUp,
                1 => CGEventType::RightMouseUp,
                _ => CGEventType::OtherMouseUp,
            };
            let _ = self.mouse(kind, b, x, y);
        }
        self.buttons.clear();
        let keys: Vec<u16> = self.keys.iter().copied().collect();
        self.flags = 0;
        for code in keys {
            let _ = self.key_event(code, false);
        }
        self.keys.clear();
        let unicode: Vec<u32> = self.unicode_keys.iter().copied().collect();
        for keysym in unicode {
            if let Some(ch) = keymap::unicode(keysym) {
                let _ = self.unicode_char(ch, false);
            }
        }
        self.unicode_keys.clear();
        Ok(())
    }
}
impl Drop for MacInput {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}
