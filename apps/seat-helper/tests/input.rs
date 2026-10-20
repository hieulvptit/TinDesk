use remotevnc_seat_helper::input::unicode_keysym;
#[test]
fn unicode_keysyms_preserve_scalar_values() {
    assert_eq!(unicode_keysym('a').unwrap(), 0x61);
    assert_eq!(unicode_keysym('ế').unwrap(), 0x01001ebf);
    assert_eq!(unicode_keysym('😀').unwrap(), 0x0101f600);
    assert!(unicode_keysym('\0').is_err());
}

fn target() -> remotevnc_seat_helper::seat::DisplayTarget {
    use remotevnc_seat_helper::seat::*;
    DisplayTarget {
        seat: "test-only".into(),
        display: std::env::var("DISPLAY").unwrap(),
        authority: std::env::var_os("XAUTHORITY").unwrap().into(),
        identity: ServerIdentity {
            process_start: "fixture".into(),
            socket_inode: 0,
            authority_inode: 0,
            authority_modified_ns: 0,
            connection_epoch: 0,
        },
        output: "screen".into(),
        origin_x: 0,
        origin_y: 0,
        width: 800,
        height: 600,
    }
}

#[test]
#[ignore = "requires scripts/test-x11.sh Xvfb fixture"]
fn x11_releases_keys_buttons_and_restores_keymap_after_error() {
    use remotevnc_protocol::*;
    use remotevnc_seat_helper::{input::X11Input, x11};
    use x11rb::{
        connection::Connection,
        protocol::{
            xkb::{ConnectionExt as _, ID},
            xproto::ConnectionExt,
        },
    };
    let t = target();
    let c = x11::connect(&t).unwrap();
    c.xkb_use_extension(1, 0).unwrap().reply().unwrap();
    let min = c.setup().min_keycode;
    let count = c.setup().max_keycode - min + 1;
    let before = c.get_keyboard_mapping(min, count).unwrap().reply().unwrap();
    let mut input = X11Input::connect(&t).unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe1,
            pressed: true,
        })
        .unwrap();
    input
        .apply(&InputEvent::Button {
            button: MouseButton::Left,
            pressed: true,
            x: 50,
            y: 50,
        })
        .unwrap();
    assert!(
        input
            .apply(&InputEvent::Text {
                text: "ế\0".into()
            })
            .is_err()
    );
    let after = c.get_keyboard_mapping(min, count).unwrap().reply().unwrap();
    assert_eq!(before.keysyms, after.keysyms);
    // The previously held remote shift must survive a failed text transaction.
    assert!(
        u16::from(
            c.xkb_get_state(ID::USE_CORE_KBD.into())
                .unwrap()
                .reply()
                .unwrap()
                .mods
        ) & 1
            != 0
    );
    drop(input); // disconnect cleanup must release both tracked key and button.
    let keys = c.query_keymap().unwrap().reply().unwrap();
    assert!(keys.keys.iter().all(|v| *v == 0));
    let pointer = c
        .query_pointer(c.setup().roots[0].root)
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(u16::from(pointer.mask) & 0x1f00, 0);
}

#[test]
#[ignore = "requires scripts/test-x11.sh GTK receiver"]
fn gtk_receives_exact_unicode_without_duplicate_commit() {
    use remotevnc_protocol::InputEvent;
    use remotevnc_seat_helper::{input::X11Input, x11};
    use x11rb::protocol::xkb::ConnectionExt as _;
    let t = target();
    let c = x11::connect(&t).unwrap();
    c.xkb_use_extension(1, 0).unwrap().reply().unwrap();
    let mut input = X11Input::connect(&t).unwrap();
    // Text must remain exact with Caps Lock on and a shortcut Shift held.
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe5,
            pressed: true,
        })
        .unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe5,
            pressed: false,
        })
        .unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe1,
            pressed: true,
        })
        .unwrap();
    let before = c.xkb_get_state(256).unwrap().reply().unwrap();
    input
        .apply(&InputEvent::Text {
            text: "Tiếng Việt|é|e\u{301}|中文|😀".into(),
        })
        .unwrap();
    let after = c.xkb_get_state(256).unwrap().reply().unwrap();
    assert_eq!(before.locked_mods, after.locked_mods);
    assert_eq!(before.mods, after.mods);
    input.release_all().unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe5,
            pressed: true,
        })
        .unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0xffe5,
            pressed: false,
        })
        .unwrap();
}

#[test]
#[ignore = "requires scripts/test-x11.sh Xvfb fixture"]
fn cancelled_long_text_restores_keymap_and_input_state_promptly() {
    use remotevnc_protocol::InputEvent;
    use remotevnc_seat_helper::{input::X11Input, x11};
    use std::{cell::Cell, time::Instant};
    use x11rb::{connection::Connection, protocol::xproto::ConnectionExt};
    let t = target();
    let c = x11::connect(&t).unwrap();
    let min = c.setup().min_keycode;
    let count = c.setup().max_keycode - min + 1;
    let before = c
        .get_keyboard_mapping(min, count)
        .unwrap()
        .reply()
        .unwrap()
        .keysyms;
    let mut input = X11Input::connect(&t).unwrap();
    let started = Instant::now();
    let checks = Cell::new(0);
    let result = input.apply_with_cancel(
        &InputEvent::Text {
            text: "ế".repeat(5000),
        },
        || {
            checks.set(checks.get() + 1);
            checks.get() > 3
        },
    );
    assert!(result.is_err());
    assert!(started.elapsed().as_secs() < 1);
    assert_eq!(
        before,
        c.get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms
    );
    assert!(
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|b| *b == 0)
    );
}

#[test]
#[ignore = "requires scripts/test-x11.sh Xvfb fixture"]
fn remote_cleanup_does_not_release_unowned_local_input() {
    use remotevnc_protocol::{InputEvent, MouseButton};
    use remotevnc_seat_helper::{input::X11Input, x11};
    use x11rb::{
        connection::Connection,
        protocol::{
            xproto::{self, ConnectionExt},
            xtest::ConnectionExt as _,
        },
    };
    let t = target();
    let c = x11::connect(&t).unwrap();
    let root = c.setup().roots[0].root;
    let map = c
        .get_keyboard_mapping(
            c.setup().min_keycode,
            c.setup().max_keycode - c.setup().min_keycode + 1,
        )
        .unwrap()
        .reply()
        .unwrap();
    let code = c.setup().min_keycode
        + map
            .keysyms
            .chunks(usize::from(map.keysyms_per_keycode))
            .position(|syms| syms[0] == 0x61)
            .unwrap() as u8;
    c.xtest_fake_input(xproto::KEY_PRESS_EVENT, code, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    c.xtest_fake_input(xproto::BUTTON_PRESS_EVENT, 1, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    let mut input = X11Input::connect(&t).unwrap();
    input
        .apply(&InputEvent::Button {
            button: MouseButton::Left,
            pressed: false,
            x: 1,
            y: 1,
        })
        .unwrap();
    assert!(
        input
            .apply(&InputEvent::Key {
                keysym: 0x61,
                pressed: true
            })
            .is_err()
    );
    assert!(
        input
            .apply(&InputEvent::Button {
                button: MouseButton::Left,
                pressed: true,
                x: 1,
                y: 1
            })
            .is_err()
    );
    assert!(
        input
            .apply(&InputEvent::Text { text: "\0".into() })
            .is_err()
    );
    drop(input);
    let keys = c.query_keymap().unwrap().reply().unwrap().keys;
    assert_ne!(keys[usize::from(code) / 8] & (1 << (code % 8)), 0);
    assert_ne!(
        u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100,
        0
    );
    c.xtest_fake_input(xproto::KEY_RELEASE_EVENT, code, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    c.xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, 1, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
}

#[test]
#[ignore = "requires scripts/test-x11.sh Xvfb fixture"]
fn remote_cleanup_preserves_local_back_and_forward_buttons() {
    use remotevnc_protocol::{InputEvent, MouseButton};
    use remotevnc_seat_helper::{input::X11Input, x11};
    use x11rb::{
        connection::Connection,
        protocol::{xinput::ConnectionExt as _, xproto, xtest::ConnectionExt as _},
    };
    let t = target();
    let c = x11::connect(&t).unwrap();
    c.xinput_xi_query_version(2, 0).unwrap().reply().unwrap();
    let root = c.setup().roots[0].root;
    let pointer = c
        .xinput_xi_query_device(x11rb::protocol::xinput::Device::ALL_MASTER)
        .unwrap()
        .reply()
        .unwrap()
        .infos
        .into_iter()
        .find(|device| device.type_ == x11rb::protocol::xinput::DeviceType::MASTER_POINTER)
        .unwrap()
        .deviceid;
    for (button, code) in [(MouseButton::Back, 8u8), (MouseButton::Forward, 9u8)] {
        c.xtest_fake_input(xproto::BUTTON_PRESS_EVENT, code, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        let mut input = X11Input::connect(&t).unwrap();
        input
            .apply(&InputEvent::Button {
                button,
                pressed: false,
                x: 1,
                y: 1,
            })
            .unwrap();
        let result = input.apply(&InputEvent::Button {
            button,
            pressed: true,
            x: 1,
            y: 1,
        });
        input.release_all().unwrap();
        drop(input);
        let after = c
            .xinput_xi_query_pointer(root, pointer)
            .unwrap()
            .reply()
            .unwrap();
        c.xtest_fake_input(xproto::BUTTON_RELEASE_EVENT, code, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        assert!(
            result.is_err(),
            "remote must reject locally held button {code}"
        );
        assert_ne!(
            after.buttons[0] & (1 << code),
            0,
            "cleanup released local button {code}"
        );
    }
    // Back/Forward remain supported when the helper can acquire ownership.
    for button in [MouseButton::Back, MouseButton::Forward] {
        let mut input = X11Input::connect(&t).unwrap();
        input
            .apply(&InputEvent::Button {
                button,
                pressed: true,
                x: 1,
                y: 1,
            })
            .unwrap();
        input.release_all().unwrap();
        assert_eq!(
            c.xinput_xi_query_pointer(root, pointer)
                .unwrap()
                .reply()
                .unwrap()
                .buttons[0]
                & ((1 << 8) | (1 << 9)),
            0
        );
    }
}
