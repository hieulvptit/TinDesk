use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use remotevnc_agent::{Result, config::Config, service::AgentService};
use remotevnc_device::{CredentialStore, DeviceApi, DeviceIdentity, FileStore, StoredCredentials};
use remotevnc_protocol::EnrollmentStatus;
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("remotevnc-agent: {error}");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().ok_or(
        "usage: remotevnc-agent enroll|run|doctor|revoke-local [--config PATH] [--name NAME]",
    )?;
    let mut path = PathBuf::from("/etc/remotevnc/agent.json");
    let mut name = String::from("Ubuntu remote computer");
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => path = args.next().ok_or("missing config path")?.into(),
            "--name" if command == "enroll" => name = args.next().ok_or("missing device name")?,
            _ => return Err("unknown argument".into()),
        }
    }
    let config = Config::read(&path)?;
    match command.as_str() {
        "run" => {
            let stop = CancellationToken::new();
            let signal = stop.clone();
            tokio::spawn(async move {
                let mut term =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("signal handler");
                tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
                signal.cancel();
            });
            AgentService::new(config)?.run(stop).await
        }
        "enroll" => enroll(&config, &name).await,
        "revoke-local" => revoke(&config).await,
        "doctor" => {
            let credentials = config.credentials()?;
            println!(
                "private identity readable by current UID; candidate enrolled: {}",
                credentials.device_id.is_some()
            );
            println!(
                "seat: {}; locally revoked: {}",
                config.seat,
                config.revoked_path().exists()
            );
            let mut helper = remotevnc_agent::helper::Helper::connect(&config.helper_socket)?;
            match tokio::time::timeout(Duration::from_secs(2), helper.events.recv()).await? {
                Some(remotevnc_protocol::HelperEvent::Ready) => println!(
                    "helper IPC ready; physical display and independent guardian admission are checked only when authorized control starts"
                ),
                _ => return Err("helper not ready".into()),
            }
            Ok(())
        }
        _ => Err("unknown command".into()),
    }
}
async fn enroll(config: &Config, name: &str) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&config.identity_path)
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let _lock = config.lock()?;
    let store = FileStore::new(&config.identity_path);
    let mut stored = store.load()?;
    if stored.is_none() {
        let mut secret = [0; 32];
        rand::rngs::OsRng.fill_bytes(&mut secret);
        stored = Some(StoredCredentials {
            device_id: None,
            signing_key: ed25519_dalek::SigningKey::from_bytes(&secret),
        });
        store.save(stored.as_ref().unwrap())?;
    }
    let mut credentials = stored.unwrap();
    let mut api = DeviceApi::new(&config.backend_origin, credentials.signing_key.clone())?;
    if let Some(id) = credentials.device_id {
        let identity = DeviceIdentity {
            device_id: id,
            signing_key: credentials.signing_key.clone(),
        };
        if api.authenticate(&identity).await.is_ok() {
            if config.revoked_path().exists() {
                std::fs::remove_file(config.revoked_path())?;
            }
            println!("Device enrollment recovered.");
            return Ok(());
        }
        if config.revoked_path().exists() {
            let mut secret = [0; 32];
            rand::rngs::OsRng.fill_bytes(&mut secret);
            credentials = StoredCredentials {
                device_id: None,
                signing_key: ed25519_dalek::SigningKey::from_bytes(&secret),
            };
            store.save(&credentials)?;
            api = DeviceApi::new(&config.backend_origin, credentials.signing_key.clone())?;
        }
    }
    let enrollment = api
        .begin_enrollment(
            name,
            &URL_SAFE_NO_PAD.encode(credentials.signing_key.verifying_key().to_bytes()),
        )
        .await?;
    // Persist the recoverable candidate before the first completion poll.
    credentials.device_id = Some(enrollment.challenge.device_id);
    store.save(&credentials)?;
    println!(
        "Open {} and confirm code {}",
        enrollment.browser_url, enrollment.display_code
    );
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match api.poll_enrollment(&enrollment).await? {
            EnrollmentStatus::Pending => {}
            EnrollmentStatus::Enrolled { device_id } => {
                credentials.device_id = Some(device_id);
                store.save(&credentials)?;
                if config.revoked_path().exists() {
                    std::fs::remove_file(config.revoked_path())?;
                }
                println!("Device enrolled. Start remotevnc-agent.service.");
                return Ok(());
            }
        }
    }
}
async fn revoke(config: &Config) -> Result<()> {
    let credentials = config.credentials()?;
    // Persistent tombstone stops the running in-memory identity and prevents systemd restart.
    let mut marker = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config.revoked_path())?;
    marker.write_all(b"locally revoked\n")?;
    marker.sync_all()?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let _lock = loop {
        if let Ok(lock) = config.lock() {
            break lock;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("local revocation marked, but running service has not stopped; stop remotevnc-agent.service before deleting identity".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let api = DeviceApi::new(&config.backend_origin, credentials.signing_key.clone())?;
    let remote = if let Some(id) = credentials.device_id {
        let identity = DeviceIdentity {
            device_id: id,
            signing_key: credentials.signing_key,
        };
        match api.authenticate(&identity).await {
            Ok(token) => api.revoke_device(&token, id).await.is_ok(),
            Err(_) => false,
        }
    } else {
        false
    };
    std::fs::remove_file(config.identity_path.join("identity.json"))?;
    if remote {
        println!("Running agent stopped; local identity removed; backend device revoked.");
    } else {
        println!(
            "Running agent stopped and local identity removed. Backend revocation was not confirmed; remove the device from another signed-in device when reachable."
        );
    }
    Ok(())
}
