//! Configuration is exercised in subprocesses so tests never mutate process environment.
use remotevnc_backend::turn::TurnClient;
use serde_json::json;
use std::{fs, path::Path, process::Command};

#[test]
fn configuration_probe() {
    let Ok(expected) = std::env::var("TURN_CONFIG_PROBE") else {
        return;
    };
    match expected.as_str() {
        "configured" => assert!(TurnClient::from_env().is_ok_and(|client| client.is_configured())),
        "unavailable" => {
            assert!(TurnClient::from_env().is_ok_and(|client| !client.is_configured()))
        }
        "invalid" => assert!(TurnClient::from_env().is_err()),
        _ => panic!("invalid fixture expectation"),
    }
}
fn probe(expected: &str, file: Option<&Path>, key: Option<&str>, token: Option<&str>) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .env_clear()
        .args(["--exact", "configuration_probe", "--nocapture"])
        .env("TURN_CONFIG_PROBE", expected);
    for (name, value) in [
        ("REMOTEVNC_TURN_KEY_ID", key),
        ("REMOTEVNC_TURN_API_TOKEN", token),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    if let Some(file) = file {
        command.env("REMOTEVNC_TURN_KEYS_FILE", file);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "configuration probe failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-token"));
}
fn private_file(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}
#[test]
fn file_and_legacy_configuration_are_exclusive_and_optional() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("keys.json");
    private_file(&path, br#"{"keys":[{"key_id":"first-key","api_token":"fixture-token"},{"key_id":"second_key","api_token":"second-token"}]}"#);
    probe("configured", Some(&path), None, None);
    probe(
        "configured",
        None,
        Some("legacy-key"),
        Some("fixture-token"),
    );
    probe("unavailable", None, None, None);
    for (key, token) in [
        (Some("legacy-key"), None),
        (None, Some("fixture-token")),
        (Some(""), None),
        (None, Some("")),
    ] {
        probe("invalid", Some(&path), key, token);
    }
    probe("invalid", None, Some("legacy-key"), None);
    probe("invalid", Some(Path::new("")), None, None);
}
#[test]
fn rejects_invalid_file_contents_and_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("keys.json");
    let valid_key = json!({"key_id":"first-key","api_token":"fixture-token"});
    for contents in [
        b"not json".to_vec(),
        json!({"keys":[]}).to_string().into_bytes(),
        json!({"keys":(0..17).map(|index| json!({"key_id":format!("key-{index}"),"api_token":format!("token-{index}")})).collect::<Vec<_>>()}).to_string().into_bytes(),
        json!({"keys":[valid_key.clone(),valid_key.clone()]}).to_string().into_bytes(),
        json!({"keys":[valid_key.clone()],"endpoint":"https://evil.test"}).to_string().into_bytes(),
        json!({"keys":[{"key_id":"first-key","api_token":"fixture-token","endpoint":"https://evil.test"}]}).to_string().into_bytes(),
        {
            let mut bytes = json!({"keys":[valid_key.clone()]}).to_string().into_bytes();
            bytes.resize(128*1024+1, b' ');
            bytes
        },
    ] {
        private_file(&path, &contents);
        probe("invalid", Some(&path), None, None);
    }
    for (key, token) in [
        ("", "fixture-token"),
        ("bad/key", "fixture-token"),
        (&"k".repeat(129), "fixture-token"),
        ("key", ""),
        ("key", "   "),
        ("key", "bad\ntoken"),
        ("key", &"t".repeat(4097)),
    ] {
        private_file(
            &path,
            json!({"keys":[{"key_id":key,"api_token":token}]})
                .to_string()
                .as_bytes(),
        );
        probe("invalid", Some(&path), None, None);
    }
    let keys: Vec<_> = (0..16)
        .map(|index| json!({"key_id":format!("key-{index}"),"api_token":format!("token-{index}")}))
        .collect();
    private_file(&path, json!({"keys":keys}).to_string().as_bytes());
    probe("configured", Some(&path), None, None);
}
#[cfg(unix)]
#[test]
fn requires_private_regular_non_symlink_file() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("keys.json");
    private_file(
        &path,
        br#"{"keys":[{"key_id":"key","api_token":"fixture-token"}]}"#,
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::File::open(&path).is_err() {
        probe("invalid", Some(&path), None, None);
    }
    for mode in [0o640, 0o604, 0o644] {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        probe("invalid", Some(&path), None, None);
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let link = directory.path().join("link.json");
    symlink(&path, &link).unwrap();
    probe("invalid", Some(&link), None, None);
    probe("invalid", Some(directory.path()), None, None);
    fs::remove_file(&path).unwrap();
    probe("invalid", Some(&path), None, None);
}

#[test]
fn distinct_keys_may_share_an_api_token() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("keys.json");
    private_file(&path, br#"{"keys":[{"key_id":"first-key","api_token":"fixture-token"},{"key_id":"second-key","api_token":"fixture-token"}]}"#);
    probe("configured", Some(&path), None, None);
}
