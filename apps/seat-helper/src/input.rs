//! XTest input. Text uses a spare keycode, never clipboard or an IME commit channel.
use crate::{Result, error, seat::DisplayTarget, x11};
use remotevnc_protocol::{InputEvent, MouseButton};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    thread,
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        xinput::{ConnectionExt as _, Device, DeviceType},
        xkb::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _, ModMask},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
};
static TEXT_TRANSACTION: Mutex<()> = Mutex::new(());
const CORE_KEYBOARD: u16 = 256;

pub fn unicode_keysym(ch: char) -> Result<u32> {
    match ch {
        '\n' | '\r' => Ok(0xff0d),
        '\t' => Ok(0xff09),
        c if c.is_control() => Err(error("unsupported control character in text")),
        c if (c as u32) <= 0xff => Ok(c as u32),
        c => Ok(0x0100_0000 | c as u32),
    }
}
pub struct X11Input {
    connection: RustConnection,
    root: u32,
    pointer_device: u16,
    width: u32,
    height: u32,
    origin_x: u32,
    origin_y: u32,
    keys: BTreeMap<u32, u8>,
    buttons: BTreeSet<u8>,
    guardian: Option<crate::recovery::Client>,
    recovery: crate::recovery::Snapshot,
}
impl X11Input {
    pub fn connect(target: &DisplayTarget) -> Result<Self> {
        let connection = x11::connect(target)?;
        connection.xtest_get_version(2, 2)?.reply()?;
        if !connection.xkb_use_extension(1, 0)?.reply()?.supported {
            return Err(error("XKB unsupported"));
        }
        if connection
            .xinput_xi_query_version(2, 0)?
            .reply()?
            .major_version
            < 2
        {
            return Err(error("XI2 required for safe mouse button ownership"));
        }
        let devices = connection
            .xinput_xi_query_device(Device::ALL_MASTER)?
            .reply()?;
        let mut pointers = devices
            .infos
            .iter()
            .filter(|device| device.type_ == DeviceType::MASTER_POINTER);
        let pointer_device = pointers
            .next()
            .ok_or_else(|| error("no XI2 master pointer"))?
            .deviceid;
        if pointers.next().is_some() {
            return Err(error("multiple XI2 master pointers are unsupported"));
        }
        let root = connection.setup().roots[0].root;
        let geometry = connection.get_geometry(root)?.reply()?;
        if target.origin_x.saturating_add(target.width) > u32::from(geometry.width)
            || target.origin_y.saturating_add(target.height) > u32::from(geometry.height)
        {
            return Err(error("selected output outside root"));
        }
        Ok(Self {
            connection,
            root,
            pointer_device,
            width: target.width,
            height: target.height,
            origin_x: target.origin_x,
            origin_y: target.origin_y,
            keys: BTreeMap::new(),
            buttons: BTreeSet::new(),
            guardian: None,
            recovery: Default::default(),
        })
    }
    pub fn connect_guarded(
        target: &DisplayTarget,
        socket: &std::path::Path,
        deadline: u64,
    ) -> Result<Self> {
        let mut input = Self::connect(target)?;
        input.guardian = Some(crate::recovery::Client::connect(
            &input.connection,
            target,
            socket,
            deadline,
        )?);
        Ok(input)
    }
    pub fn guardian_healthy(&self) -> bool {
        self.guardian.as_ref().is_none_or(|g| g.healthy())
    }
    pub fn renew_guardian(&mut self, deadline: u64) -> Result<()> {
        self.guardian
            .as_mut()
            .ok_or("no input guardian")?
            .renew(deadline)
    }
    pub fn poll_recovery(&mut self) -> Result<()> {
        if self.guardian.is_none() || self.recovery.text.is_some() {
            return Ok(());
        }
        let borrowed: Vec<_> = self
            .recovery
            .keys
            .iter()
            .copied()
            .filter(|code| !self.keys.values().any(|owned| owned == code))
            .collect();
        if borrowed.is_empty() {
            return Ok(());
        }
        for code in borrowed {
            if !crate::recovery::physical_state(&self.connection)?
                .keys
                .contains(&code)
            {
                self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0)?;
                self.recovery.keys.remove(&code);
            }
        }
        self.save_recovery()
    }
    fn save_recovery(&mut self) -> Result<()> {
        if let Some(guardian) = self.guardian.as_mut() {
            guardian.state(&self.recovery)?;
        }
        Ok(())
    }
    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.connection
            .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)?
            .check()?;
        Ok(())
    }
    fn motion(&self, x: u32, y: u32) -> Result<()> {
        if x >= self.width || y >= self.height {
            return Err(error("pointer outside current display"));
        }
        self.fake(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            i16::try_from(self.origin_x.checked_add(x).ok_or("pointer overflow")?)?,
            i16::try_from(self.origin_y.checked_add(y).ok_or("pointer overflow")?)?,
        )
    }
    pub fn apply(&mut self, event: &InputEvent) -> Result<()> {
        self.apply_with_cancel(event, || false)
    }
    pub fn apply_with_cancel(
        &mut self,
        event: &InputEvent,
        cancelled: impl Fn() -> bool,
    ) -> Result<()> {
        if cancelled() || !self.guardian_healthy() {
            return Err(error("input cancelled"));
        }
        self.poll_recovery()?;
        match event {
            InputEvent::PointerMove { x, y } => self.motion(*x, *y)?,
            InputEvent::Button {
                button,
                pressed,
                x,
                y,
            } => {
                self.motion(*x, *y)?;
                let code = match button {
                    MouseButton::Left => 1,
                    MouseButton::Middle => 2,
                    MouseButton::Right => 3,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 9,
                };
                if !pressed && !self.buttons.contains(&code) {
                    return Ok(());
                }
                if *pressed && !self.buttons.contains(&code) {
                    // Core QueryPointer exposes only buttons1..5. XI2 also covers
                    // Back/Forward, so no supported button can be claimed locally-held.
                    let state = self
                        .connection
                        .xinput_xi_query_pointer(self.root, self.pointer_device)?
                        .reply()?;
                    let word = state
                        .buttons
                        .get(usize::from(code) / 32)
                        .ok_or_else(|| error("mouse button state unavailable"))?;
                    if word & (1 << (code % 32)) != 0 {
                        return Err(error("button already held by local input"));
                    }
                }
                if *pressed {
                    self.recovery.buttons.insert(code);
                    self.save_recovery()?;
                }
                if !pressed
                    && self.guardian.is_some()
                    && crate::recovery::physical_state(&self.connection)?
                        .buttons
                        .contains(&code)
                {
                    return Err(error("local button ownership keeps recovery pending"));
                }
                self.fake(
                    if *pressed {
                        xproto::BUTTON_PRESS_EVENT
                    } else {
                        xproto::BUTTON_RELEASE_EVENT
                    },
                    code,
                    0,
                    0,
                )?;
                if *pressed {
                    self.buttons.insert(code);
                } else {
                    self.buttons.remove(&code);
                    self.recovery.buttons.remove(&code);
                    self.save_recovery()?;
                }
            }
            InputEvent::Key { keysym, pressed } => self.key(*keysym, *pressed)?,
            InputEvent::Text { text } => self.text(text, &cancelled)?,
            InputEvent::Scroll {
                delta_x,
                delta_y,
                x,
                y,
            } => {
                if delta_x.unsigned_abs() > 100 || delta_y.unsigned_abs() > 100 {
                    return Err(error("scroll exceeds 100 steps per axis"));
                }
                self.motion(*x, *y)?;
                for (delta, negative, positive) in [(*delta_y, 5, 4), (*delta_x, 7, 6)] {
                    for _ in 0..delta.unsigned_abs() {
                        let button = if delta < 0 { negative } else { positive };
                        let state = self
                            .connection
                            .xinput_xi_query_pointer(self.root, self.pointer_device)?
                            .reply()?;
                        if state
                            .buttons
                            .first()
                            .is_none_or(|word| word & (1 << button) != 0)
                        {
                            return Err(error("scroll button already held by local input"));
                        }
                        self.recovery.buttons.insert(button);
                        self.save_recovery()?;
                        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
                        if self.guardian.is_some()
                            && crate::recovery::physical_state(&self.connection)?
                                .buttons
                                .contains(&button)
                        {
                            return Err(error("local scroll ownership keeps recovery pending"));
                        }
                        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
                        self.recovery.buttons.remove(&button);
                        self.save_recovery()?;
                    }
                }
            }
            InputEvent::ReleaseAll => self.release_all()?,
            InputEvent::DisplayAck => {}
        }
        self.connection.flush()?;
        Ok(())
    }
    fn key(&mut self, keysym: u32, pressed: bool) -> Result<()> {
        if !pressed {
            if let Some(code) = self.keys.get(&keysym).copied() {
                if self.guardian.is_some()
                    && crate::recovery::physical_state(&self.connection)?
                        .keys
                        .contains(&code)
                {
                    return Err(error("local key ownership keeps recovery pending"));
                }
                self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0)?;
                self.keys.remove(&keysym);
                self.recovery.keys.remove(&code);
                self.save_recovery()?;
            }
            return Ok(());
        }
        if self.keys.contains_key(&keysym) {
            return Ok(());
        }
        let setup = self.connection.setup();
        let map = self
            .connection
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)?
            .reply()?;
        let state = self.connection.xkb_get_state(CORE_KEYBOARD)?.reply()?;
        let group = usize::from(u8::from(state.group));
        // Core mappings expose XKB group pairs. Preserve current physical/remote
        // modifiers: shortcuts carry their own Shift/Control/Alt key events.
        let code = map
            .keysyms
            .chunks(usize::from(map.keysyms_per_keycode))
            .enumerate()
            .find_map(|(index, syms)| {
                let pair = if group * 2 < syms.len() { group * 2 } else { 0 };
                syms[pair..syms.len().min(pair + 2)]
                    .contains(&keysym)
                    .then_some(setup.min_keycode + index as u8)
            })
            .ok_or_else(|| error("keysym unavailable in active XKB group"))?;
        let down = self.connection.query_keymap()?.reply()?.keys;
        if down[usize::from(code) / 8] & (1 << (code % 8)) != 0 {
            return Err(error("key already held by local input"));
        }
        self.recovery.keys.insert(code);
        self.save_recovery()?;
        self.fake(xproto::KEY_PRESS_EVENT, code, 0, 0)?;
        self.keys.insert(keysym, code);
        Ok(())
    }
    fn text(&mut self, text: &str, cancelled: &impl Fn() -> bool) -> Result<()> {
        if text.len() > remotevnc_protocol::TEXT_LIMIT {
            return Err(error("text too large"));
        }
        let _serial = TEXT_TRANSACTION
            .lock()
            .map_err(|_| error("text transaction poisoned"))?;
        let c = &self.connection;
        let fake = |kind: u8, code: u8| -> Result<()> {
            c.xtest_fake_input(kind, code, 0, self.root, 0, 0, 0)?
                .check()?;
            Ok(())
        };
        let state = c.xkb_get_state(CORE_KEYBOARD)?.reply()?;
        if u16::from(state.latched_mods) != 0 || state.latched_group != 0 || state.base_group != 0 {
            return Err(error(
                "text injection unsupported with latched modifiers or momentary group",
            ));
        }
        let setup = c.setup();
        let map = c
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)?
            .reply()?;
        let per = usize::from(map.keysyms_per_keycode);
        let down = c.query_keymap()?.reply()?.keys;
        let spare = map
            .keysyms
            .chunks(per)
            .enumerate()
            .rev()
            .find(|(index, syms)| {
                let code = usize::from(setup.min_keycode) + index;
                syms.iter().all(|sym| *sym == 0) && down[code / 8] & (1 << (code % 8)) == 0
            })
            .map(|(i, syms)| (setup.min_keycode + i as u8, syms.to_vec()))
            .ok_or_else(|| error("no unused X11 keycode for Unicode text"))?;
        let modifiers = c.get_modifier_mapping()?.reply()?.keycodes;
        let held: BTreeSet<u8> = modifiers
            .into_iter()
            .filter(|code| *code != 0 && down[usize::from(*code) / 8] & (1 << (*code % 8)) != 0)
            .collect();
        let physical = crate::recovery::physical_state(c)?;
        if physical.keys.contains(&spare.0) {
            return Err(error("spare keycode held by a physical source"));
        }
        let local_modifiers: BTreeSet<u8> = held
            .iter()
            .copied()
            .filter(|code| !self.keys.values().any(|v| v == code))
            .collect();
        if local_modifiers
            .iter()
            .any(|code| !physical.keys.contains(code))
        {
            return Err(error("text cannot borrow an unowned shared XTEST modifier"));
        }
        self.recovery.keys.insert(spare.0);
        self.recovery.text = Some(crate::recovery::TextRestore {
            code: spare.0,
            original: spare.1.clone(),
            expected: vec![spare.1.clone()],
            local_modifiers,
            locked_mods: u16::from(state.locked_mods) as u8,
            locked_group: u8::from(state.locked_group),
        });
        if let Some(g) = self.guardian.as_mut() {
            g.state(&self.recovery)?;
        }
        let operation = (|| -> Result<()> {
            for code in &held {
                fake(xproto::KEY_RELEASE_EVENT, *code)?;
            }
            c.xkb_latch_lock_state(
                CORE_KEYBOARD,
                ModMask::from(255u8),
                ModMask::from(0u8),
                true,
                xkb::Group::M1,
                ModMask::from(0u8),
                false,
                0,
            )?
            .check()?;
            for ch in text.chars() {
                if cancelled() || self.guardian.as_ref().is_some_and(|g| !g.healthy()) {
                    return Err(error("text transaction cancelled"));
                }
                let symbol = unicode_keysym(ch)?;
                let text_state = self.recovery.text.as_mut().unwrap();
                let prior = text_state.expected.last().unwrap().clone();
                text_state.expected = vec![prior, vec![symbol; per]];
                if let Some(g) = self.guardian.as_mut() {
                    g.state(&self.recovery)?;
                }
                if crate::recovery::physical_state(c)?.keys.contains(&spare.0) {
                    return Err(error("local spare-key mapping keeps recovery pending"));
                }
                c.change_keyboard_mapping(1, spare.0, map.keysyms_per_keycode, &vec![symbol; per])?
                    .check()?;
                // X11 has no client-consumption acknowledgement. Give toolkit keymap
                // invalidation a turn before sending, and delivery a turn before restore.
                thread::sleep(Duration::from_millis(20));
                if cancelled() || self.guardian.as_ref().is_some_and(|g| !g.healthy()) {
                    return Err(error("text transaction cancelled"));
                }
                if crate::recovery::physical_state(c)?.keys.contains(&spare.0) {
                    return Err(error("local spare-key ownership keeps recovery pending"));
                }
                fake(xproto::KEY_PRESS_EVENT, spare.0)?;
                if crate::recovery::physical_state(c)?.keys.contains(&spare.0) {
                    return Err(error("local spare-key ownership keeps recovery pending"));
                }
                fake(xproto::KEY_RELEASE_EVENT, spare.0)?;
                thread::sleep(Duration::from_millis(20));
            }
            Ok(())
        })();
        // Cleanup is attempted in full even if one restoration request fails.
        let key_release = (|| -> Result<()> {
            if crate::recovery::physical_state(c)?.keys.contains(&spare.0) {
                return Err(error("local spare-key ownership keeps recovery pending"));
            }
            fake(xproto::KEY_RELEASE_EVENT, spare.0)
        })();
        let mapping_restore = (|| -> Result<()> {
            let current = c.get_keyboard_mapping(spare.0, 1)?.reply()?;
            let text_state = self.recovery.text.as_ref().unwrap();
            if !crate::recovery::mapping_owned(&current.keysyms, &text_state.expected) {
                return Ok(()); // Preserve a mapping changed by another local owner.
            }
            if crate::recovery::physical_state(c)?.keys.contains(&spare.0) {
                return Err(error("local spare-key mapping keeps recovery pending"));
            }
            c.change_keyboard_mapping(1, spare.0, map.keysyms_per_keycode, &spare.1)?
                .check()?;
            Ok(())
        })();
        let physical_after = crate::recovery::physical_state(c);
        let mut modifier_restore = if physical_after.is_err() {
            Err(error("physical modifier state unavailable"))
        } else {
            Ok(())
        };
        for code in &held {
            if !self.keys.values().any(|v| v == code)
                && !physical_after.as_ref().is_ok_and(|p| p.keys.contains(code))
            {
                continue;
            }
            if !self.keys.values().any(|v| v == code) {
                self.recovery.keys.insert(*code);
                if let Some(g) = self.guardian.as_mut()
                    && let Err(e) = g.state(&self.recovery)
                {
                    modifier_restore = Err(e);
                }
            }
            if !self.keys.values().any(|v| v == code)
                && !crate::recovery::physical_state(c).is_ok_and(|p| p.keys.contains(code))
            {
                continue;
            }
            if let Err(e) = fake(xproto::KEY_PRESS_EVENT, *code) {
                modifier_restore = Err(e);
            }
        }
        let lock_restore = (|| -> Result<()> {
            let current = c.xkb_get_state(CORE_KEYBOARD)?.reply()?;
            if u16::from(current.locked_mods) != 0 || u8::from(current.locked_group) != 0 {
                return Ok(());
            }
            c.xkb_latch_lock_state(
                CORE_KEYBOARD,
                ModMask::from(255u8),
                state.locked_mods,
                true,
                state.locked_group,
                ModMask::from(0u8),
                false,
                0,
            )?
            .check()?;
            Ok(())
        })();
        if key_release.is_ok()
            && mapping_restore.is_ok()
            && modifier_restore.is_ok()
            && lock_restore.is_ok()
        {
            self.recovery.text = None;
            self.recovery.keys.remove(&spare.0);
            if let Some(g) = self.guardian.as_mut() {
                let _ = g.state(&self.recovery);
            }
        }
        operation
            .and(key_release)
            .and(mapping_restore)
            .and(modifier_restore)
            .and(lock_restore)
    }
    pub fn release_all(&mut self) -> Result<()> {
        if self.guardian.is_some() {
            let complete = crate::recovery::restore(&self.connection, &mut self.recovery)?;
            let _ = self.save_recovery();
            if complete {
                self.keys.clear();
                self.buttons.clear();
                return Ok(());
            }
            return Err(error("local input ownership keeps recovery pending"));
        }
        let mut result = Ok(());
        for code in self.keys.values().copied().collect::<Vec<_>>() {
            if let Err(e) = self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0) {
                result = Err(e);
            }
        }
        for button in self.buttons.iter().copied().collect::<Vec<_>>() {
            if let Err(e) = self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0) {
                result = Err(e);
            }
        }
        if result.is_ok() {
            self.keys.clear();
            self.buttons.clear();
        }
        result
    }
}
impl Drop for X11Input {
    fn drop(&mut self) {
        let _ = self.release_all();
        // Retain the last surviving recovery connection while a physical
        // source still owns overlapping state. The caller stops capture first.
        while self.guardian.is_some()
            && (!self.recovery.keys.is_empty()
                || !self.recovery.buttons.is_empty()
                || self.recovery.text.is_some())
        {
            if crate::recovery::restore(&self.connection, &mut self.recovery).is_ok_and(|done| done)
            {
                break;
            }
            if self
                .connection
                .get_input_focus()
                .ok()
                .and_then(|v| v.reply().ok())
                .is_none()
            {
                break; // That exact X server is gone; never reconnect for cleanup.
            }
            thread::sleep(Duration::from_millis(10));
        }
        if let Some(guardian) = self.guardian.as_mut() {
            let _ = guardian.quiesce(&self.connection);
        }
    }
}
