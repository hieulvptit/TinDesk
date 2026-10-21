//! Process/native acceptance; opt in with scripts/test-e2e.sh on disposable Xvfb.
#![cfg(target_os = "linux")]
use remotevnc_agent::config::Config;
use remotevnc_device::{CredentialStore, DeviceApi, DeviceIdentity, FileStore, StoredCredentials};
use remotevnc_protocol::*;
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConnectionExt as _, KeyButMask},
};

fn key(byte: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[byte; 32])
}
struct Process(Child, bool);
impl Process {
    fn kill(&mut self) {
        // End only this fixture's private group before reaping its identity.
        // Never signal a numeric group again after wait() permits PID reuse.
        let result = unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        assert!(result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        self.0.wait().unwrap();
        self.1 = false;
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        // Capture children share only their fixture parent's private process group.
        if self.1 {
            unsafe {
                libc::kill(-(self.0.id() as i32), libc::SIGKILL);
            }
            let _ = self.0.wait();
        }
    }
}
fn spawn(dir: &Path, role: &str) -> Process {
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{role}.log")))
        .unwrap();
    Process(
        Command::new(
            std::env::var_os("REMOTEVNC_E2E_FIXTURE").expect("explicit fixture executable"),
        )
        .env("REMOTEVNC_E2E_ROLE", role)
        .env("REMOTEVNC_E2E_DIR", dir)
        .process_group(0)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap(),
        true,
    )
}
async fn wait(mut condition: impl FnMut() -> bool, duration: Duration, description: &str) {
    let end = Instant::now() + duration;
    while !condition() {
        assert!(Instant::now() < end, "timed out: {description}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
// Fresh test RSA keys are generated before listen and can take tens of seconds.
// This setup allowance never changes native disconnect, HTTP or authority bounds.
async fn backend_ready(dir: &Path, process: &mut Process, phase: &str) {
    let started = Instant::now();
    while !dir.join("backend.ready").exists() {
        if let Some(status) = process.0.try_wait().unwrap() {
            process.1 = false;
            panic!("backend fixture {phase} exited before readiness: {status}; see backend.log");
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "backend fixture {phase} setup exceeded 120s; see backend.log"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!(
        "backend fixture {phase} ready after {:?}",
        started.elapsed()
    );
}
fn held(connection: &x11rb::rust_connection::RustConnection) -> (bool, bool) {
    let min = connection.setup().min_keycode;
    let mapping = connection
        .get_keyboard_mapping(min, connection.setup().max_keycode - min + 1)
        .unwrap()
        .reply()
        .unwrap();
    let code = min as usize
        + mapping
            .keysyms
            .chunks(mapping.keysyms_per_keycode as usize)
            .position(|s| s.contains(&0xffe1))
            .unwrap();
    let keys = connection.query_keymap().unwrap().reply().unwrap().keys;
    let pointer = connection
        .query_pointer(connection.setup().roots[0].root)
        .unwrap()
        .reply()
        .unwrap();
    (
        keys[code / 8] & (1 << (code % 8)) != 0,
        pointer.mask.contains(KeyButMask::BUTTON1),
    )
}
async fn online(api: &DeviceApi, token: &AccessToken, agent: DeviceId) {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if api
            .presence(token)
            .await
            .is_ok_and(|p| p.iter().any(|p| p.device_id == agent && p.online))
        {
            return;
        }
        assert!(Instant::now() < end, "agent did not authenticate/reconnect");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn controller(
    dir: &Path,
    unicode: bool,
    connection: &x11rb::rust_connection::RustConnection,
) -> (Process, SessionGrant) {
    let _ = std::fs::remove_file(dir.join("controller.ready"));
    let process = spawn(
        dir,
        if unicode {
            "unicode-controller"
        } else {
            "controller"
        },
    );
    wait(
        || dir.join("controller.ready").exists(),
        Duration::from_secs(30),
        "native Peer H264/input readiness",
    )
    .await;
    wait(
        || held(connection) == (true, true),
        Duration::from_secs(5),
        "real X11 held key and button",
    )
    .await;
    let grant = serde_json::from_slice(&std::fs::read(dir.join("session.json")).unwrap()).unwrap();
    (process, grant)
}
async fn closed(api: &DeviceApi, token: &AccessToken, session: SessionId) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            api.renew_session(token, session).await,
            Err(remotevnc_device::Error::Unauthorized)
        ) {
            return;
        }
        assert!(Instant::now() < end, "dead session could still renew");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit process/native Xvfb/GTK acceptance: scripts/test-e2e.sh"]
async fn signed_accounts_unicode_revocation_and_process_restarts_release_input() {
    assert_eq!(std::env::var("REMOTEVNC_E2E").as_deref(), Ok("1"));
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let dir = directory.path();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Log paths are retained on failure in the explicitly supplied private results directory.
    let result_dir = PathBuf::from(std::env::var_os("REMOTEVNC_E2E_RESULTS").unwrap());
    struct Logs<'a>(&'a Path, &'a Path);
    impl Drop for Logs<'_> {
        fn drop(&mut self) {
            for entry in std::fs::read_dir(self.0).unwrap().flatten() {
                if entry.path().extension().is_some_and(|e| e == "log") {
                    let _ = std::fs::copy(entry.path(), self.1.join(entry.file_name()));
                }
            }
        }
    }
    let _logs = Logs(dir, &result_dir);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    std::fs::write(dir.join("address"), address.to_string()).unwrap();
    let mut backend = spawn(dir, "backend");
    backend_ready(dir, &mut backend, "initial").await;
    let ids: Vec<DeviceId> =
        serde_json::from_slice(&std::fs::read(dir.join("devices.json")).unwrap()).unwrap();
    let base = format!("http://{address}");
    let api = DeviceApi::new(&base, key(1)).unwrap();
    let token = api
        .authenticate(&DeviceIdentity {
            device_id: ids[0],
            signing_key: key(1),
        })
        .await
        .unwrap();
    let authority_api = DeviceApi::new(&base, key(2)).unwrap();
    let authority_token = authority_api
        .authenticate(&DeviceIdentity {
            device_id: ids[1],
            signing_key: key(2),
        })
        .await
        .unwrap();
    let other = DeviceApi::new(&base, key(3)).unwrap();
    let other_token = other
        .authenticate(&DeviceIdentity {
            device_id: ids[2],
            signing_key: key(3),
        })
        .await
        .unwrap();
    let owned = other.list_devices(&other_token).await.unwrap();
    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].id, ids[2]);
    assert!(
        other
            .presence(&other_token)
            .await
            .unwrap()
            .iter()
            .all(|p| p.device_id == ids[2])
    );
    assert!(matches!(
        other.create_session(&other_token, ids[1]).await,
        Err(remotevnc_device::Error::Unauthorized)
    ));
    let _guardian = spawn(dir, "guardian");
    wait(
        || dir.join("guardian.sock").exists(),
        Duration::from_secs(3),
        "independent guardian socket",
    )
    .await;
    let mut helper = spawn(dir, "helper");
    wait(
        || dir.join("helper.sock").exists(),
        Duration::from_secs(3),
        "helper socket",
    )
    .await;
    let identity = dir.join("identity");
    std::fs::create_dir(&identity).unwrap();
    std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o700)).unwrap();
    FileStore::new(&identity)
        .save(&StoredCredentials {
            device_id: Some(ids[1]),
            signing_key: key(2),
        })
        .unwrap();
    let config = Config {
        backend_origin: base.clone(),
        identity_path: identity,
        helper_socket: dir.join("helper.sock"),
        seat: "seat0".into(),
        relay_only: false,
    };
    let config_path = dir.join("agent.json");
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let log = std::fs::File::create(dir.join("agent.log")).unwrap();
    let _agent = Process(
        Command::new(env!("CARGO_BIN_EXE_remotevnc-agent"))
            .args(["run", "--config"])
            .arg(config_path)
            .process_group(0)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .unwrap(),
        true,
    );
    online(&api, &token, ids[1]).await;
    let (connection, _) = x11rb::connect(None).unwrap();
    assert_eq!(held(&connection), (false, false));
    let (mut client, grant) = controller(dir, true, &connection).await;
    let receiver = PathBuf::from(std::env::var_os("REMOTEVNC_GTK_RESULT").unwrap());
    wait(
        || std::fs::read_to_string(&receiver).is_ok_and(|s| s == "Tiếng Việt|é|e\u{301}|中文|😀"),
        Duration::from_secs(5),
        "exact end-to-end GTK Unicode",
    )
    .await;
    let pts = || {
        std::fs::read_to_string(dir.join("controller.pts"))
            .ok()?
            .parse::<u64>()
            .ok()
    };
    let before_pause = pts().expect("decoded source PTS");
    assert_eq!(
        unsafe { libc::kill(_agent.0.id() as i32, libc::SIGSTOP) },
        0
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        unsafe { libc::kill(_agent.0.id() as i32, libc::SIGCONT) },
        0
    );
    wait(
        || pts().is_some_and(|p| p > before_pause + 500_000_000),
        Duration::from_secs(20),
        "fresh decoded media after 250ms scheduling pressure",
    )
    .await;
    assert_eq!(
        held(&connection),
        (true, true),
        "video pressure must preserve held input"
    );
    assert!(
        authority_api
            .renew_session(&authority_token, grant.session_id)
            .await
            .is_ok()
    );
    println!(
        "PASS 250ms agent scheduling pause: fresh decoded PTS, held input, generation/session and authority preserved"
    );
    // Account B cannot signal into an established A session; its socket is policy-closed.
    let (mut bad_sender, mut bad_events) = other
        .events(&other_token, DeviceRole::Client)
        .await
        .unwrap()
        .split();
    bad_sender
        .send(&DeviceCommand::Signal {
            receiver: ids[1],
            message: SignalMessage::Offer {
                session_id: grant.session_id,
                epoch: 1,
                sdp: "cross-account-denied".into(),
            },
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match bad_events.next().await {
                Ok(Some(DeviceEvent::Presence { .. } | DeviceEvent::SessionSnapshot { .. })) => {}
                Ok(None) | Err(_) => break,
                _ => panic!("unexpected account B event"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        held(&connection),
        (true, true),
        "denied B must not disturb A"
    );
    println!(
        "PASS signed account A -> real H264/WebRTC reliable channel -> helper -> exact GTK Unicode; account B enumerate/create/signal denied"
    );
    // Actual SIGKILL destroys native Peer and signaling process, with no End command.
    assert!(
        authority_api
            .renew_session(&authority_token, grant.session_id)
            .await
            .is_ok(),
        "live agent renewal control must succeed before native process loss"
    );
    let start = Instant::now();
    client.kill();
    wait(
        || held(&connection) == (false, false),
        Duration::from_secs(45),
        "SIGKILL controller input release",
    )
    .await;
    closed(&authority_api, &authority_token, grant.session_id).await;
    println!(
        "PASS established controller SIGKILL: held key/button released and session cannot renew in {:?}; no Signal::End",
        start.elapsed()
    );
    drop(client);
    let (mut client, grant) = controller(dir, false, &connection).await;
    let start = Instant::now();
    helper.kill();
    wait(
        || held(&connection) == (false, false),
        Duration::from_secs(5),
        "independent guardian after helper SIGKILL",
    )
    .await;
    closed(&authority_api, &authority_token, grant.session_id).await;
    println!(
        "PASS helper SIGKILL: guardian releases held key/button and session closes in {:?}",
        start.elapsed()
    );
    client.kill();
    drop(client);
    drop(helper);
    std::fs::remove_file(dir.join("helper.sock")).unwrap();
    helper = spawn(dir, "helper");
    wait(
        || dir.join("helper.sock").exists(),
        Duration::from_secs(3),
        "restarted helper",
    )
    .await;
    let (mut client, grant) = controller(dir, false, &connection).await;
    let start = Instant::now();
    backend.kill();
    // Backend is actually down; HTTP renewal cannot mask native input release.
    wait(
        || held(&connection) == (false, false),
        Duration::from_secs(15),
        "backend process loss releases input",
    )
    .await;
    println!(
        "PASS backend SIGKILL: held input released in {:?}",
        start.elapsed()
    );
    client.kill();
    drop(client);
    drop(backend);
    std::fs::remove_file(dir.join("backend.ready")).unwrap();
    backend = spawn(dir, "backend");
    backend_ready(dir, &mut backend, "restart").await;
    online(&api, &token, ids[1]).await;
    closed(&authority_api, &authority_token, grant.session_id).await;
    let (mut client, grant) = controller(dir, false, &connection).await;
    let start = Instant::now();
    api.revoke_device(&token, ids[1]).await.unwrap();
    wait(
        || held(&connection) == (false, false),
        Duration::from_secs(5),
        "live revoke releases held input",
    )
    .await;
    closed(&authority_api, &authority_token, grant.session_id).await;
    assert!(api.create_session(&token, ids[1]).await.is_err());
    println!(
        "PASS fresh session after helper/backend restarts; live revoke stops access and releases held key/button in {:?}",
        start.elapsed()
    );
    wait(
        || {
            if client.0.try_wait().unwrap().is_some() {
                client.1 = false;
                true
            } else {
                false
            }
        },
        Duration::from_secs(5),
        "revoked controller process exits",
    )
    .await;
    drop(helper);
    drop(backend);
}
