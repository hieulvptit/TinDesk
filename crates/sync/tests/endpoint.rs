use binachicken_protocol::*;
use binachicken_sync::*;
use std::{
    cell::RefCell,
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
};

type Seen = (Vec<String>, Vec<Vec<PathBuf>>);
#[derive(Default, Clone)]
struct Fake(Rc<RefCell<Seen>>);
impl Clipboard for Fake {
    fn set_text(&mut self, t: String) -> std::io::Result<()> {
        self.0.borrow_mut().0.push(t);
        Ok(())
    }
    fn set_files(&mut self, p: &[PathBuf]) -> std::io::Result<()> {
        self.0.borrow_mut().1.push(p.to_vec());
        Ok(())
    }
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("bc-sync-{name}-{}", uuid_like()));
    fs::create_dir_all(&p).unwrap();
    p
}
fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}
const ON: Policy = Policy {
    clipboard: true,
    files: true,
};

struct Pair {
    a: Endpoint<Fake>,
    b: Endpoint<Fake>,
    fa: Fake,
    fb: Fake,
    a_to_b: VecDeque<Vec<u8>>,
    b_to_a: VecDeque<Vec<u8>>,
}
impl Pair {
    fn new(root: &Path, pa: Policy, pb: Policy) -> Self {
        let (fa, fb) = (Fake::default(), Fake::default());
        Self {
            a: Endpoint::new(root.join("a"), fa.clone(), pa),
            b: Endpoint::new(root.join("b"), fb.clone(), pb),
            fa,
            fb,
            a_to_b: VecDeque::new(),
            b_to_a: VecDeque::new(),
        }
    }
    fn run(&mut self) {
        for _ in 0..100_000 {
            let (mut qa, mut qb) = (vec![], vec![]);
            self.a.poll(&mut |f| {
                qa.push(f.to_vec());
                Ok(())
            });
            self.b.poll(&mut |f| {
                qb.push(f.to_vec());
                Ok(())
            });
            self.a_to_b.extend(qa);
            self.b_to_a.extend(qb);
            if self.a_to_b.is_empty()
                && self.b_to_a.is_empty()
                && !self.a.outbound.busy()
                && !self.b.outbound.busy()
            {
                return;
            }
            while let Some(f) = self.a_to_b.pop_front() {
                self.b.on_frame(&f).unwrap();
            }
            while let Some(f) = self.b_to_a.pop_front() {
                self.a.on_frame(&f).unwrap();
            }
        }
        panic!("did not converge");
    }
}

#[test]
fn text_flows_both_ways_and_unicode_survives() {
    let root = tmp("text");
    let mut p = Pair::new(&root, ON, ON);
    p.a.outbound.queue_text("Tiếng Việt 😀\n\0".into()).unwrap();
    p.b.outbound.queue_text("back".into()).unwrap();
    p.run();
    assert_eq!(p.fb.0.borrow().0, vec!["Tiếng Việt 😀\n\0"]);
    assert_eq!(p.fa.0.borrow().0, vec!["back"]);
    assert!(p.a.take_notices().contains(&Notice::SentText));
    assert!(p.b.take_notices().contains(&Notice::ReceivedText));
}

#[test]
fn file_batch_is_staged_verified_and_published_in_order() {
    let root = tmp("files");
    let src = tmp("src");
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    fs::write(src.join("big.bin"), &big).unwrap();
    fs::write(src.join("empty.txt"), b"").unwrap();
    fs::write(src.join("báo cáo.txt"), "nội dung").unwrap();
    fs::create_dir(src.join("folder")).unwrap();
    let mut p = Pair::new(&root, ON, ON);
    let q = p.a.outbound.queue_files(vec![
        src.join("big.bin"),
        src.join("folder"),
        src.join("empty.txt"),
        src.join("báo cáo.txt"),
        src.join("missing"),
    ]);
    assert_eq!(
        q,
        QueuedFiles {
            queued: 3,
            skipped: 2
        }
    );
    p.run();
    let published = p.fb.0.borrow().1.clone();
    assert_eq!(published.len(), 1);
    let names: Vec<_> = published[0]
        .iter()
        .map(|f| f.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["big.bin", "empty.txt", "báo cáo.txt"]);
    assert_eq!(fs::read(&published[0][0]).unwrap(), big);
    assert_eq!(fs::read_to_string(&published[0][2]).unwrap(), "nội dung");
    // Each file lives in its own fresh directory under b's staging root.
    for f in &published[0] {
        assert!(f.starts_with(root.join("b")));
    }
    assert!(p.a.take_notices().contains(&Notice::SentFiles(3)));
    assert!(p.b.take_notices().contains(&Notice::ReceivedFiles(3)));
}

#[test]
fn disabled_receiver_refuses_and_sender_reports_it() {
    let root = tmp("off");
    let mut p = Pair::new(
        &root,
        ON,
        Policy {
            clipboard: false,
            files: false,
        },
    );
    p.a.outbound.queue_text("secret".into()).unwrap();
    p.run();
    assert!(p.fb.0.borrow().0.is_empty());
    assert!(
        p.a.take_notices()
            .contains(&Notice::Refused(RejectReason::Disabled))
    );
    assert!(!root.join("b").exists() || fs::read_dir(root.join("b")).unwrap().next().is_none());
}

#[test]
fn reset_aborts_partial_files_without_leaving_them() {
    let root = tmp("reset");
    let src = tmp("rsrc");
    fs::write(src.join("f"), vec![1u8; 2_000_000]).unwrap();
    let mut p = Pair::new(&root, ON, ON);
    p.a.outbound.queue_files(vec![src.join("f")]);
    // Run a few rounds, then drop the connection mid-transfer.
    for _ in 0..3 {
        let mut qa = vec![];
        p.a.poll(&mut |f| {
            qa.push(f.to_vec());
            Ok(())
        });
        for f in qa {
            p.b.on_frame(&f).unwrap();
        }
    }
    p.b.reset();
    p.a.reset();
    let leftover: Vec<_> = fs::read_dir(root.join("b"))
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(leftover.is_empty(), "partial file left behind");
    assert!(!p.a.outbound.busy());
}

#[test]
fn busy_transport_applies_backpressure_without_losing_frames() {
    let root = tmp("busy");
    let mut p = Pair::new(&root, ON, ON);
    p.a.outbound.queue_text("hello".into()).unwrap();
    let mut refused = 0;
    p.a.poll(&mut |_| {
        refused += 1;
        Err(Busy)
    });
    assert_eq!(refused, 1);
    p.run();
    assert_eq!(p.fb.0.borrow().0, vec!["hello"]);
}

#[test]
fn purge_removes_only_old_staged_directories() {
    let root = tmp("purge");
    fs::create_dir(root.join("old")).unwrap();
    fs::write(root.join("keep.txt"), "x").unwrap();
    purge_older_than(&root, std::time::Duration::from_secs(3600));
    assert!(root.join("old").exists(), "fresh dir must stay");
    purge_older_than(&root, std::time::Duration::ZERO);
    std::thread::sleep(std::time::Duration::from_millis(5));
    purge_older_than(&root, std::time::Duration::ZERO);
    assert!(!root.join("old").exists());
    assert!(
        root.join("keep.txt").exists(),
        "plain files are never removed"
    );
}

#[test]
fn frame_reader_handles_split_batched_and_oversize_input() {
    let mut wire = vec![];
    write_frame(&mut wire, b"hello").unwrap();
    write_frame(&mut wire, b"").unwrap();
    write_frame(&mut wire, b"world!").unwrap();
    let mut r = FrameReader::default();
    let mut out = vec![];
    for byte in &wire {
        out.extend(r.push(&[*byte]).unwrap());
    }
    assert_eq!(out, vec![b"hello".to_vec(), vec![], b"world!".to_vec()]);
    assert_eq!(FrameReader::default().push(&wire).unwrap().len(), 3);
    let huge = ((SYNC_FRAME_LIMIT + 1) as u32).to_be_bytes();
    assert!(FrameReader::default().push(&huge).is_err());
    assert!(write_frame(&mut vec![], &vec![0; SYNC_FRAME_LIMIT + 1]).is_err());
}
