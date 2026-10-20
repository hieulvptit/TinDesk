//! Explicit disposable-VM fixture. Default cargo test invocation is inert.
use remotevnc_protocol::*;
use remotevnc_seat_helper::{
    ipc, recovery,
    seat::{DisplayTarget, ServerIdentity},
};
use std::{
    io::{Read, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{xinput::ConnectionExt as _, xproto::ConnectionExt as _, xtest::ConnectionExt as _},
};
fn main() {
    let result = match std::env::args().nth(1).as_deref() {
        Some("--capture-worker") => remotevnc_seat_helper::capture::worker_main(),
        Some("service") => service(),
        Some("probe") => probe(),
        _ => Ok(()),
    };
    if let Err(e) = result {
        eprintln!("fixture: {e}");
        std::process::exit(1);
    }
}
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
fn service() -> remotevnc_seat_helper::Result<()> {
    assert_eq!(unsafe { libc::geteuid() }, 0);
    // Verify the shipped AF restriction inside the actual helper unit namespace.
    assert_eq!(
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) },
        -1
    );
    let listener = UnixListener::bind("/run/remotevnc/helper.sock")?;
    let uid = std::process::Command::new("id")
        .args(["-u", "remotevnc-agent"])
        .output()?;
    let uid = std::str::from_utf8(&uid.stdout)?.trim().parse::<u32>()?;
    let path = c"/run/remotevnc/helper.sock";
    if unsafe { libc::chown(path.as_ptr(), uid, u32::MAX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    std::fs::set_permissions(
        "/run/remotevnc/helper.sock",
        std::fs::Permissions::from_mode(0o600),
    )?;
    let mut generation = 0;
    loop {
        let (mut peer, _) = listener.accept()?;
        ipc::authorize_uid(ipc::peer_uid(&peer)?, uid)?;
        let _ = ipc::serve(
            &mut peer,
            &mut |_| Ok(Some(target())),
            &mut generation,
            || recovery::boot_time_ms().unwrap_or(u64::MAX),
            &AtomicBool::new(false),
        );
    }
}
fn send(stream: &mut UnixStream, request: HelperRequest) -> remotevnc_seat_helper::Result<()> {
    let bytes = encode_control(&request)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}
fn wait(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < end, "native fixture timeout");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn main_pid(unit: &str) -> u32 {
    let output = std::process::Command::new("systemctl")
        .args(["show", unit, "--property=MainPID", "--value"])
        .output()
        .unwrap();
    std::str::from_utf8(&output.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn probe() -> remotevnc_seat_helper::Result<()> {
    // Root orchestrates Xvfb and systemctl; forked connection gets the dedicated agent UID.
    let helper_pid = main_pid("remotevnc-seat-helper.service");
    let guardian_pid = main_pid("remotevnc-input-guardian.service");
    let uid = std::process::Command::new("id")
        .args(["-u", "remotevnc-agent"])
        .output()?;
    let uid = std::str::from_utf8(&uid.stdout)?.trim().parse::<u32>()?;
    let c = remotevnc_seat_helper::x11::connect(&target())?;
    c.xinput_xi_query_version(2, 0)?.reply()?;
    let root = c.setup().roots[0].root;
    let devices = c
        .xinput_xi_query_device(x11rb::protocol::xinput::Device::ALL)?
        .reply()?
        .infos;
    let keyboard = devices
        .iter()
        .find(|d| d.name == b"Xvfb keyboard")
        .unwrap()
        .deviceid as u8;
    let key_type = c
        .xinput_open_device(keyboard)?
        .reply()?
        .class_info
        .iter()
        .find(|d| d.class_id == x11rb::protocol::xinput::InputClass::KEY)
        .unwrap()
        .event_type_base;
    let min = c.setup().min_keycode;
    let map = c
        .get_keyboard_mapping(min, c.setup().max_keycode - min + 1)?
        .reply()?;
    let code = min
        + map
            .keysyms
            .chunks(map.keysyms_per_keycode as usize)
            .position(|m| m.contains(&0x61))
            .unwrap() as u8;
    let held = || {
        let keys = c.query_keymap().unwrap().reply().unwrap().keys;
        keys[code as usize / 8] & (1 << (code % 8)) != 0
    };
    // Drop effective uid only around connect; kernel peer credentials prove the real agent owner.
    assert_eq!(unsafe { libc::seteuid(uid) }, 0);
    let connection = UnixStream::connect("/run/remotevnc/helper.sock");
    assert_eq!(unsafe { libc::seteuid(0) }, 0);
    let mut stream = connection?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let display = loop {
        match read_helper_output(&mut stream)? {
            HelperOutput::Event(HelperEvent::DisplayChanged(d)) => break d,
            HelperOutput::Event(HelperEvent::Error { message }) => panic!("{message}"),
            _ => {}
        }
    };
    let id = SessionId::from_uuid(uuid::Uuid::new_v4());
    let deadline = recovery::boot_time_ms()? + 8_000;
    send(
        &mut stream,
        HelperRequest::Start {
            session_id: id,
            display,
            deadline_boot_ms: deadline,
        },
    )?;
    send(
        &mut stream,
        HelperRequest::StartCapture {
            session_id: id,
            generation: display.generation,
            profile: VideoProfile {
                width: 320,
                height: 240,
                fps: 10,
                bitrate_kbps: 512,
                encoder: VideoEncoder::X264,
            },
        },
    )?;
    for (sequence, event) in [
        InputEvent::DisplayAck,
        InputEvent::Key {
            keysym: 0x61,
            pressed: true,
        },
    ]
    .into_iter()
    .enumerate()
    {
        send(
            &mut stream,
            HelperRequest::Input(InputEnvelope {
                version: WIRE_VERSION,
                session_id: id,
                generation: display.generation,
                sequence: sequence as u64,
                event,
            }),
        )?;
    }
    wait(held);
    let frame = loop {
        match read_helper_output(&mut stream)? {
            HelperOutput::Video(frame) => break frame,
            HelperOutput::Event(HelperEvent::Error { message }) => panic!("{message}"),
            _ => {}
        }
    };
    assert!(frame.keyframe && frame.data.len() > 100);
    let children =
        std::fs::read_to_string(format!("/proc/{helper_pid}/task/{helper_pid}/children"))?;
    let capture_pid = children
        .split_whitespace()
        .map(|s| s.parse::<u32>().unwrap())
        .find(|pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|s| s.windows(16).any(|w| w == b"--capture-worker"))
        })
        .expect("real capture child");
    println!(
        "sandbox capture: helper={helper_pid}, guardian={guardian_pid}, capture={capture_pid}, encoded_bytes={}",
        frame.data.len()
    );
    // Overlap comes from Xvfb's non-XTEST physical source, as in Task 7 native tests.
    c.xtest_fake_input(key_type, code, 0, root, 0, 0, keyboard)?
        .check()?;
    assert!(held());
    // Pause the main with input owned and the real capture child running. Guardian deadline kills it.
    assert_eq!(unsafe { libc::kill(helper_pid as i32, libc::SIGSTOP) }, 0);
    // Drain capture until guardian expiry interrupts its main; do not cause output backpressure first.
    let reader = std::thread::spawn(move || while read_helper_output(&mut stream).is_ok() {});
    wait(|| {
        main_pid("remotevnc-seat-helper.service") != helper_pid
            && main_pid("remotevnc-seat-helper.service") != 0
    });
    let restarted = main_pid("remotevnc-seat-helper.service");
    wait(|| !std::path::Path::new(&format!("/proc/{capture_pid}")).exists());
    assert_eq!(main_pid("remotevnc-input-guardian.service"), guardian_pid);
    assert!(
        held(),
        "pending physical source must survive helper cgroup cleanup"
    );
    println!(
        "deadline killed helper; restarted={restarted}; capture child gone; independent guardian preserved physical overlap"
    );
    // Guardian must remain busy while old physical ownership is pending.
    let mut probe = UnixStream::connect("/run/remotevnc-guardian/guardian.sock")?;
    let mut size = [0; 4];
    probe.read_exact(&mut size)?;
    let mut bytes = vec![0; u32::from_be_bytes(size) as usize];
    probe.read_exact(&mut bytes)?;
    assert!(String::from_utf8_lossy(&bytes).contains("Busy"));
    drop(probe);
    c.xtest_fake_input(key_type + 1, code, 0, root, 0, 0, keyboard)?
        .check()?;
    wait(|| !held());
    reader.join().unwrap();
    println!("physical source released; remote residual cleared; guardian PID unchanged");
    Ok(())
}
