use crate::{Error, Result};
use binachicken_protocol::DeviceId;
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

pub struct DeviceIdentity {
    pub device_id: DeviceId,
    pub signing_key: SigningKey,
}
/// Persist the provisional key before beginning enrollment. Before the first completion
/// poll, persist `Enrollment::challenge.device_id` as the candidate `device_id`.
/// After a restart or a lost poll response, try authentication with that candidate ID
/// and key: the backend may already have completed enrollment. If enrollment never
/// completed and its private polling verifier was not retained, start a new enrollment
/// with the same key. An already registered key cannot be enrolled a second time.
pub struct StoredCredentials {
    pub device_id: Option<DeviceId>,
    pub signing_key: SigningKey,
}
pub trait CredentialStore {
    fn load(&self) -> Result<Option<StoredCredentials>>;
    fn save(&self, credentials: &StoredCredentials) -> Result<()>;
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiskCredentials {
    version: u8,
    device_id: Option<DeviceId>,
    secret: [u8; 32],
}
impl Drop for DiskCredentials {
    fn drop(&mut self) {
        self.secret.zeroize();
    }
}
fn encode(credentials: &StoredCredentials) -> Result<Zeroizing<Vec<u8>>> {
    let disk = DiskCredentials {
        version: 1,
        device_id: credentials.device_id,
        secret: credentials.signing_key.to_bytes(),
    };
    serde_json::to_vec(&disk)
        .map(Zeroizing::new)
        .map_err(|_| Error::Storage)
}
fn decode(bytes: &[u8]) -> Result<StoredCredentials> {
    let disk: DiskCredentials = serde_json::from_slice(bytes).map_err(|_| Error::Storage)?;
    if disk.version != 1 {
        return Err(Error::Storage);
    }
    Ok(StoredCredentials {
        device_id: disk.device_id,
        signing_key: SigningKey::from_bytes(&disk.secret),
    })
}

#[cfg(unix)]
pub struct FileStore {
    directory: std::path::PathBuf,
}
#[cfg(unix)]
impl FileStore {
    pub fn new(path: impl AsRef<std::path::Path>) -> Self {
        Self {
            directory: path.as_ref().into(),
        }
    }
    fn directory(&self, create: bool) -> Result<Option<std::fs::File>> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
        if create {
            match std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&self.directory)
            {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(Error::Storage),
            }
        }
        let directory = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&self.directory)
        {
            Ok(file) => file,
            Err(e) if !create && e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Storage),
        };
        let metadata = directory.metadata().map_err(|_| Error::Storage)?;
        // SAFETY: geteuid has no preconditions.
        if metadata.mode() & 0o077 != 0 || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(Error::Storage);
        }
        Ok(Some(directory))
    }
    fn open_record(directory: &std::fs::File) -> Result<Option<std::fs::File>> {
        use std::os::{
            fd::{AsRawFd, FromRawFd},
            unix::fs::MetadataExt,
        };
        // SAFETY: directory is a live FD and the fixed relative name is NUL terminated.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                c"identity.json".as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(Error::Storage)
            };
        }
        // SAFETY: successful openat transfers a new owned FD.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let meta = file.metadata().map_err(|_| Error::Storage)?;
        if !meta.is_file()
            || meta.mode() & 0o077 != 0
            || meta.nlink() != 1
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.len() > 4096
        {
            return Err(Error::Storage);
        }
        Ok(Some(file))
    }
}
#[cfg(unix)]
impl CredentialStore for FileStore {
    fn load(&self) -> Result<Option<StoredCredentials>> {
        use std::io::Read;
        let Some(directory) = self.directory(false)? else {
            return Ok(None);
        };
        let Some(file) = Self::open_record(&directory)? else {
            return Ok(None);
        };
        let mut bytes = Zeroizing::new(Vec::new());
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Storage)?;
        if bytes.len() > 4096 {
            return Err(Error::Storage);
        }
        decode(&bytes).map(Some)
    }
    fn save(&self, credentials: &StoredCredentials) -> Result<()> {
        use std::{
            io::Write,
            os::fd::{AsRawFd, FromRawFd},
        };
        let directory = self.directory(true)?.ok_or(Error::Storage)?;
        Self::open_record(&directory)?;
        let name = std::ffi::CString::new(format!(".identity-{:016x}.tmp", rand::random::<u64>()))
            .map_err(|_| Error::Storage)?;
        // SAFETY: directory is live, name is relative/NUL terminated; mode accompanies O_CREAT.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(Error::Storage);
        }
        // SAFETY: successful openat transfers a new owned FD.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let result = (|| {
            file.write_all(&encode(credentials)?)
                .map_err(|_| Error::Storage)?;
            file.sync_all().map_err(|_| Error::Storage)?;
            // SAFETY: live directory FD and fixed valid relative names, atomic replacement.
            if unsafe {
                libc::renameat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    directory.as_raw_fd(),
                    c"identity.json".as_ptr(),
                )
            } != 0
            {
                return Err(Error::Storage);
            }
            directory.sync_all().map_err(|_| Error::Storage)
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
        }
        result
    }
}

#[cfg(target_os = "macos")]
pub struct KeychainStore {
    service: String,
    account: String,
    helper: Option<std::path::PathBuf>,
}
#[cfg(target_os = "macos")]
impl KeychainStore {
    /// Use a stable application service and account, scoped to the backend origin.
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
            helper: None,
        }
    }
    /// A bundled stable helper avoids per-build cdhash partitions for local signing.
    /// The helper verifies the client's pinned code signature before accessing this item.
    pub fn with_helper(account: impl Into<String>, helper: std::path::PathBuf) -> Self {
        Self {
            service: "io.binachicken.client".into(),
            account: account.into(),
            helper: Some(helper),
        }
    }
    fn helper(&self, action: &str, input: Option<&[u8]>) -> Result<Option<Zeroizing<Vec<u8>>>> {
        use std::{
            io::Write,
            process::{Command, Stdio},
        };
        let mut child = Command::new(self.helper.as_ref().ok_or(Error::Storage)?)
            .args([action, &self.account])
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| Error::Storage)?;
        if let Some(input) = input {
            let result = child.stdin.take().ok_or(Error::Storage)?.write_all(input);
            if result.is_err() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Storage);
            }
        }
        let output = child.wait_with_output().map_err(|_| Error::Storage)?;
        let bytes = Zeroizing::new(output.stdout);
        if output.status.success() && bytes.len() <= 4096 {
            Ok(Some(bytes))
        } else if action == "read" && output.status.code() == Some(3) {
            Ok(None)
        } else {
            Err(Error::Storage)
        }
    }
    pub fn delete(&self) -> Result<()> {
        if self.helper.is_some() {
            self.helper("delete", None).map(|_| ())
        } else {
            match security_framework::passwords::delete_generic_password(
                &self.service,
                &self.account,
            ) {
                Ok(()) => Ok(()),
                Err(e) if e.code() == -25300 => Ok(()),
                Err(_) => Err(Error::Storage),
            }
        }
    }
}
#[cfg(target_os = "macos")]
impl CredentialStore for KeychainStore {
    fn load(&self) -> Result<Option<StoredCredentials>> {
        if self.helper.is_some() {
            return self
                .helper("read", None)?
                .map(|bytes| decode(&bytes))
                .transpose();
        }
        match security_framework::passwords::get_generic_password(&self.service, &self.account) {
            Ok(bytes) => decode(&Zeroizing::new(bytes)).map(Some),
            Err(error) if error.code() == -25300 => Ok(None),
            Err(_) => Err(Error::Storage),
        }
    }
    fn save(&self, credentials: &StoredCredentials) -> Result<()> {
        if self.helper.is_some() {
            return self
                .helper("write", Some(&encode(credentials)?))
                .map(|_| ());
        }
        security_framework::passwords::set_generic_password(
            &self.service,
            &self.account,
            &encode(credentials)?,
        )
        .map_err(|_| Error::Storage)
    }
}
