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
