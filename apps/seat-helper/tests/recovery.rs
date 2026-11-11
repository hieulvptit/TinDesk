//! Real X server/process fixtures. Never used by the production display monitor.
#![cfg(target_os = "linux")]
use binachicken_protocol::{InputEvent, MouseButton};
use binachicken_seat_helper::{input::X11Input, recovery, seat::*};
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
fn private_send(stream: &mut std::os::unix::net::UnixStream, value: serde_json::Value) {
    use std::io::Write;
    let bytes = serde_json::to_vec(&value).unwrap();
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&bytes).unwrap();
}
fn private_receive(stream: &mut std::os::unix::net::UnixStream) -> serde_json::Value {
    use std::io::Read;
    let mut size = [0; 4];
    stream.read_exact(&mut size).unwrap();
    let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
    stream.read_exact(&mut bytes).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}
fn raw_guardian(
    c: &x11rb::rust_connection::RustConnection,
    socket: &std::path::Path,
    deadline: u64,
) -> (std::os::unix::net::UnixStream, u32) {
    use x11rb::{
        protocol::xproto::{self, CreateWindowAux, PropMode, WindowClass},
        wrapper::ConnectionExt as _,
    };
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    private_send(
        &mut stream,
        serde_json::json!({"Begin":{"target":target(),"deadline":deadline}}),
    );
    let anchor = private_receive(&mut stream);
    let window = anchor["Anchor"]["window"].as_u64().unwrap() as u32;
    let atom = anchor["Anchor"]["atom"].as_u64().unwrap() as u32;
    let nonce: Vec<u8> = anchor["Anchor"]["nonce"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u8)
        .collect();
    assert_eq!(
        c.get_property(false, window, atom, xproto::AtomEnum::STRING, 0, 16)
            .unwrap()
            .reply()
            .unwrap()
            .value,
        nonce
    );
    let lifetime = c.generate_id().unwrap();
    c.create_window(
        0,
        lifetime,
        c.setup().roots[0].root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .unwrap()
    .check()
    .unwrap();
    c.change_property8(
        PropMode::REPLACE,
        lifetime,
        atom,
        xproto::AtomEnum::STRING,
        &nonce,
    )
    .unwrap()
    .check()
    .unwrap();
    private_send(
        &mut stream,
        serde_json::json!({"Ready":{"lifetime_window":lifetime}}),
    );
    assert_eq!(private_receive(&mut stream), serde_json::json!("Ack"));
    (stream, lifetime)
}
// Test-process-only syscall denial. No production bypass or seccomp policy.
fn deny_syscall(number: libc::c_long, errno: u32, allow_signal_zero: bool) {
    let mut filter = vec![
        libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 0,
        },
        libc::sock_filter {
            code: 0x15,
            jt: 0,
            jf: if allow_signal_zero { 3 } else { 1 },
            k: number as u32,
        },
    ];
    if allow_signal_zero {
        filter.push(libc::sock_filter {
            code: 0x20,
            jt: 0,
            jf: 0,
            k: 24,
        });
        filter.push(libc::sock_filter {
            code: 0x15,
            jt: 1,
            jf: 0,
            k: 0,
        });
    }
    filter.push(libc::sock_filter {
        code: 0x06,
        jt: 0,
        jf: 0,
        k: 0x0005_0000 | errno,
    });
    filter.push(libc::sock_filter {
        code: 0x06,
        jt: 0,
        jf: 0,
        k: 0x7fff_0000,
    });
    let program = libc::sock_fprog {
        len: filter.len() as u16,
        filter: filter.as_mut_ptr(),
    };
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) },
        0
    );
}
fn paused_producer(role: &str, socket: &std::path::Path) {
    use x11rb::protocol::{xproto, xtest::ConnectionExt as _};
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
    c.xtest_get_version(2, 2).unwrap().reply().unwrap();
    let min = c.setup().min_keycode;
    let map = c
        .get_keyboard_mapping(min, c.setup().max_keycode - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let code = min
        + map
            .keysyms
            .chunks(map.keysyms_per_keycode as usize)
            .position(|v| v.contains(&0x61))
            .unwrap() as u8;
    let deadline = recovery::boot_time_ms().unwrap() + 300;
    let (mut stream, lifetime) = raw_guardian(&c, socket, deadline);
    private_send(
        &mut stream,
        serde_json::json!({"State":{"keys":[code],"buttons":[],"text":null}}),
    );
    assert_eq!(private_receive(&mut stream), serde_json::json!("Ack"));
    std::fs::write(
        socket.with_extension("ready.tmp"),
        format!("{deadline} {lifetime} {code}"),
    )
    .unwrap();
    std::fs::rename(
        socket.with_extension("ready.tmp"),
        socket.with_extension("ready"),
    )
    .unwrap();
    if role == "late_eof" {
        drop(stream);
    }
    unsafe {
        libc::raise(libc::SIGSTOP);
    }
    let request = c
        .xtest_fake_input(
            xproto::KEY_PRESS_EVENT,
            code,
            0,
            c.setup().roots[0].root,
            0,
            0,
            0,
        )
        .unwrap();
    if role != "queued_key" {
        request.check().unwrap();
    }
    c.flush().unwrap();
    std::fs::write(socket.with_extension("injected"), []).unwrap();
    unsafe {
        libc::raise(libc::SIGSTOP);
    }
}
#[test]
#[ignore = "subprocess entry point for Xvfb recovery fixtures"]
fn recovery_child() {
    let Ok(role) = std::env::var("BINACHICKEN_RECOVERY_TEST_ROLE") else {
        return;
    };
    let socket = std::env::var("BINACHICKEN_RECOVERY_TEST_SOCKET").unwrap();
    if matches!(role.as_str(), "late_key" | "late_eof" | "queued_key") {
        paused_producer(&role, socket.as_ref());
        return;
    }
    if role.starts_with("guardian") {
        let listener = UnixListener::bind(socket).unwrap();
        let (mut stream, _) = listener.accept().unwrap();
        if role == "guardian_no_pidfd" {
            deny_syscall(libc::SYS_getsockopt, libc::ENOPROTOOPT as u32, false);
        } else if role == "guardian_no_kill" {
            deny_syscall(libc::SYS_pidfd_send_signal, libc::EPERM as u32, true);
        }
        let result = recovery::serve(&mut stream);
        if role != "guardian" {
            assert!(result.is_err());
        }
    } else {
        let deadline =
            recovery::boot_time_ms().unwrap() + if role == "deadline" { 200 } else { 60_000 };
        let mut input = X11Input::connect_guarded(&target(), socket.as_ref(), deadline).unwrap();
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
            if role == "setup_loss" {
                std::fs::write(std::path::Path::new(&socket).with_extension("ready"), []).unwrap();
                wait(|| !input.guardian_healthy());
                assert!(
                    input
                        .apply(&InputEvent::Button {
                            button: MouseButton::Left,
                            pressed: true,
                            x: 10,
                            y: 10,
                        })
                        .is_err(),
                    "setup must fail closed after guardian loss"
                );
                return;
            }
            input
                .apply(&InputEvent::Button {
                    button: MouseButton::Left,
                    pressed: true,
                    x: 10,
                    y: 10,
                })
                .unwrap();
            std::fs::write(std::path::Path::new(&socket).with_extension("ready"), []).unwrap();
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
        let _ = std::fs::remove_file(self.socket.with_extension("ready"));
        let _ = std::fs::remove_file(self.socket.with_extension("injected"));
    }
}
fn launch(role: &str, socket: &std::path::Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "recovery_child", "--ignored", "--nocapture"])
        .env("BINACHICKEN_RECOVERY_TEST_ROLE", role)
        .env("BINACHICKEN_RECOVERY_TEST_SOCKET", socket)
        .spawn()
        .unwrap()
}
fn fixture(role: &str) -> Fixture {
    let socket = std::env::temp_dir().join(format!(
        "binachicken-recovery-{}.sock",
        uuid::Uuid::new_v4()
    ));
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
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
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
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
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
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
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
    guardian_loss("held");
}
#[test]
#[ignore = "requires isolated Xvfb"]
fn guardian_loss_during_setup_fails_closed_and_releases() {
    guardian_loss("setup_loss");
}
fn guardian_loss(role: &str) {
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
    let mut f = fixture(role);
    wait(|| f.socket.with_extension("ready").exists());
    f.guardian.kill().unwrap();
    f.guardian.wait().unwrap();
    wait(|| f.helper.try_wait().unwrap().is_some());
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
    assert!(f.helper.wait().unwrap().success());
}

#[test]
#[ignore = "requires isolated Xvfb; resets only verified Xvfb process"]
fn guardian_never_restores_old_ownership_into_reset_xserver() {
    use std::os::fd::AsRawFd;
    use x11rb::protocol::{xproto, xtest::ConnectionExt as _};
    let t = target();
    let c = binachicken_seat_helper::x11::connect(&t).unwrap();
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
    let fresh = binachicken_seat_helper::x11::connect(&t).unwrap();
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
    let c = binachicken_seat_helper::x11::connect(&t).unwrap();
    let socket =
        std::env::temp_dir().join(format!("binachicken-normal-{}.sock", uuid::Uuid::new_v4()));
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
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
    let mut f = fixture("deadline");
    wait(|| f.socket.with_extension("ready").exists());
    wait(|| f.helper.try_wait().unwrap().is_some());
    wait(|| f.guardian.try_wait().unwrap().is_some());
    assert!(
        c.query_keymap()
            .unwrap()
            .reply()
            .unwrap()
            .keys
            .iter()
            .all(|v| *v == 0)
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
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
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
    let t = target();
    let c = binachicken_seat_helper::x11::connect(&t).unwrap();
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
    let mut rearranged = vec![0u32; per];
    rearranged[2] = 0x01001ebf;
    rearranged[3] = 0x01001ebf;
    // An unrelated symbol AND the same symbol in another group must survive.
    for (actual, restore) in [
        (vec![0x01001ebfu32; per], true),
        (vec![0x01001ec7; per], true),
        (vec![0x7a; per], false),
        (rearranged, false),
    ] {
        let socket =
            std::env::temp_dir().join(format!("binachicken-map-{}.sock", uuid::Uuid::new_v4()));
        let mut guardian = launch("guardian", &socket);
        wait(|| socket.exists());
        let (mut stream, lifetime) =
            raw_guardian(&c, &socket, recovery::boot_time_ms().unwrap() + 10_000);
        private_send(
            &mut stream,
            serde_json::json!({"State":{"keys":[spare],"buttons":[],"text":{"code":spare,"original":vec![0u32;per],"expected":[vec![0x01001ebfu32;per],vec![0x01001ec7u32;per]],"local_modifiers":[],"locked_mods":0,"locked_group":0}}}),
        );
        assert_eq!(private_receive(&mut stream), serde_json::json!("Ack"));
        c.change_keyboard_mapping(1, spare, per as u8, &actual)
            .unwrap()
            .check()
            .unwrap();
        let changed = c
            .get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms;
        c.destroy_window(lifetime).unwrap().check().unwrap();
        private_send(&mut stream, serde_json::json!("Finish"));
        assert_eq!(private_receive(&mut stream), serde_json::json!("Ack"));
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
        if !restore {
            assert_eq!(after, changed, "unrelated map must survive");
        } else {
            assert_eq!(
                after, original.keysyms,
                "both acknowledged transition states must restore"
            );
        }
    }
}

#[test]
#[ignore = "requires isolated Xvfb"]
fn expiry_keeps_acknowledged_mutation_covered_until_producer_death() {
    delayed_mutation("late_key");
}
#[test]
#[ignore = "requires isolated Xvfb"]
fn ipc_eof_keeps_acknowledged_mutation_covered_until_producer_death() {
    delayed_mutation("late_eof");
}
fn delayed_mutation(role: &str) {
    use x11rb::protocol::{
        Event,
        xproto::{self, ChangeWindowAttributesAux, EventMask},
        xtest::ConnectionExt as _,
    };
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
    c.change_window_attributes(
        c.setup().roots[0].root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::KEY_PRESS),
    )
    .unwrap()
    .check()
    .unwrap();
    let mut f = fixture(role);
    wait(|| f.socket.with_extension("ready").exists());
    let fields: Vec<u64> = std::fs::read_to_string(f.socket.with_extension("ready"))
        .unwrap()
        .split_whitespace()
        .map(|v| v.parse().unwrap())
        .collect();
    wait(|| recovery::boot_time_ms().unwrap() > fields[0] + 100);
    wait(|| f.helper.try_wait().unwrap().is_some());
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        f.helper.wait().unwrap().signal(),
        Some(libc::SIGKILL),
        "guardian must terminate the paused producer"
    );
    wait(|| f.guardian.try_wait().unwrap().is_some());
    let code = fields[2] as u8;
    let down = c.query_keymap().unwrap().reply().unwrap().keys;
    let mut late_press = false;
    while let Some(event) = c.poll_for_event().unwrap() {
        late_press |= matches!(event, Event::KeyPress(_));
    }
    c.xtest_fake_input(
        xproto::KEY_RELEASE_EVENT,
        code,
        0,
        c.setup().roots[0].root,
        0,
        0,
        0,
    )
    .unwrap()
    .check()
    .unwrap();
    assert!(
        !late_press,
        "paused producer must not inject after expiry/EOF quiescence"
    );
    assert!(!f.socket.with_extension("injected").exists());
    assert_eq!(down[code as usize / 8] & (1 << (code % 8)), 0);
}

#[test]
#[ignore = "requires isolated Xvfb; server grab exists only in this fixture"]
fn queued_x11_mutation_cannot_follow_producer_lifetime_destruction() {
    use x11rb::protocol::{
        Event,
        xproto::{self, ChangeWindowAttributesAux, EventMask},
        xtest::ConnectionExt as _,
    };
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
    let root = c.setup().roots[0].root;
    c.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::KEY_PRESS),
    )
    .unwrap()
    .check()
    .unwrap();
    let mut f = fixture("queued_key");
    wait(|| f.socket.with_extension("ready").exists());
    let fields: Vec<u64> = std::fs::read_to_string(f.socket.with_extension("ready"))
        .unwrap()
        .split_whitespace()
        .map(|v| v.parse().unwrap())
        .collect();
    let lifetime = fields[1] as u32;
    let code = fields[2] as u8;
    c.change_window_attributes(
        lifetime,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
    )
    .unwrap()
    .check()
    .unwrap();
    // Queue a real request in the helper's X socket while Xvfb cannot process it.
    // No production path grabs the server.
    c.grab_server().unwrap().check().unwrap();
    assert_eq!(
        unsafe { libc::kill(f.helper.id() as i32, libc::SIGCONT) },
        0
    );
    wait(|| f.socket.with_extension("injected").exists());
    wait(|| f.helper.try_wait().unwrap().is_some());
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(f.helper.wait().unwrap().signal(), Some(libc::SIGKILL));
    assert!(
        f.guardian.try_wait().unwrap().is_none(),
        "guardian must await queued X11 processing before retirement"
    );
    c.ungrab_server().unwrap().check().unwrap();
    wait(|| f.guardian.try_wait().unwrap().is_some());
    let down = c.query_keymap().unwrap().reply().unwrap().keys;
    let mut destroyed = false;
    while let Some(event) = c.poll_for_event().unwrap() {
        match event {
            Event::DestroyNotify(e) if e.window == lifetime => destroyed = true,
            Event::KeyPress(_) => assert!(
                !destroyed,
                "queued input executed after lifetime destruction"
            ),
            _ => {}
        }
    }
    assert!(
        destroyed,
        "must observe real helper Xclient resource destruction"
    );
    assert!(c.get_window_attributes(lifetime).unwrap().reply().is_err());
    c.xtest_fake_input(xproto::KEY_RELEASE_EVENT, code, 0, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        down[code as usize / 8] & (1 << (code % 8)),
        0,
        "guardian final cleanup follows all old queued requests"
    );
}

#[test]
#[ignore = "requires isolated Xvfb and Linux seccomp fixture"]
fn unsupported_peer_pidfd_rejects_admission_before_input() {
    let c = binachicken_seat_helper::x11::connect(&target()).unwrap();
    let before = c.query_keymap().unwrap().reply().unwrap().keys;
    let socket =
        std::env::temp_dir().join(format!("binachicken-pidfd-{}.sock", uuid::Uuid::new_v4()));
    let mut guardian = launch("guardian_no_pidfd", &socket);
    wait(|| socket.exists());
    assert!(
        X11Input::connect_guarded(
            &target(),
            &socket,
            recovery::boot_time_ms().unwrap() + 60_000
        )
        .is_err()
    );
    wait(|| guardian.try_wait().unwrap().is_some());
    assert!(guardian.wait().unwrap().success());
    assert_eq!(c.query_keymap().unwrap().reply().unwrap().keys, before);
    std::fs::remove_file(socket).unwrap();
}
#[test]
#[ignore = "requires isolated Xvfb and Linux seccomp fixture"]
fn denied_pidfd_termination_retains_recovery_until_producer_is_gone() {
    let socket =
        std::env::temp_dir().join(format!("binachicken-pidfd-{}.sock", uuid::Uuid::new_v4()));
    let guardian = launch("guardian_no_kill", &socket);
    wait(|| socket.exists());
    let helper = launch("late_key", &socket);
    let mut f = Fixture {
        guardian,
        helper,
        socket,
    };
    wait(|| f.socket.with_extension("ready").exists());
    let fields: Vec<u64> = std::fs::read_to_string(f.socket.with_extension("ready"))
        .unwrap()
        .split_whitespace()
        .map(|v| v.parse().unwrap())
        .collect();
    wait(|| recovery::boot_time_ms().unwrap() > fields[0] + 100);
    assert!(
        f.helper.try_wait().unwrap().is_none(),
        "fixture must really deny the kill"
    );
    assert!(
        f.guardian.try_wait().unwrap().is_none(),
        "denied termination must retain recovery responsibility"
    );
    f.helper.kill().unwrap();
    f.helper.wait().unwrap();
    wait(|| f.guardian.try_wait().unwrap().is_some());
    assert!(
        f.guardian.wait().unwrap().success(),
        "guardian serve must report the denied termination"
    );
}

#[test]
#[ignore = "requires isolated Xvfb"]
fn normal_unicode_cleanup_preserves_local_same_symbol_remap() {
    let t = target();
    let c = binachicken_seat_helper::x11::connect(&t).unwrap();
    let min = c.setup().min_keycode;
    let count = c.setup().max_keycode - min + 1;
    let original = c.get_keyboard_mapping(min, count).unwrap().reply().unwrap();
    let per = original.keysyms_per_keycode as usize;
    let socket = std::env::temp_dir().join(format!(
        "binachicken-local-map-{}.sock",
        uuid::Uuid::new_v4()
    ));
    let mut guardian = launch("guardian", &socket);
    wait(|| socket.exists());
    let mut input =
        X11Input::connect_guarded(&t, &socket, recovery::boot_time_ms().unwrap() + 60_000).unwrap();
    let changed = std::cell::RefCell::new(None);
    let result = input.apply_with_cancel(&InputEvent::Text { text: "ế".into() }, || {
        let map = c
            .get_keyboard_mapping(min, count)
            .unwrap()
            .reply()
            .unwrap()
            .keysyms;
        if let Some(index) = map
            .chunks(per)
            .zip(original.keysyms.chunks(per))
            .position(|(a, b)| a != b)
        {
            let mut local = vec![0; per];
            local[2] = 0x01001ebf;
            local[3] = 0x01001ebf;
            c.change_keyboard_mapping(1, min + index as u8, per as u8, &local)
                .unwrap()
                .check()
                .unwrap();
            *changed.borrow_mut() = Some(
                c.get_keyboard_mapping(min, count)
                    .unwrap()
                    .reply()
                    .unwrap()
                    .keysyms,
            );
            true
        } else {
            false
        }
    });
    assert!(result.is_err());
    drop(input);
    wait(|| guardian.try_wait().unwrap().is_some());
    let after = c
        .get_keyboard_mapping(min, count)
        .unwrap()
        .reply()
        .unwrap()
        .keysyms;
    c.change_keyboard_mapping(count, min, per as u8, &original.keysyms)
        .unwrap()
        .check()
        .unwrap();
    assert_eq!(
        after,
        changed
            .into_inner()
            .expect("fixture must make completed same-symbol local change")
    );
    std::fs::remove_file(socket).unwrap();
}

#[test]
#[ignore = "requires Xvfb; starts and stops only its own private server"]
fn guardian_spares_producer_when_its_xserver_exits() {
    use std::os::unix::process::ExitStatusExt;
    // Model LightDM retiring the lock-screen greeter's X server on unlock.
    let dir = std::env::temp_dir().join(format!("binachicken-exit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let authority = dir.join("authority");
    std::fs::write(&authority, []).unwrap();
    let number = dir.join("display");
    let mut xvfb = Command::new("sh")
        .arg("-c")
        .arg("exec Xvfb -displayfd 3 -nolisten tcp -screen 0 800x600x24 3>\"$1\"")
        .arg("sh")
        .arg(&number)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    wait(|| std::fs::read_to_string(&number).is_ok_and(|n| n.ends_with('\n')));
    let display = format!(":{}", std::fs::read_to_string(&number).unwrap().trim());
    let socket = dir.join("guardian.sock");
    let child = |role: &str| {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "recovery_child", "--ignored", "--nocapture"])
            .env("BINACHICKEN_RECOVERY_TEST_ROLE", role)
            .env("BINACHICKEN_RECOVERY_TEST_SOCKET", &socket)
            .env("DISPLAY", &display)
            .env("XAUTHORITY", &authority)
            .spawn()
            .unwrap()
    };
    let mut guardian = child("guardian");
    wait(|| socket.exists());
    let mut helper = child("held");
    wait(|| socket.with_extension("ready").exists());
    xvfb.kill().unwrap();
    xvfb.wait().unwrap();
    wait(|| helper.try_wait().unwrap().is_some());
    let status = helper.wait().unwrap();
    wait(|| guardian.try_wait().unwrap().is_some());
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!(
        status.signal(),
        Some(libc::SIGKILL),
        "an exited X server must not make the guardian kill the producer"
    );
    assert!(status.success(), "producer observes the lost server itself");
}
