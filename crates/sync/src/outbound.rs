use crate::Notice;
use binachicken_protocol::*;
use std::{
    collections::VecDeque,
    fs::File,
    io::{Cursor, Read},
    path::PathBuf,
    time::{Duration, Instant},
};

/// Returned by a transport when its send backlog is full; try again on the next poll.
pub struct Busy;

/// No progress for this long after the last byte aborts the transfer.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

enum Source {
    Bytes(Cursor<Vec<u8>>),
    File(std::io::Take<File>),
}
impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::Bytes(c) => c.read(buf),
            Source::File(f) => f.read(buf),
        }
    }
}

struct Job {
    kind: SyncKind,
    name: String,
    size: u64,
    source: Source,
}
struct Batch {
    jobs: VecDeque<Job>,
    publish: bool,
    done: Vec<u32>,
    files: usize,
}
struct Current {
    id: u32,
    sender: SyncSender,
    source: Source,
    finish_sent: bool,
    last_activity: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuedFiles {
    pub queued: usize,
    /// Directories, unreadable and oversize entries are skipped, never sent.
    pub skipped: usize,
}

/// Sends batches strictly one transfer at a time, honoring the receiver's window.
pub struct Outbound {
    next_id: u32,
    batches: VecDeque<Batch>,
    current: Option<Current>,
    pending: VecDeque<Vec<u8>>,
    notices: Vec<Notice>,
}
impl Default for Outbound {
    fn default() -> Self {
        Self::new()
    }
}
impl Outbound {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            batches: VecDeque::new(),
            current: None,
            pending: VecDeque::new(),
            notices: Vec::new(),
        }
    }
    pub fn take_notices(&mut self) -> Vec<Notice> {
        std::mem::take(&mut self.notices)
    }
    /// Batches not yet fully sent, including the one in progress.
    pub fn pending_batches(&self) -> usize {
        self.batches.len()
    }
    pub fn busy(&self) -> bool {
        self.current.is_some() || !self.batches.is_empty()
    }
    pub fn queue_text(&mut self, text: String) -> Result<(), SyncError> {
        let size = text.len() as u64;
        if text.is_empty() || !size_allowed(SyncKind::Text, size) {
            return Err(SyncError::TooLarge);
        }
        self.batches.push_back(Batch {
            jobs: [Job {
                kind: SyncKind::Text,
                name: String::new(),
                size,
                source: Source::Bytes(Cursor::new(text.into_bytes())),
            }]
            .into(),
            publish: false,
            done: vec![],
            files: 0,
        });
        Ok(())
    }
    pub fn queue_files(&mut self, paths: Vec<PathBuf>) -> QueuedFiles {
        let mut jobs = VecDeque::new();
        let mut skipped = 0;
        for path in paths {
            if jobs.len() >= MAX_BATCH_FILES {
                skipped += 1;
                continue;
            }
            // Following a link is intended: the user copied that entry.
            let job = (|| {
                let meta = std::fs::metadata(&path).ok()?;
                let name = sanitize_name(path.file_name()?.to_str()?).ok()?;
                if !meta.is_file() || !size_allowed(SyncKind::File, meta.len()) {
                    return None;
                }
                let file = File::open(&path).ok()?;
                Some(Job {
                    kind: SyncKind::File,
                    name,
                    size: meta.len(),
                    source: Source::File(file.take(meta.len())),
                })
            })();
            match job {
                Some(job) => jobs.push_back(job),
                None => skipped += 1,
            }
        }
        let queued = jobs.len();
        if queued > 0 {
            self.batches.push_back(Batch {
                jobs,
                publish: true,
                done: vec![],
                files: queued,
            });
        }
        QueuedFiles { queued, skipped }
    }
    /// Abort the batch in progress (peer refused, restart, or local failure).
    pub fn reset(&mut self) {
        self.batches.clear();
        self.current = None;
        self.pending.clear();
    }
    fn fail_batch(&mut self, notice: Notice) {
        if let Some(cur) = self.current.take()
            && let Ok(frame) = encode_sync_control(&SyncMessage::Cancel { id: cur.id })
        {
            self.pending.push_back(frame);
        }
        self.batches.pop_front();
        self.notices.push(notice);
    }
    /// Feed back Progress/Done/Reject from the peer.
    pub fn handle(&mut self, message: &SyncMessage) {
        match message {
            SyncMessage::Progress { id, received } => {
                if let Some(cur) = self.current.as_mut().filter(|c| c.id == *id) {
                    cur.sender.on_progress(*received);
                    cur.last_activity = Instant::now();
                }
            }
            SyncMessage::Done { id } if self.current.as_ref().is_some_and(|c| c.id == *id) => {
                self.current = None;
                if let Some(batch) = self.batches.front_mut() {
                    batch.done.push(*id);
                }
            }
            SyncMessage::Reject { id, reason }
                if self.current.as_ref().is_some_and(|c| c.id == *id) =>
            {
                self.current = None;
                self.batches.pop_front();
                self.notices.push(Notice::Refused(*reason));
            }
            _ => {}
        }
    }
    /// Generate and send as much as the window and `send` allow.
    pub fn poll(&mut self, send: &mut dyn FnMut(&[u8]) -> Result<(), Busy>) {
        loop {
            while let Some(frame) = self.pending.front() {
                if send(frame).is_err() {
                    return;
                }
                self.pending.pop_front();
            }
            if let Some(cur) = &self.current {
                if cur.finish_sent {
                    if cur.last_activity.elapsed() > STALL_TIMEOUT {
                        self.fail_batch(Notice::Failed("transfer stalled"));
                        continue;
                    }
                    return;
                }
            } else {
                // Between transfers: finish a completed batch, then start the next job.
                let finished = self.batches.front().is_some_and(|b| b.jobs.is_empty());
                if finished {
                    let batch = self.batches.pop_front().unwrap();
                    if batch.publish
                        && let Ok(frame) =
                            encode_sync_control(&SyncMessage::Publish { ids: batch.done })
                    {
                        self.pending.push_back(frame);
                    }
                    self.notices.push(if batch.files == 0 {
                        Notice::SentText
                    } else {
                        Notice::SentFiles(batch.files)
                    });
                    continue;
                }
                let Some(batch) = self.batches.front_mut() else {
                    return;
                };
                let job = batch.jobs.pop_front().unwrap();
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                match SyncSender::new(id, job.kind, &job.name, job.size) {
                    Ok((sender, offer)) => {
                        self.pending.push_back(encode_sync_control(&offer).unwrap());
                        self.current = Some(Current {
                            id,
                            sender,
                            source: job.source,
                            finish_sent: false,
                            last_activity: Instant::now(),
                        });
                    }
                    Err(_) => self.fail_batch(Notice::Failed("transfer rejected locally")),
                }
                continue;
            }
            // Active transfer: produce at most one frame, then flush it.
            let cur = self.current.as_mut().unwrap();
            if !cur.sender.can_send() {
                return;
            }
            let mut buf = vec![0u8; SYNC_CHUNK_SIZE];
            let mut filled = 0;
            while filled < buf.len() {
                match cur.source.read(&mut buf[filled..]) {
                    Ok(0) => break,
                    Ok(n) => filled += n,
                    Err(_) => {
                        self.fail_batch(Notice::Failed("could not read source"));
                        break;
                    }
                }
            }
            if self.current.is_none() {
                continue;
            }
            let cur = self.current.as_mut().unwrap();
            if filled > 0 {
                match cur.sender.chunk(&buf[..filled]) {
                    Ok(frame) => {
                        cur.last_activity = Instant::now();
                        self.pending.push_back(frame);
                    }
                    Err(_) => self.fail_batch(Notice::Failed("transfer window violated")),
                }
            } else {
                // Source exhausted: the sender verifies the announced size matches.
                let sender = std::mem::replace(
                    &mut cur.sender,
                    SyncSender::new(0, SyncKind::Text, "", 0).unwrap().0,
                );
                cur.finish_sent = true;
                cur.last_activity = Instant::now();
                match sender.finish() {
                    Ok(frame) => self.pending.push_back(frame),
                    Err(_) => self.fail_batch(Notice::Failed("source changed while sending")),
                }
            }
        }
    }
}
