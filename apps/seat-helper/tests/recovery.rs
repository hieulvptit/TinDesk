//! Real X server/process fixtures. Never used by the production display monitor.
#![cfg(target_os = "linux")]
use remotevnc_protocol::{InputEvent, MouseButton};
use remotevnc_seat_helper::{input::X11Input, recovery, seat::*};
use std::{
    os::unix::net::UnixListener,
    process::{Child, Command},
    time::{Duration, Instant},
};
use x11rb::{connection::Connection, protocol::xproto::ConnectionExt as _};
fn target() -> DisplayTarget {
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
#[ignore = "subprocess entry point for Xvfb recovery fixtures"]
fn recovery_child() {
    let Ok(role) = std::env::var("REMOTEVNC_RECOVERY_TEST_ROLE") else {
        return;
    };
    let socket = std::env::var("REMOTEVNC_RECOVERY_TEST_SOCKET").unwrap();
    if role == "guardian" {
        let listener = UnixListener::bind(socket).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        let _ = recovery::serve(&mut stream);
    } else {
        let mut input = X11Input::connect_guarded(
            &target(),
            socket.as_ref(),
            recovery::boot_time_ms().unwrap() + 60_000,
        )
        .unwrap();
        if role == "text" || role == "text_short" {
            input
                .apply(&InputEvent::Text {
                    text: "ế".repeat(if role == "text_short" { 1 } else { 1000 }),
                })
                .unwrap();
        } else {
            input
                .apply(&InputEvent::Key {
                    keysym: 0x61,
                    pressed: true,
                })
                .unwrap();
            input
                .apply(&InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                    x: 10,
                    y: 10,
                })
                .unwrap();
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end {
                if input
                    .apply(&InputEvent::PointerMove { x: 10, y: 10 })
                    .is_err()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}
struct Fixture {
    guardian: Child,
    helper: Child,
    socket: std::path::PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.helper.kill();
        let _ = self.helper.wait();
        let _ = self.guardian.kill();
        let _ = self.guardian.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}
fn launch(role: &str, socket: &std::path::Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "recovery_child", "--ignored", "--nocapture"])
        .env("REMOTEVNC_RECOVERY_TEST_ROLE", role)
        .env("REMOTEVNC_RECOVERY_TEST_SOCKET", socket)
        .spawn()
        .unwrap()
}
fn fixture(role: &str) -> Fixture {
    let socket =
        std::env::temp_dir().join(format!("remotevnc-recovery-{}.sock", uuid::Uuid::new_v4()));
    let guardian = launch("guardian", &socket);
    wait(|| socket.exists());
    let helper = launch(role, &socket);
    Fixture {
        guardian,
        helper,
        socket,
    }
}
fn wait(mut condition: impl FnMut() -> bool) {
    let started = Instant::now();
    while !condition() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "native recovery deadline"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
#[ignore = "requires isolated Xvfb"]
fn guardian_releases_real_keys_and_buttons_after_helper_sigkill() {
    let c = remotevnc_seat_helper::x11::connect(&target()).unwrap();
    let root = c.setup().roots[0].root;
    let mut f = fixture("held");
    wait(|| {
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .any(|b| *b != 0)
            && u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100 != 0
    });
    f.helper.kill().unwrap();
    f.helper.wait().unwrap();
    wait(|| {
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|b| *b == 0)
            && u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100 == 0
    });
    assert!(f.guardian.wait().unwrap().success());
}
#[test]
#[ignore = "requires isolated Xvfb"]
fn guardian_restores_unicode_mapping_after_helper_sigkill() {
    let c = remotevnc_seat_helper::x11::connect(&target()).unwrap();
    let min = c.setup().min_keycode;
    let count = c.setup().max_keycode - min + 1;
    let before = c
        .get_keyboard_mapping(min, count)
        .unwrap()
        .reply()
        .unwrap()
        .keysyms;
    let mut f = fixture("text");
    wait(|| {
        c.get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms
            != before
    });
    f.helper.kill().unwrap();
    f.helper.wait().unwrap();
    wait(|| {
        c.get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms
            == before
    });
    assert!(f.guardian.wait().unwrap().success());
}

#[test]
#[ignore = "requires isolated Xvfb with separate physical source devices"]
fn guardian_preserves_physical_ownership_and_finishes_after_source_release() {
    physical_overlap(false);
}
#[test]
#[ignore = "requires isolated Xvfb with separate physical source devices"]
fn surviving_helper_keeps_recovery_after_guardian_death() {
    physical_overlap(true);
}
fn physical_overlap(kill_guardian: bool) {
    use x11rb::protocol::xinput::{
        ConnectionExt as _, Device, DeviceType, InputClass, InputStateData,
    };
    use x11rb::protocol::xtest::ConnectionExt as _;
    let c = remotevnc_seat_helper::x11::connect(&target()).unwrap();
    c.xinput_xi_query_version(2, 0).unwrap().reply().unwrap();
    let root = c.setup().roots[0].root;
    let devices = c
        .xinput_xi_query_device(Device::ALL)
        .unwrap()
        .reply()
        .unwrap()
        .infos;
    let keyboard = devices
        .iter()
        .find(|d| d.type_ == DeviceType::SLAVE_KEYBOARD && d.name == b"Xvfb keyboard")
        .unwrap()
        .deviceid as u8;
    let pointer = devices
        .iter()
        .find(|d| d.type_ == DeviceType::SLAVE_POINTER && d.name == b"Xvfb mouse")
        .unwrap()
        .deviceid as u8;
    let key_type = c
        .xinput_open_device(keyboard)
        .unwrap()
        .reply()
        .unwrap()
        .class_info
        .iter()
        .find(|v| v.class_id == InputClass::KEY)
        .unwrap()
        .event_type_base;
    let button_type = c
        .xinput_open_device(pointer)
        .unwrap()
        .reply()
        .unwrap()
        .class_info
        .iter()
        .find(|v| v.class_id == InputClass::BUTTON)
        .unwrap()
        .event_type_base;
    let min = c.setup().min_keycode;
    let map = c
        .get_keyboard_mapping(min, c.setup().max_keycode - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let key = |sym| {
        min + map
            .keysyms
            .chunks(map.keysyms_per_keycode as usize)
            .position(|v| v.contains(&sym))
            .unwrap() as u8
    };
    let a = key(0x61);
    let b = key(0x62);
    let fake = |kind, detail, device| {
        c.xtest_fake_input(kind, detail, 0, root, 0, 0, device)
            .unwrap()
            .check()
            .unwrap()
    };
    // A pre-existing local key must never enter the helper's ownership set.
    fake(key_type, b, keyboard);
    let mut f = fixture("held");
    wait(|| {
        let keys = c.query_keymap().unwrap().reply().unwrap().keys;
        keys[a as usize / 8] & (1 << (a % 8)) != 0
            && u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100 != 0
    });
    fake(key_type, a, keyboard);
    fake(button_type, 1, pointer);
    let physical_down = c
        .xinput_query_device_state(keyboard)
        .unwrap()
        .reply()
        .unwrap()
        .classes
        .into_iter()
        .any(|v| matches!(v.data,InputStateData::Key(k) if k.keys[a as usize/8]&(1<<(a%8))!=0));
    assert!(
        physical_down,
        "fixture must use physical source, not shared XTEST"
    );
    if kill_guardian {
        f.guardian.kill().unwrap();
        f.guardian.wait().unwrap();
    } else {
        f.helper.kill().unwrap();
        f.helper.wait().unwrap();
    }
    std::thread::sleep(Duration::from_millis(100));
    let survivor_pending = if kill_guardian {
        f.helper.try_wait().unwrap().is_none()
    } else {
        f.guardian.try_wait().unwrap().is_none()
    };
    let keys = c.query_keymap().unwrap().reply().unwrap().keys;
    assert_ne!(keys[a as usize / 8] & (1 << (a % 8)), 0);
    assert_ne!(keys[b as usize / 8] & (1 << (b % 8)), 0);
    assert_ne!(
        u16::from(c.query_pointer(root).unwrap().reply().unwrap().mask) & 0x100,
        0
    );
    fake(key_type + 1, a, keyboard);
    fake(button_type + 1, 1, pointer);
    wait(|| {
        if kill_guardian {
            f.helper.try_wait().unwrap().is_some()
        } else {
            f.guardian.try_wait().unwrap().is_some()
        }
    });
    let keys = c.query_keymap().unwrap().reply().unwrap().keys;
    assert_eq!(keys[a as usize / 8] & (1 << (a % 8)), 0);
    assert_ne!(keys[b as usize / 8] & (1 << (b % 8)), 0);
    fake(key_type + 1, b, keyboard);
    assert!(
        survivor_pending,
        "surviving recovery owner must retain pending physical overlap"
    );
}

#[test]
#[ignore = "requires isolated Xvfb"]
fn guardian_death_makes_helper_fail_closed_and_release() {
    let c = remotevnc_seat_helper::x11::connect(&target()).unwrap();
    let mut f = fixture("held");
    wait(|| {
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .any(|b| *b != 0)
    });
    f.guardian.kill().unwrap();
    f.guardian.wait().unwrap();
    wait(|| f.helper.try_wait().unwrap().is_some());
    assert!(f.helper.wait().unwrap().success());
    assert!(
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|b| *b == 0)
    );
    assert_eq!(
        u16::from(
            c.query_pointer(c.setup().roots[0].root)
                .unwrap()
                .reply()
                .unwrap()
                .mask
        ) & 0x100,
        0
    );
}

#[test]
#[ignore = "requires isolated Xvfb; resets only verified Xvfb process"]
fn guardian_never_restores_old_ownership_into_reset_xserver() {
    use std::os::fd::AsRawFd;
    use x11rb::protocol::{xproto, xtest::ConnectionExt as _};
    let t = target();
    let c = remotevnc_seat_helper::x11::connect(&t).unwrap();
    let probe = std::os::unix::net::UnixStream::connect(t.socket_path().unwrap()).unwrap();
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of_val(&cred) as libc::socklen_t;
    assert_eq!(
        unsafe {
            libc::getsockopt(
                probe.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut size,
            )
        },
        0
    );
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{}/comm", cred.pid))
            .unwrap()
            .trim(),
        "Xvfb",
        "never reset a physical server"
    );
    drop(probe);
    let mut f = fixture("held");
    wait(|| {
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .any(|v| *v != 0)
    });
    assert_eq!(
        unsafe { libc::kill(f.helper.id() as i32, libc::SIGSTOP) },
        0
    );
    assert_eq!(unsafe { libc::kill(cred.pid, libc::SIGHUP) }, 0);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        unsafe { libc::kill(cred.pid, 0) },
        0,
        "same Xvfb process must survive reset"
    );
    let fresh = remotevnc_seat_helper::x11::connect(&t).unwrap();
    let min = fresh.setup().min_keycode;
    let map = fresh
        .get_keyboard_mapping(min, fresh.setup().max_keycode - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let code = min
        + map
            .keysyms
            .chunks(map.keysyms_per_keycode as usize)
            .position(|v| v.contains(&0x61))
            .unwrap() as u8;
    let root = fresh.setup().roots[0].root;
    fresh
        .xtest_fake_input(xproto::KEY_PRESS_EVENT, code, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    f.helper.kill().unwrap();
    f.helper.wait().unwrap();
    wait(|| f.guardian.try_wait().unwrap().is_some());
    let keys = fresh.query_keymap().unwrap().reply().unwrap().keys;
    fresh
        .xtest_fake_input(xproto::KEY_RELEASE_EVENT, code, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    assert_ne!(
        keys[code as usize / 8] & (1 << (code % 8)),
        0,
        "old guardian must not reconnect and release replacement-server input"
    );
}

#[test]
#[ignore = "requires isolated Xvfb"]
fn graceful_input_cleanup_releases_before_guardian_eof() {
    let t = target();
    let c = remotevnc_seat_helper::x11::connect(&t).unwrap();
    let socket =
        std::env::temp_dir().join(format!("remotevnc-normal-{}.sock", uuid::Uuid::new_v4()));
    let mut guardian = launch("guardian", &socket);
    wait(|| socket.exists());
    let mut input =
        X11Input::connect_guarded(&t, &socket, recovery::boot_time_ms().unwrap() + 60_000).unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0x61,
            pressed: true,
        })
        .unwrap();
    drop(input);
    wait(|| guardian.try_wait().unwrap().is_some());
    assert!(guardian.wait().unwrap().success());
    assert!(
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|v| *v == 0)
    );
    std::fs::remove_file(socket).unwrap();
}

#[test]
#[ignore = "requires isolated Xvfb"]
fn guardian_enforces_absolute_deadline_without_helper_traffic_extension() {
    let t = target();
    let c = remotevnc_seat_helper::x11::connect(&t).unwrap();
    let socket =
        std::env::temp_dir().join(format!("remotevnc-deadline-{}.sock", uuid::Uuid::new_v4()));
    let mut guardian = launch("guardian", &socket);
    wait(|| socket.exists());
    let deadline = recovery::boot_time_ms().unwrap() + 200;
    let mut input = X11Input::connect_guarded(&t, &socket, deadline).unwrap();
    input
        .apply(&InputEvent::Key {
            keysym: 0x61,
            pressed: true,
        })
        .unwrap();
    while recovery::boot_time_ms().unwrap() < deadline {
        let _ = input.apply(&InputEvent::PointerMove { x: 20, y: 20 });
        std::thread::sleep(Duration::from_millis(5));
    }
    wait(|| {
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|v| *v == 0)
    });
    assert!(!input.guardian_healthy());
    drop(input);
    wait(|| guardian.try_wait().unwrap().is_some());
    std::fs::remove_file(socket).unwrap();
}

#[test]
#[ignore = "requires isolated Xvfb with physical keyboard source"]
fn unicode_recovery_tracks_restored_physical_modifier_until_release() {
    physical_modifier(true);
}
#[test]
#[ignore = "requires isolated Xvfb with physical keyboard source"]
fn normal_unicode_tracks_restored_physical_modifier_until_release() {
    physical_modifier(false);
}
fn physical_modifier(kill_helper: bool) {
    use x11rb::protocol::{
        xinput::{ConnectionExt as _, Device, DeviceType, InputClass, InputStateData},
        xproto,
        xtest::ConnectionExt as _,
    };
    let c = remotevnc_seat_helper::x11::connect(&target()).unwrap();
    c.xinput_xi_query_version(2, 0).unwrap().reply().unwrap();
    let devices = c
        .xinput_xi_query_device(Device::ALL)
        .unwrap()
        .reply()
        .unwrap()
        .infos;
    let keyboard = devices
        .iter()
        .find(|d| d.type_ == DeviceType::SLAVE_KEYBOARD && d.name == b"Xvfb keyboard")
        .unwrap()
        .deviceid as u8;
    let xtest = devices
        .iter()
        .find(|d| d.name == b"Virtual core XTEST keyboard")
        .unwrap()
        .deviceid as u8;
    let key_type = c
        .xinput_open_device(keyboard)
        .unwrap()
        .reply()
        .unwrap()
        .class_info
        .iter()
        .find(|v| v.class_id == InputClass::KEY)
        .unwrap()
        .event_type_base;
    let min = c.setup().min_keycode;
    let count = c.setup().max_keycode - min + 1;
    let map = c.get_keyboard_mapping(min, count).unwrap().reply().unwrap();
    let shift = min
        + map
            .keysyms
            .chunks(map.keysyms_per_keycode as usize)
            .position(|v| v.contains(&0xffe1))
            .unwrap() as u8;
    let root = c.setup().roots[0].root;
    c.xtest_fake_input(key_type, shift, 0, root, 0, 0, keyboard)
        .unwrap()
        .check()
        .unwrap();
    let mut f = fixture(if kill_helper { "text" } else { "text_short" });
    wait(|| {
        c.get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms
            != map.keysyms
    });
    if kill_helper {
        f.helper.kill().unwrap();
        f.helper.wait().unwrap();
    }
    wait(|| {
        let keys = c.query_keymap().unwrap().reply().unwrap().keys;
        keys[shift as usize / 8] & (1 << (shift % 8)) != 0
    });
    std::thread::sleep(Duration::from_millis(50));
    let pending = f.guardian.try_wait().unwrap().is_none();
    c.xtest_fake_input(key_type + 1, shift, 0, root, 0, 0, keyboard)
        .unwrap()
        .check()
        .unwrap();
    wait(|| f.guardian.try_wait().unwrap().is_some());
    let residual=c.xinput_query_device_state(xtest).unwrap().reply().unwrap().classes.into_iter().any(|s|matches!(s.data,InputStateData::Key(k) if k.keys[shift as usize/8]&(1<<(shift%8))!=0));
    c.xtest_fake_input(xproto::KEY_RELEASE_EVENT, shift, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    assert!(
        pending,
        "guardian must retain ownership of its physical-modifier restoration"
    );
    assert!(
        !residual,
        "restored modifier must not remain held in XTEST source"
    );
}

#[test]
#[ignore = "requires isolated Xvfb"]
fn write_ahead_mapping_accepts_before_after_and_preserves_unrelated_changes() {
    use std::io::{Read, Write};
    let t = target();
    let c = remotevnc_seat_helper::x11::connect(&t).unwrap();
    let min = c.setup().min_keycode;
    let count = c.setup().max_keycode - min + 1;
    let original = c.get_keyboard_mapping(min, count).unwrap().reply().unwrap();
    let per = original.keysyms_per_keycode as usize;
    let spare = min
        + original
            .keysyms
            .chunks(per)
            .rposition(|v| v.iter().all(|s| *s == 0))
            .unwrap() as u8;
    // The third symbol is an unrelated local map change and must survive.
    for actual in [0x01001ebfu32, 0x01001ec7, 0x7a] {
        let socket =
            std::env::temp_dir().join(format!("remotevnc-map-{}.sock", uuid::Uuid::new_v4()));
        let mut guardian = launch("guardian", &socket);
        wait(|| socket.exists());
        let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let send = |s: &mut std::os::unix::net::UnixStream, value: serde_json::Value| {
            let bytes = serde_json::to_vec(&value).unwrap();
            s.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap();
            s.write_all(&bytes).unwrap();
        };
        let receive = |s: &mut std::os::unix::net::UnixStream| {
            let mut size = [0; 4];
            s.read_exact(&mut size).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
            s.read_exact(&mut bytes).unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };
        send(
            &mut stream,
            serde_json::json!({"Begin":{"target":t,"deadline":recovery::boot_time_ms().unwrap()+10_000}}),
        );
        let anchor = receive(&mut stream);
        let window = anchor["Anchor"]["window"].as_u64().unwrap() as u32;
        let atom = anchor["Anchor"]["atom"].as_u64().unwrap() as u32;
        let nonce: Vec<u8> = anchor["Anchor"]["nonce"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u8)
            .collect();
        assert_eq!(
            c.get_property(
                false,
                window,
                atom,
                x11rb::protocol::xproto::AtomEnum::STRING,
                0,
                16
            )
            .unwrap()
            .reply()
            .unwrap()
            .value,
            nonce
        );
        send(&mut stream, serde_json::json!("Ready"));
        assert_eq!(receive(&mut stream), serde_json::json!("Ack"));
        send(
            &mut stream,
            serde_json::json!({"State":{"keys":[spare],"buttons":[],"text":{"code":spare,"original":vec![0u32;per],"expected":[vec![0x01001ebfu32;per],vec![0x01001ec7u32;per]],"local_modifiers":[],"locked_mods":0,"locked_group":0}}}),
        );
        assert_eq!(receive(&mut stream), serde_json::json!("Ack"));
        c.change_keyboard_mapping(1, spare, per as u8, &vec![actual; per])
            .unwrap()
            .check()
            .unwrap();
        let changed = c
            .get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms;
        drop(stream);
        wait(|| guardian.try_wait().unwrap().is_some());
        let after = c
            .get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms;
        c.change_keyboard_mapping(1, spare, per as u8, &vec![0; per])
            .unwrap()
            .check()
            .unwrap();
        std::fs::remove_file(socket).unwrap();
        if actual == 0x7a {
            assert_eq!(after, changed, "unrelated map must survive");
        } else {
            assert_eq!(
                after, original.keysyms,
                "both acknowledged transition states must restore"
            );
        }
    }
}
