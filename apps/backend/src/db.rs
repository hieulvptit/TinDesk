use crate::{Error, Result};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use rusqlite::{Connection, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct Database(Arc<Mutex<Connection>>);
impl Database {
    /// Store the database inside a directory accessible only to the service user.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        #[cfg(unix)]
        {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let parent = path.parent().ok_or(Error::Invalid)?;
            if std::fs::metadata(parent)
                .map_err(|_| Error::Internal)?
                .permissions()
                .mode()
                & 0o077
                != 0
            {
                return Err(Error::Invalid);
            }
            if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(Error::Invalid);
            }
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(path)
                .map_err(|_| Error::Internal)?;
            if file
                .metadata()
                .map_err(|_| Error::Internal)?
                .permissions()
                .mode()
                & 0o077
                != 0
            {
                return Err(Error::Invalid);
            }
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.execute_batch(include_str!("../migrations/001_identity.sql"))?;
        connection.execute_batch(include_str!("../migrations/002_sessions.sql"))?;
        Ok(Self(Arc::new(Mutex::new(connection))))
    }
    pub(crate) fn transaction<T>(
        &self,
        f: impl FnOnce(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut connection = self.0.lock().map_err(|_| Error::Internal)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = f(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }
}
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as i64
}
pub(crate) fn random() -> String {
    let mut bytes = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
pub(crate) fn hash(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::Internal
    }
}

/// One backend runtime serializes final socket writes with revocation/close commits.
pub(crate) fn authorization_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    &GATE
}
