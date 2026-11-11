//! Real X11 integration: needs `DISPLAY` (Xvfb is enough) and `xclip`; skipped otherwise.
#![cfg(target_os = "linux")]
use binachicken_protocol::Policy;
use binachicken_sync::{Busy, Clipboard, Endpoint, FrameReader, write_frame};
use binachicken_sync_helper::{bridge, x11clip};
use std::{
    cell::RefCell,
    fs,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    rc::Rc,
    time::{Duration, Instant},
};

type Seen = (Vec<String>, Vec<Vec<PathBuf>>);
#[derive(Default, Clone)]
struct Rec(Rc<RefCell<Seen>>);
impl Clipboard for Rec {
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
    let p = std::env::temp_dir().join(format!("bc-x11-{name}-{}", std::process::id()));
    fs::create_dir_all(&p).unwrap();
    p
}
fn xclip_out(target: &str) -> String {
    let o = Command::new("xclip")
        .args(["-o", "-selection", "clipboard", "-t", target])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}
/// xclip daemonizes to keep ownership, like a real application would.
fn xclip_set(target: &str, data: &[u8]) {
    let mut c = Command::new("xclip")
        .args(["-i", "-selection", "clipboard", "-t", target])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    c.stdin.take().unwrap().write_all(data).unwrap();
    c.wait().unwrap();
}

struct Agent {
    ep: Endpoint<Rec>,
    stream: UnixStream,
    reader: FrameReader,
    rec: Rec,
}
impl Agent {
    fn pump(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        let mut buf = [0u8; 16384];
        while Instant::now() < end {
            if let Ok(n) = self.stream.read(&mut buf) {
                assert!(n > 0, "helper closed the session");
                for f in self.reader.push(&buf[..n]).unwrap() {
                    self.ep.on_frame(&f).unwrap();
                }
            }
            let stream = &mut self.stream;
            self.ep
                .poll(&mut |f| write_frame(stream, f).map_err(|_| Busy));
        }
    }
}

fn setup() -> Option<Agent> {
    std::env::var_os("DISPLAY")?;
    let sock = tmp("sock").join("sync.sock");
    let _ = fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).unwrap();
    let staging = tmp("staging");
    let (clip, events) = x11clip::start().expect("X11 clipboard");
    std::thread::spawn(move || {
        let stream = UnixStream::connect(&sock).unwrap();
        let _ = bridge::run_session(stream, clip, &events, &staging);
    });
    let (stream, _) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(10)))
        .unwrap();
    let rec = Rec::default();
    let ep = Endpoint::new(
        tmp("agent-staging"),
        rec.clone(),
        Policy {
            clipboard: true,
            files: true,
        },
    );
    Some(Agent {
        ep,
        stream,
        reader: FrameReader::default(),
        rec,
    })
}

#[test]
fn clipboard_text_and_files_cross_x11_both_ways() {
    let Some(mut a) = setup() else {
        return eprintln!("skipped: no DISPLAY");
    };
    a.pump(300);

    // remote -> X11 text, including multibyte and newlines
    a.ep.outbound
        .queue_text("xin chào 😀\nhàng 2".into())
        .unwrap();
    a.pump(600);
    assert_eq!(xclip_out("UTF8_STRING"), "xin chào 😀\nhàng 2");

    // X11 -> remote text
    xclip_set("UTF8_STRING", "từ Ubuntu 🐧".as_bytes());
    a.pump(1200);
    assert_eq!(
        a.rec.0.borrow().0.last().map(String::as_str),
        Some("từ Ubuntu 🐧")
    );
    let echoes = a.rec.0.borrow().0.len();
    a.pump(800);
    assert_eq!(a.rec.0.borrow().0.len(), echoes, "no echo loop");

    // remote -> X11 files: a file manager would paste these
    let src = tmp("src");
    let big: Vec<u8> = (0..2_500_000u32).map(|i| (i % 253) as u8).collect();
    fs::write(src.join("báo cáo.bin"), &big).unwrap();
    fs::write(src.join("b.txt"), "hello").unwrap();
    a.ep.outbound
        .queue_files(vec![src.join("báo cáo.bin"), src.join("b.txt")]);
    a.pump(4000);
    let uris = xclip_out("text/uri-list");
    let lines: Vec<&str> = uris.lines().collect();
    assert_eq!(lines.len(), 2, "uri-list: {uris:?}");
    assert!(lines[0].starts_with("file://") && lines[0].ends_with("/b%C3%A1o%20c%C3%A1o.bin"));
    let gnome = xclip_out("x-special/gnome-copied-files");
    assert!(gnome.starts_with("copy\nfile://"), "{gnome:?}");
    let first = Path::new(&lines[0]["file://".len()..]).to_path_buf();
    let decoded = binachicken_sync_helper::uri::parse_list(&uris);
    assert_eq!(fs::read(&decoded[0]).unwrap(), big);
    assert_eq!(fs::read_to_string(&decoded[1]).unwrap(), "hello");
    let _ = first;

    // X11 -> remote files
    let up = tmp("up");
    fs::write(up.join("lên.txt"), "gửi lên").unwrap();
    let list = binachicken_sync_helper::uri::uri_list(&[up.join("lên.txt")]);
    xclip_set("text/uri-list", list.as_bytes());
    a.pump(2500);
    let got = a.rec.0.borrow().1.last().cloned().expect("files published");
    assert_eq!(got[0].file_name().unwrap(), "lên.txt");
    assert_eq!(fs::read_to_string(&got[0]).unwrap(), "gửi lên");
}
