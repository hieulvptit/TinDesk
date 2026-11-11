use crate::Result;
use binachicken_device::{CredentialStore, FileStore, StoredCredentials};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub backend_origin: String,
    /// Private directory containing the SDK's identity.json; operations run as its owner.
    pub identity_path: PathBuf,
    pub helper_socket: PathBuf,
    pub seat: String,
    #[serde(default)]
    pub relay_only: bool,
    /// Where the logged-in user's sync helper connects. Absent disables clipboard/file sync.
    #[serde(default)]
    pub sync_socket: Option<PathBuf>,
    /// Local sshd port that SSH tunnels lead to. Absent disables the tunnel (the default).
    #[serde(default)]
    pub ssh_port: Option<u16>,
}
impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        if bytes.len() > 64 * 1024 {
            return Err("configuration too large".into());
        }
        let config: Self = serde_json::from_slice(&bytes)?;
        config.validate()?;
        Ok(config)
    }
    pub fn validate(&self) -> Result<()> {
        if self.seat != "seat0"
            || !self.identity_path.is_absolute()
            || !self.helper_socket.is_absolute()
        {
            return Err("requires seat0 and absolute identity/helper paths".into());
        }
        if self.ssh_port == Some(0) {
            return Err("ssh port must be nonzero".into());
        }
        if self.sync_socket.as_ref().is_some_and(|p| !p.is_absolute()) {
            return Err("sync socket path must be absolute".into());
        }
        binachicken_device::DeviceApi::new(
            &self.backend_origin,
            ed25519_dalek::SigningKey::from_bytes(&[0; 32]),
        )?;
        Ok(())
    }
    pub fn credentials(&self) -> Result<StoredCredentials> {
        FileStore::new(&self.identity_path)
            .load()?
            .ok_or_else(|| "device is not enrolled".into())
    }
    pub fn revoked_path(&self) -> PathBuf {
        self.identity_path.join("revoked")
    }
    pub fn lock(&self) -> Result<File> {
        // SDK validates directory ownership/mode before we open a fixed local filename.
        FileStore::new(&self.identity_path).load()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(self.identity_path.join("agent.lock"))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("agent or enrollment is already running".into());
        }
        Ok(lock)
    }
}
