use crate::Notice;
use binachicken_protocol::{CLIPBOARD_TEXT_LIMIT, MAX_BATCH_FILES, Sink, SyncKind};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Total bytes of staged files kept on disk before new transfers are refused.
const STAGING_QUOTA: u64 = 4 * 1024 * 1024 * 1024;

/// The platform clipboard. `set_files` receives absolute paths of fully verified files.
pub trait Clipboard {
    fn set_text(&mut self, text: String) -> io::Result<()>;
    fn set_files(&mut self, paths: &[PathBuf]) -> io::Result<()>;
}

enum Incoming {
    Text(Vec<u8>),
    File {
        dir: PathBuf,
        path: PathBuf,
        file: File,
    },
}

/// Stages received files under `root/<random>/<name>`: the sanitized name never
/// chooses a directory, the random directory never collides, and nothing existing
/// is overwritten or followed.
pub struct ClipboardSink<C: Clipboard> {
    root: PathBuf,
    pub clipboard: C,
    incoming: HashMap<u32, Incoming>,
    finished: Vec<(u32, PathBuf)>,
    notices: Vec<Notice>,
}

impl<C: Clipboard> ClipboardSink<C> {
    pub fn new(root: PathBuf, clipboard: C) -> Self {
        Self {
            root,
            clipboard,
            incoming: HashMap::new(),
            finished: Vec::new(),
            notices: Vec::new(),
        }
    }
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }
    fn make_root(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }
}

fn dir_size(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| match e.metadata() {
            Ok(m) if m.is_dir() => dir_size(&e.path()),
            Ok(m) => m.len(),
            Err(_) => 0,
        })
        .sum()
}

/// Remove staged transfers older than `max_age`. Only direct children of `root` are touched.
pub fn purge_older_than(root: &Path, max_age: Duration) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > max_age);
        // symlink_metadata via file_type: never follow a planted link out of root.
        if old && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

impl<C: Clipboard> Sink for ClipboardSink<C> {
    fn begin(&mut self, id: u32, kind: SyncKind, name: &str, size: u64) -> io::Result<()> {
        let incoming = match kind {
            SyncKind::Text => Incoming::Text(Vec::new()),
            SyncKind::File => {
                self.make_root()?;
                if dir_size(&self.root).saturating_add(size) > STAGING_QUOTA {
                    return Err(io::Error::other("staging quota exceeded"));
                }
                let dir = self.root.join(uuid::Uuid::new_v4().simple().to_string());
                fs::create_dir(&dir)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
                }
                let path = dir.join(name);
                let file = OpenOptions::new().write(true).create_new(true).open(&path);
                match file {
                    Ok(file) => Incoming::File { dir, path, file },
                    Err(e) => {
                        let _ = fs::remove_dir_all(&dir);
                        return Err(e);
                    }
                }
            }
        };
        self.incoming.insert(id, incoming);
        Ok(())
    }
    fn write(&mut self, id: u32, data: &[u8]) -> io::Result<()> {
        match self.incoming.get_mut(&id) {
            Some(Incoming::Text(buf)) => {
                buf.extend_from_slice(data);
                Ok(())
            }
            Some(Incoming::File { file, .. }) => file.write_all(data),
            None => Err(io::Error::other("unknown transfer")),
        }
    }
    fn commit(&mut self, id: u32) -> io::Result<()> {
        match self.incoming.remove(&id) {
            Some(Incoming::Text(buf)) => {
                if buf.len() as u64 > CLIPBOARD_TEXT_LIMIT {
                    return Err(io::Error::other("text too large"));
                }
                let text = String::from_utf8(buf).map_err(io::Error::other)?;
                self.clipboard.set_text(text)?;
                self.notices.push(Notice::ReceivedText);
                Ok(())
            }
            Some(Incoming::File { dir, path, file }) => {
                if let Err(e) = file.sync_all() {
                    let _ = fs::remove_dir_all(&dir);
                    return Err(e);
                }
                self.finished.push((id, path));
                // Bound memory: only the latest batch can still be published.
                let excess = self.finished.len().saturating_sub(MAX_BATCH_FILES * 2);
                self.finished.drain(..excess);
                Ok(())
            }
            None => Err(io::Error::other("unknown transfer")),
        }
    }
    fn abort(&mut self, id: u32) {
        if let Some(Incoming::File { dir, .. }) = self.incoming.remove(&id) {
            let _ = fs::remove_dir_all(dir);
        }
    }
    fn publish(&mut self, ids: &[u32]) -> io::Result<()> {
        let mut paths = Vec::with_capacity(ids.len());
        for id in ids {
            let (_, path) = self
                .finished
                .iter()
                .find(|(done, _)| done == id)
                .ok_or_else(|| io::Error::other("publish of unfinished transfer"))?;
            paths.push(path.clone());
        }
        self.clipboard.set_files(&paths)?;
        self.finished.retain(|(id, _)| !ids.contains(id));
        self.notices.push(Notice::ReceivedFiles(paths.len()));
        Ok(())
    }
}
