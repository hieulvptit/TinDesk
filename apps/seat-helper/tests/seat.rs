use remotevnc_seat_helper::seat::{ServerIdentity, SessionProperties, physical_session};

#[test]
fn only_local_active_seat0_x11_is_eligible() {
    let good = SessionProperties::parse(
        "Seat=seat0\nType=x11\nRemote=no\nActive=yes\nDisplay=:7\nVTNr=7\n",
    );
    assert!(physical_session(&good));
    for (from, to) in [
        ("seat0", "seat1"),
        ("x11", "wayland"),
        ("Remote=no", "Remote=yes"),
        ("Active=yes", "Active=no"),
        ("VTNr=7", "VTNr=0"),
        (":7", "host:7"),
    ] {
        assert!(!physical_session(&SessionProperties::parse(
            &"Seat=seat0\nType=x11\nRemote=no\nActive=yes\nDisplay=:7\nVTNr=7\n".replace(from, to)
        )));
    }
}

#[test]
fn same_display_number_does_not_hide_server_reset() {
    let a = ServerIdentity {
        process_start: "100".into(),
        socket_inode: 42,
        authority_inode: 43,
        authority_modified_ns: 99,
        connection_epoch: 1,
    };
    let mut b = a.clone();
    b.connection_epoch += 1;
    assert_ne!(a, b);
}

#[test]
#[ignore = "requires scripts/test-x11.sh Xvfb fixture"]
fn live_probe_changes_epoch_on_same_pid_server_reset() {
    use remotevnc_seat_helper::{seat::DisplayTarget, x11::ServerProbe};
    use std::{thread, time::Duration};
    let t = DisplayTarget {
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
        width: 0,
        height: 0,
    };
    let mut probe = ServerProbe::default();
    let (first, _, _) = probe.refresh(&t).unwrap();
    assert_eq!(first, probe.refresh(&t).unwrap().0);
    let pid = std::env::var("REMOTEVNC_XVFB_PID")
        .unwrap()
        .parse::<i32>()
        .unwrap();
    unsafe {
        libc::kill(pid, libc::SIGHUP);
    }
    thread::sleep(Duration::from_millis(200));
    let (after, _, _) = probe.refresh(&t).unwrap();
    assert!(
        after > first,
        "same process and display must get new identity after reset"
    );
}

#[test]
fn stalled_discovery_subprocess_is_bounded_and_cancellable() {
    use remotevnc_seat_helper::seat::command_output;
    use std::{
        process::Command,
        time::{Duration, Instant},
    };
    let started = Instant::now();
    assert!(
        command_output(
            Command::new("/bin/sleep").arg("5"),
            Duration::from_millis(40),
            &|| false
        )
        .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    let started = Instant::now();
    assert!(
        command_output(
            Command::new("/bin/sleep").arg("5"),
            Duration::from_secs(5),
            &|| started.elapsed() >= Duration::from_millis(40)
        )
        .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        command_output(
            Command::new("/bin/echo").arg("seat0"),
            Duration::from_secs(1),
            &|| false
        )
        .unwrap(),
        "seat0\n"
    );
}

#[test]
fn selects_one_active_randr_output_and_offsets_pointer() {
    use remotevnc_seat_helper::seat::{OutputGeometry, select_output};
    let left = OutputGeometry {
        name: "HDMI-1".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: false,
    };
    let right = OutputGeometry {
        name: "DP-1".into(),
        x: 1920,
        y: 100,
        width: 1280,
        height: 720,
        primary: true,
    };
    assert_eq!(
        select_output(&[left.clone(), right.clone()], None).unwrap(),
        right
    );
    assert_eq!(
        select_output(&[left.clone(), right.clone()], Some("HDMI-1")).unwrap(),
        left
    );
    assert!(select_output(std::slice::from_ref(&right), Some("missing")).is_err());
    assert_eq!(right.root_position(100, 50).unwrap(), (2020, 150));
    assert!(right.root_position(1280, 50).is_err());
}
