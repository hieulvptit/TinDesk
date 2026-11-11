use binachicken_client::clipboard::*;
use std::{
    collections::VecDeque,
    fs, io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct State {
    count: i64,
    content: Option<Content>,
    secret: bool,
}
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<State>>);
impl Fake {
    /// A user (or app) copying something.
    fn copy(&self, c: Content, secret: bool) {
        let mut s = self.0.lock().unwrap();
        s.count += 1;
        s.content = Some(c);
        s.secret = secret;
    }
    fn current(&self) -> Option<Content> {
        self.0.lock().unwrap().content.clone()
    }
}
impl Pasteboard for Fake {
    fn change_count(&self) -> i64 {
        self.0.lock().unwrap().count
    }
    fn read(&self) -> Option<Content> {
        let s = self.0.lock().unwrap();
        if s.secret { None } else { s.content.clone() }
    }
    fn write_text(&mut self, t: &str) -> io::Result<()> {
        self.copy(Content::Text(t.into()), false);
        Ok(())
    }
    fn write_files(&mut self, p: &[PathBuf]) -> io::Result<()> {
        self.copy(Content::Files(p.to_vec()), false);
        Ok(())
    }
}

type Frames = VecDeque<Vec<u8>>;
#[derive(Default)]
struct Wire(Mutex<(Frames, Frames)>);
struct End<'a>(&'a Wire, bool);
impl SyncLink for End<'_> {
    fn send(&self, f: &[u8]) -> Result<(), LinkError> {
        let mut w = self.0.0.lock().unwrap();
        if self.1 {
            w.0.push_back(f.to_vec())
        } else {
            w.1.push_back(f.to_vec())
        }
        Ok(())
    }
    fn recv(&self) -> Option<Vec<u8>> {
        let mut w = self.0.0.lock().unwrap();
        if self.1 {
            w.1.pop_front()
        } else {
            w.0.pop_front()
        }
    }
}

fn tmp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "bc-client-{name}-{}-{:?}",
        std::process::id(),
        std::time::Instant::now()
    ));
    fs::create_dir_all(&p).unwrap();
    p
}
struct Rig {
    a: SyncController<Fake>,
    b: SyncController<Fake>,
    fa: Fake,
    fb: Fake,
    wire: Wire,
}
impl Rig {
    fn new(fa: Fake, fb: Fake) -> Self {
        Self {
            a: SyncController::new(tmp("a"), fa.clone()),
            b: SyncController::new(tmp("b"), fb.clone()),
            fa,
            fb,
            wire: Wire::default(),
        }
    }
    fn run(&mut self) {
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(30));
            self.a.tick(&End(&self.wire, true)).ok().unwrap();
            self.b.tick(&End(&self.wire, false)).ok().unwrap();
        }
    }
}

#[test]
fn text_and_files_cross_once_without_echo_and_secrets_stay_local() {
    // Isolated HOME so the on/off marker never touches the real one.
    let home = tmp("home");
    unsafe { std::env::set_var("HOME", &home) };
    let (fa, fb) = (Fake::default(), Fake::default());
    // Copied before the session: must not be sent.
    fa.copy(Content::Text("old".into()), false);
    let mut rig = Rig::new(fa.clone(), fb.clone());
    rig.run();
    assert_eq!(rig.fb.current(), None);

    // Text.
    rig.fa.copy(Content::Text("xin chào 😀".into()), false);
    rig.run();
    assert_eq!(rig.fb.current(), Some(Content::Text("xin chào 😀".into())));
    let b_count = rig.fb.change_count();
    rig.run();
    assert_eq!(rig.fb.change_count(), b_count, "no echo loop");
    assert_eq!(rig.fa.current(), Some(Content::Text("xin chào 😀".into())));

    // Secret (password manager) is never sent.
    rig.fa.copy(Content::Text("hunter2".into()), true);
    rig.run();
    assert_eq!(rig.fb.current(), Some(Content::Text("xin chào 😀".into())));

    // Files.
    let src = tmp("src");
    fs::write(src.join("a.txt"), "hello").unwrap();
    rig.fa.copy(Content::Files(vec![src.join("a.txt")]), false);
    rig.run();
    let Some(Content::Files(got)) = rig.fb.current() else {
        panic!("no files")
    };
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].file_name().unwrap(), "a.txt");
    assert_eq!(fs::read_to_string(&got[0]).unwrap(), "hello");

    // Switched off: nothing leaves, nothing is accepted, and it persists.
    set_enabled(false);
    assert!(
        home.join("Library/Application Support/BinaChicken/clipboard-sync-off")
            .exists()
    );
    rig.fa.copy(Content::Text("private".into()), false);
    rig.run();
    assert!(!matches!(rig.fb.current(), Some(Content::Text(t)) if t == "private"));
    set_enabled(true);
    assert!(
        !home
            .join("Library/Application Support/BinaChicken/clipboard-sync-off")
            .exists()
    );
}
