use crate::video::Viewport;
use binachicken_protocol::{DisplayInfo, InputEvent, MouseButton, TEXT_LIMIT};
use eframe::egui;
use std::collections::BTreeSet;

#[derive(Default)]
pub struct InputMapper {
    generation: Option<u64>,
    keys: BTreeSet<u32>,
    buttons: Vec<MouseButton>,
    composing: bool,
    scroll_points: [f32; 2],
}
pub enum ClientInput {
    Pointer {
        x: f32,
        y: f32,
    },
    Button {
        button: MouseButton,
        pressed: bool,
        x: f32,
        y: f32,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
        x: f32,
        y: f32,
    },
    Key {
        keysym: u32,
        pressed: bool,
        printable: bool,
        shortcut: bool,
    },
    Text(String),
    Composition(String),
    Commit(String),
    FocusLost,
}
impl InputMapper {
    pub fn reset(&mut self) {
        self.keys.clear();
        self.buttons.clear();
        self.composing = false;
        self.scroll_points = [0.; 2];
    }
    pub fn map(
        &mut self,
        event: ClientInput,
        viewport: Viewport,
        display: DisplayInfo,
    ) -> Vec<InputEvent> {
        if self
            .generation
            .replace(display.generation)
            .is_some_and(|g| g != display.generation)
        {
            self.reset();
            return vec![InputEvent::ReleaseAll];
        }
        let event = match event {
            ClientInput::FocusLost => {
                self.reset();
                InputEvent::ReleaseAll
            }
            ClientInput::Composition(text) => {
                self.composing = !text.is_empty();
                return vec![];
            }
            ClientInput::Commit(text) => {
                self.composing = false;
                if text.is_empty() || text.len() > TEXT_LIMIT {
                    return vec![];
                }
                InputEvent::Text { text }
            }
            ClientInput::Text(text) => {
                if self.composing
                    || [0xffe3, 0xffe9, 0xffeb]
                        .iter()
                        .any(|k| self.keys.contains(k))
                    || text.is_empty()
                    || text.len() > TEXT_LIMIT
                {
                    return vec![];
                }
                InputEvent::Text { text }
            }
            ClientInput::Key {
                keysym,
                pressed,
                printable,
                shortcut,
            } => {
                if pressed {
                    if self.composing || (printable && !shortcut) {
                        return vec![];
                    }
                    self.keys.insert(keysym);
                } else if !self.keys.remove(&keysym) {
                    return vec![];
                }
                InputEvent::Key { keysym, pressed }
            }
            ClientInput::Pointer { x, y } => {
                let Some((x, y)) = viewport.position(x, y, display, !self.buttons.is_empty())
                else {
                    return vec![];
                };
                InputEvent::PointerMove { x, y }
            }
            ClientInput::Button {
                button,
                pressed,
                x,
                y,
            } => {
                let held = self.buttons.contains(&button);
                if !pressed && !held {
                    return vec![];
                }
                let Some((x, y)) = viewport.position(x, y, display, held) else {
                    return vec![];
                };
                if pressed {
                    if !held {
                        self.buttons.push(button)
                    }
                } else {
                    self.buttons.retain(|b| *b != button)
                }
                InputEvent::Button {
                    button,
                    pressed,
                    x,
                    y,
                }
            }
            ClientInput::Scroll {
                delta_x,
                delta_y,
                x,
                y,
            } => {
                let Some((x, y)) = viewport.position(x, y, display, false) else {
                    return vec![];
                };
                InputEvent::Scroll {
                    delta_x,
                    delta_y,
                    x,
                    y,
                }
            }
        };
        vec![event]
    }
    fn modifiers(&mut self, m: egui::Modifiers, v: Viewport, d: DisplayInfo) -> Vec<InputEvent> {
        let mut out = vec![];
        for (keysym, pressed) in [
            (0xffe1, m.shift),
            // Command is the Mac shortcut key: it acts as Control on the remote, so
            // Cmd+C/V/X/A/Z work there. A held physical Control shares the same key.
            (0xffe3, m.ctrl || m.mac_cmd),
            (0xffe9, m.alt),
        ] {
            if self.keys.contains(&keysym) != pressed {
                out.extend(self.map(
                    ClientInput::Key {
                        keysym,
                        pressed,
                        printable: false,
                        shortcut: true,
                    },
                    v,
                    d,
                ));
            }
        }
        out
    }
    /// egui-winit emits Key + Text for ordinary printing, and Ime::Commit for
    /// committed composition. Pair dedup is batch-local; identical commits remain distinct.
    pub fn native_batch(
        &mut self,
        events: &[egui::Event],
        modifiers: egui::Modifiers,
        pointer: Option<egui::Pos2>,
        v: Viewport,
        d: DisplayInfo,
    ) -> Vec<InputEvent> {
        let mut out = vec![];
        let mut previous_commit: Option<&str> = None;
        for (index, event) in events.iter().enumerate() {
            // Prefer the actual IME commit in either adjacent duplicate order.
            // Never deduplicate two commits, or keep a cache across UI batches.
            if let egui::Event::Text(text) = event {
                let followed_by_commit = matches!(events.get(index+1),Some(egui::Event::Ime(egui::ImeEvent::Commit(commit))) if commit==text);
                if previous_commit == Some(text.as_str()) || followed_by_commit {
                    previous_commit = None;
                    continue;
                }
            }
            previous_commit = match event {
                egui::Event::Ime(egui::ImeEvent::Commit(text)) => Some(text.as_str()),
                _ => None,
            };
            let input = match event {
                egui::Event::WindowFocused(false) => Some(ClientInput::FocusLost),
                egui::Event::PointerMoved(p) => Some(ClientInput::Pointer { x: p.x, y: p.y }),
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    out.extend(self.modifiers(*modifiers, v, d));
                    Some(ClientInput::Button {
                        button: match button {
                            egui::PointerButton::Primary => MouseButton::Left,
                            egui::PointerButton::Secondary => MouseButton::Right,
                            egui::PointerButton::Middle => MouseButton::Middle,
                            egui::PointerButton::Extra1 => MouseButton::Back,
                            egui::PointerButton::Extra2 => MouseButton::Forward,
                        },
                        pressed: *pressed,
                        x: pos.x,
                        y: pos.y,
                    })
                }
                egui::Event::Key {
                    key,
                    pressed,
                    modifiers,
                    ..
                } => {
                    out.extend(self.modifiers(*modifiers, v, d));
                    keysym(*key).map(|(keysym, printable)| ClientInput::Key {
                        keysym,
                        pressed: *pressed,
                        printable,
                        shortcut: modifiers.ctrl || modifiers.mac_cmd || modifiers.alt,
                    })
                }
                egui::Event::Text(s) => Some(ClientInput::Text(s.clone())),
                egui::Event::Ime(egui::ImeEvent::Preedit { text, .. }) => {
                    Some(ClientInput::Composition(text.clone()))
                }
                egui::Event::Ime(egui::ImeEvent::Commit(s)) => Some(ClientInput::Commit(s.clone())),
                // NativeOptions disables clipboard shortcuts. Paste/Copy/Cut
                // payloads (including externally supplied events) never go remote.
                egui::Event::ModifiersChanged(m) => {
                    out.extend(self.modifiers(*m, v, d));
                    None
                }
                egui::Event::MouseWheel { delta, unit, .. } => pointer.and_then(|p| {
                    v.position(p.x, p.y, d, false)?;
                    let scale = match unit {
                        egui::MouseWheelUnit::Point => 1.,
                        egui::MouseWheelUnit::Line => 40.,
                        egui::MouseWheelUnit::Page => 400.,
                    };
                    self.scroll_points[0] += delta.x * scale;
                    self.scroll_points[1] += delta.y * scale;
                    let delta_x = (self.scroll_points[0] / 40.).trunc() as i32;
                    let delta_y = (self.scroll_points[1] / 40.).trunc() as i32;
                    self.scroll_points[0] -= delta_x as f32 * 40.;
                    self.scroll_points[1] -= delta_y as f32 * 40.;
                    (delta_x != 0 || delta_y != 0).then_some(ClientInput::Scroll {
                        delta_x,
                        delta_y,
                        x: p.x,
                        y: p.y,
                    })
                }),
                _ => None,
            };
            if let Some(input) = input {
                out.extend(self.map(input, v, d));
            }
        }
        if !events
            .iter()
            .any(|e| matches!(e, egui::Event::WindowFocused(false)))
        {
            out.extend(self.modifiers(modifiers, v, d));
        }
        out
    }
}
fn keysym(key: egui::Key) -> Option<(u32, bool)> {
    use egui::Key::*;
    let value = match key {
        ArrowLeft => 0xff51,
        ArrowUp => 0xff52,
        ArrowRight => 0xff53,
        ArrowDown => 0xff54,
        Escape => 0xff1b,
        Tab => 0xff09,
        Backspace => 0xff08,
        Enter => 0xff0d,
        Insert => 0xff63,
        Delete => 0xffff,
        Home => 0xff50,
        End => 0xff57,
        PageUp => 0xff55,
        PageDown => 0xff56,
        F1 => 0xffbe,
        F2 => 0xffbf,
        F3 => 0xffc0,
        F4 => 0xffc1,
        F5 => 0xffc2,
        F6 => 0xffc3,
        F7 => 0xffc4,
        F8 => 0xffc5,
        F9 => 0xffc6,
        F10 => 0xffc7,
        F11 => 0xffc8,
        F12 => 0xffc9,
        _ => {
            let name = key.name();
            if name.len() == 1 {
                return Some((name.to_ascii_lowercase().as_bytes()[0] as u32, true));
            }
            if key == Space {
                return Some((32, true));
            }
            return None;
        }
    };
    Some((value, false))
}
