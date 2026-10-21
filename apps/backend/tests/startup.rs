//! Exercise the installed entry point before OIDC discovery, without network/credentials.
use std::process::Command;
#[test]
fn production_binary_rejects_missing_or_invalid_auth_before_database_writes() {
    for origin in [
        None,
        Some("http://example.test"),
        Some("https://example.test/path"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("must-not-exist.db");
        let mut command = Command::new(env!("CARGO_BIN_EXE_remotevnc-backend"));
        command.env_clear().env("REMOTEVNC_DATABASE", &database);
        if let Some(origin) = origin {
            command
                .env("REMOTEVNC_PUBLIC_ORIGIN", origin)
                .env("REMOTEVNC_GOOGLE_CLIENT_ID", "fixture-id")
                .env("REMOTEVNC_GOOGLE_CLIENT_SECRET", "fixture-secret");
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
        assert!(!database.exists());
        let stderr = String::from_utf8(result.stderr).unwrap();
        assert!(!stderr.contains("fixture-secret"));
        assert_eq!(
            stderr.trim(),
            "RemoteVNC backend could not start or continue"
        );
    }
}

#[test]
fn invalid_turn_file_is_rejected_before_database_or_discovery() {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let database = directory.path().join("must-not-exist.db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_remotevnc-backend"))
        .env_clear()
        .env("REMOTEVNC_PUBLIC_ORIGIN", "https://example.test")
        .env("REMOTEVNC_GOOGLE_CLIENT_ID", "fixture-id")
        .env("REMOTEVNC_GOOGLE_CLIENT_SECRET", "fixture-secret")
        .env("REMOTEVNC_DATABASE", &database)
        .env(
            "REMOTEVNC_TURN_KEYS_FILE",
            directory.path().join("missing.json"),
        )
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(!database.exists());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap().trim(),
        "RemoteVNC backend could not start or continue"
    );
}
