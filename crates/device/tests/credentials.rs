#![cfg(unix)]
use ed25519_dalek::SigningKey;
use remotevnc_device::{CredentialStore, FileStore, StoredCredentials};
use std::os::unix::fs::{PermissionsExt, symlink};

#[test]
fn private_key_and_enrolled_identity_survive_application_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let provisional = StoredCredentials {
        device_id: None,
        signing_key: SigningKey::from_bytes(&[1; 32]),
    };
    FileStore::new(&path).save(&provisional).unwrap();
    let reloaded = FileStore::new(&path).load().unwrap().unwrap();
    assert!(reloaded.device_id.is_none());
    assert_eq!(reloaded.signing_key.to_bytes(), [1; 32]);
    let id = serde_json::from_str("\"abf64b97-9f3d-4a26-ab4a-e7a339832ef4\"").unwrap();
    FileStore::new(&path)
        .save(&StoredCredentials {
            device_id: Some(id),
            ..reloaded
        })
        .unwrap();
    assert_eq!(
        FileStore::new(&path).load().unwrap().unwrap().device_id,
        Some(id)
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(path.join("identity.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
#[test]
fn unsafe_permissions_symlinks_and_corrupt_storage_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials");
    let store = FileStore::new(&path);
    assert!(store.load().unwrap().is_none());
    let credentials = StoredCredentials {
        device_id: None,
        signing_key: SigningKey::from_bytes(&[2; 32]),
    };
    store.save(&credentials).unwrap();
    let file = path.join("identity.json");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&credentials).is_err());
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&file, "malformed secret data").unwrap();
    assert!(store.load().is_err());
    std::fs::remove_file(&file).unwrap();
    let victim = dir.path().join("victim");
    std::fs::write(&victim, "untouched").unwrap();
    symlink(&victim, &file).unwrap();
    assert!(store.load().is_err());
    assert!(store.save(&credentials).is_err());
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    let alias = dir.path().join("alias");
    symlink(&path, &alias).unwrap();
    assert!(FileStore::new(alias).load().is_err());
}
