//! Private root-to-root recovery channel. No input text or durable journal.
use crate::{Result, error, ipc::FrameDecoder, seat::DisplayTarget, x11};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::Path,
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        xinput::{ConnectionExt as _, Device, DeviceType, InputStateData},
        xkb::{self, ConnectionExt as _},
        xproto::{self, ConnectionExt as _, CreateWindowAux, PropMode, WindowClass},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
const TIMEOUT: Duration = Duration::from_millis(100);
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub keys: BTreeSet<u8>,
    pub buttons: BTreeSet<u8>,
    pub text: Option<TextRestore>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TextRestore {
    pub code: u8,
    pub original: Vec<u32>,
    pub expected: Vec<Vec<u32>>,
    pub local_modifiers: BTreeSet<u8>,
    pub locked_mods: u8,
    pub locked_group: u8,
}
#[derive(Serialize, Deserialize)]
enum Request {
    Begin {
        target: DisplayTarget,
        deadline: u64,
    },
    Ready,
    State(Snapshot),
    Renew {
        deadline: u64,
    },
}
#[derive(Serialize, Deserialize)]
enum Reply {
    Busy,
    Anchor {
        window: u32,
        atom: u32,
        nonce: Vec<u8>,
    },
    Ack,
}
#[cfg(target_os = "linux")]
pub fn boot_time_ms() -> Result<u64> {
    let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((ts.tv_sec as u64).saturating_mul(1000) + (ts.tv_nsec as u64) / 1_000_000)
}
#[cfg(not(target_os = "linux"))]
pub fn boot_time_ms() -> Result<u64> {
    Err(error("guardian requires Linux CLOCK_BOOTTIME"))
}
fn deadline_valid(deadline: u64) -> Result<()> {
    let now = boot_time_ms()?;
    if deadline <= now || deadline - now > 60_000 {
        return Err(error("invalid guardian lease deadline"));
    }
    Ok(())
}
fn send<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = remotevnc_protocol::encode_control(value)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes())?;
    stream.write_all(&bytes)?;
    Ok(())
}
fn receive<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> Result<T> {
    let mut size = [0; 4];
    stream.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size == 0 || size > remotevnc_protocol::CONTROL_PAYLOAD_LIMIT {
        return Err(error("invalid guardian frame"));
    }
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes)?;
    Ok(remotevnc_protocol::decode_control(&bytes)?)
}
pub(crate) struct Client {
    stream: UnixStream,
}
impl Client {
    pub fn connect(
        c: &RustConnection,
        target: &DisplayTarget,
        path: &Path,
        deadline: u64,
    ) -> Result<Self> {
        deadline_valid(deadline)?;
        let mut stream = UnixStream::connect(path)?;
        // Production helper and guardian both run as root. Tests may exercise
        // this trusted library boundary as their own UID; no CLI bypass exists.
        crate::ipc::authorize_uid(crate::ipc::peer_uid(&stream)?, unsafe { libc::geteuid() })?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        send(
            &mut stream,
            &Request::Begin {
                target: target.clone(),
                deadline,
            },
        )?;
        let (window, atom, nonce) = match receive(&mut stream)? {
            Reply::Anchor {
                window,
                atom,
                nonce,
            } => (window, atom, nonce),
            Reply::Busy => return Err(error("input recovery pending or guardian busy")),
            Reply::Ack => return Err(error("guardian anchor required")),
        };
        if nonce.len() != 16 {
            return Err(error("invalid guardian nonce"));
        }
        let property = c
            .get_property(false, window, atom, xproto::AtomEnum::STRING, 0, 16)?
            .reply()?;
        if property.value != nonce || property.bytes_after != 0 {
            return Err(error("guardian X server mismatch"));
        }
        send(&mut stream, &Request::Ready)?;
        if !matches!(receive::<Reply>(&mut stream)?, Reply::Ack) {
            return Err(error("guardian acknowledgement required"));
        }
        Ok(Self { stream })
    }
    pub fn state(&mut self, snapshot: &Snapshot) -> Result<()> {
        send(&mut self.stream, &Request::State(snapshot.clone()))?;
        if !matches!(receive::<Reply>(&mut self.stream)?, Reply::Ack) {
            return Err(error("guardian acknowledgement required"));
        }
        Ok(())
    }
    pub fn healthy(&self) -> bool {
        use std::os::fd::AsRawFd;
        let mut byte = 0u8;
        let n = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                (&mut byte as *mut u8).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        n < 0
            && matches!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
            )
    }
    pub fn renew(&mut self, deadline: u64) -> Result<()> {
        deadline_valid(deadline)?;
        send(&mut self.stream, &Request::Renew { deadline })?;
        if !matches!(receive::<Reply>(&mut self.stream)?, Reply::Ack) {
            return Err(error("guardian acknowledgement required"));
        }
        Ok(())
    }
}
#[derive(Default)]
pub(crate) struct PhysicalState {
    pub keys: BTreeSet<u8>,
    pub buttons: BTreeSet<u8>,
}
/// Admission performs this query as well as recovery. Unsupported source state
/// fails admission, rather than discovering lost ownership after helper death.
pub(crate) fn physical_state(c: &RustConnection) -> Result<PhysicalState> {
    c.xinput_xi_query_version(2, 0)?.reply()?;
    let devices = c.xinput_xi_query_device(Device::ALL)?.reply()?.infos;
    let mut masters = devices
        .iter()
        .filter(|d| d.type_ == DeviceType::MASTER_POINTER);
    let master = masters.next().ok_or("no master pointer")?;
    if masters.next().is_some() {
        return Err(error("multiple master pointers unsupported"));
    }
    let name = String::from_utf8(master.name.clone())?;
    let prefix = name
        .strip_suffix(" pointer")
        .ok_or("unsupported XTEST source naming")?;
    let xtest_pointer = format!("{prefix} XTEST pointer");
    let xtest_keyboard = format!("{prefix} XTEST keyboard");
    if !devices
        .iter()
        .any(|d| d.name == xtest_pointer.as_bytes() && d.attachment == master.deviceid)
        || !devices
            .iter()
            .any(|d| d.name == xtest_keyboard.as_bytes() && d.attachment == master.attachment)
    {
        return Err(error("XTEST sources unavailable"));
    }
    let mut state = PhysicalState::default();
    for d in &devices {
        if d.type_ == DeviceType::MASTER_POINTER
            || d.type_ == DeviceType::MASTER_KEYBOARD
            || d.name == xtest_pointer.as_bytes()
            || d.name == xtest_keyboard.as_bytes()
        {
            continue;
        }
        let id = u8::try_from(d.deviceid).map_err(|_| error("source cannot expose XI1 state"))?;
        for class in c.xinput_query_device_state(id)?.reply()?.classes {
            match class.data {
                InputStateData::Key(k) => {
                    for code in 0..=255u8 {
                        if k.keys[code as usize / 8] & (1 << (code % 8)) != 0 {
                            state.keys.insert(code);
                        }
                    }
                }
                InputStateData::Button(b) => {
                    for code in 0..=255u8 {
                        if b.buttons[code as usize / 8] & (1 << (code % 8)) != 0 {
                            state.buttons.insert(code);
                        }
                    }
                }
                InputStateData::Valuator(_) => {}
                InputStateData::InvalidValue(_) => {
                    return Err(error("unsupported physical input state"));
                }
            }
        }
    }
    Ok(state)
}
fn validate(snapshot: &Snapshot, c: &RustConnection) -> Result<()> {
    let setup = c.setup();
    if snapshot
        .keys
        .iter()
        .any(|k| *k < setup.min_keycode || *k > setup.max_keycode)
        || snapshot.buttons.iter().any(|b| !(1..=9).contains(b))
    {
        return Err(error("invalid recovery ownership"));
    }
    if let Some(t) = &snapshot.text
        && (t.code < setup.min_keycode
            || t.code > setup.max_keycode
            || t.original.is_empty()
            || t.original.iter().any(|v| *v != 0)
            || t.original.len() > 255
            || t.expected.is_empty()
            || t.expected.len() > 2
            || t.expected.iter().any(|m| m.len() != t.original.len())
            || t.local_modifiers.len() > 32
            || t.locked_group > 3)
    {
        return Err(error("invalid text recovery state"));
    }
    Ok(())
}
fn fake(c: &RustConnection, kind: u8, code: u8) -> Result<()> {
    c.xtest_fake_input(kind, code, 0, c.setup().roots[0].root, 0, 0, 0)?
        .check()?;
    Ok(())
}
/// Returns false while physical ownership prevents releasing a residual remote
/// source. The caller keeps the snapshot and connection until that owner lets go.
/// X11 queries and XTest mutations are not atomic: a concurrent physical change
/// between them can still be affected. These checks preserve observed ownership;
/// they are not a race-free arbitration mechanism.
pub(crate) fn restore(c: &RustConnection, snapshot: &mut Snapshot) -> Result<bool> {
    if let Some(mut t) = snapshot.text.clone() {
        let current = c.get_keyboard_mapping(t.code, 1)?.reply()?;
        // XKB expands core groups and zero-pads the requested repeated-symbol
        // map. Only the formerly empty spare and recorded symbols are eligible.
        let mapping_owned = t.expected.iter().any(|expected| {
            current
                .keysyms
                .iter()
                .all(|sym| *sym == 0 || expected.contains(sym))
        });
        let mut mapping_pending = mapping_owned && physical_state(c)?.keys.contains(&t.code);
        if mapping_owned && !mapping_pending {
            fake(c, xproto::KEY_RELEASE_EVENT, t.code)?;
            mapping_pending = physical_state(c)?.keys.contains(&t.code);
            if !mapping_pending {
                c.change_keyboard_mapping(1, t.code, t.original.len() as u8, &t.original)?
                    .check()?;
            }
        }
        let down = c.query_keymap()?.reply()?.keys;
        for code in &t.local_modifiers {
            if physical_state(c)?.keys.contains(code) {
                // Restoring the core modifier through XTEST creates ownership
                // that must remain pending until the physical source lets go.
                snapshot.keys.insert(*code);
                if down[*code as usize / 8] & (1 << (*code % 8)) == 0 {
                    fake(c, xproto::KEY_PRESS_EVENT, *code)?;
                }
            }
        }
        c.xkb_use_extension(1, 0)?.reply()?;
        let state = c.xkb_get_state(256)?.reply()?;
        if u16::from(state.locked_mods) == 0 && u8::from(state.locked_group) == 0 {
            c.xkb_latch_lock_state(
                256,
                xproto::ModMask::from(255u8),
                xproto::ModMask::from(t.locked_mods),
                true,
                xkb::Group::from(t.locked_group),
                xproto::ModMask::from(0u8),
                false,
                0,
            )?
            .check()?;
        }
        if mapping_pending {
            t.local_modifiers.clear();
            t.locked_mods = 0;
            t.locked_group = 0;
            snapshot.text = Some(t);
        } else {
            snapshot.text = None;
        }
    }
    for code in snapshot.keys.clone() {
        if !physical_state(c)?.keys.contains(&code) {
            fake(c, xproto::KEY_RELEASE_EVENT, code)?;
            snapshot.keys.remove(&code);
        }
    }
    for button in snapshot.buttons.clone() {
        if !physical_state(c)?.buttons.contains(&button) {
            fake(c, xproto::BUTTON_RELEASE_EVENT, button)?;
            snapshot.buttons.remove(&button);
        }
    }
    Ok(snapshot.keys.is_empty() && snapshot.buttons.is_empty() && snapshot.text.is_none())
}
/// One already-authenticated root helper connection. Tests use this same server
/// directly with a private socket; the production listener enforces root UID.
pub fn serve(stream: &mut UnixStream) -> Result<()> {
    serve_until(stream, &std::sync::atomic::AtomicBool::new(false))
}
fn serve_until(stream: &mut UnixStream, stop: &std::sync::atomic::AtomicBool) -> Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let Request::Begin {
        target,
        mut deadline,
    } = receive(stream)?
    else {
        return Err(error("guardian Begin required"));
    };
    deadline_valid(deadline)?;
    let c = x11::connect(&target)?;
    physical_state(&c)?;
    let root = c.setup().roots[0].root;
    let window = c.generate_id()?;
    c.create_window(
        0,
        window,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?
    .check()?;
    let atom = c
        .intern_atom(false, b"_REMOTEVNC_GUARDIAN_ANCHOR")?
        .reply()?
        .atom;
    let nonce = uuid::Uuid::new_v4().as_bytes().to_vec();
    c.change_property8(
        PropMode::REPLACE,
        window,
        atom,
        xproto::AtomEnum::STRING,
        &nonce,
    )?
    .check()?;
    send(
        stream,
        &Reply::Anchor {
            window,
            atom,
            nonce,
        },
    )?;
    if !matches!(receive::<Request>(stream)?, Request::Ready) {
        return Err(error("guardian Ready required"));
    }
    send(stream, &Reply::Ack)?;
    stream.set_read_timeout(Some(Duration::from_millis(5)))?;
    let mut snapshot = Snapshot::default();
    let mut decoder = FrameDecoder::default();
    let mut buffer = [0; 4096];
    let result = (|| -> Result<()> {
        while boot_time_ms()? < deadline && !stop.load(std::sync::atomic::Ordering::Relaxed) {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    for bytes in decoder.push(&buffer[..n])? {
                        // A live guardian connection and window disappear on reset;
                        // never reconnect and restore a replacement X server.
                        c.get_geometry(window)?.reply()?;
                        if boot_time_ms()? >= deadline {
                            return Err(error("guardian lease expired"));
                        }
                        match remotevnc_protocol::decode_control::<Request>(&bytes)? {
                            Request::State(next) => {
                                validate(&next, &c)?;
                                snapshot = next;
                            }
                            Request::Renew { deadline: next } => {
                                deadline_valid(next)?;
                                if next < deadline {
                                    return Err(error("guardian deadline cannot go backwards"));
                                }
                                deadline = next;
                            }
                            _ => return Err(error("unexpected guardian request")),
                        }
                        send(stream, &Reply::Ack)?;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    })();
    let _ = stream.shutdown(std::net::Shutdown::Both);
    loop {
        if restore(&c, &mut snapshot).is_ok_and(|done| done) {
            break;
        }
        if c.get_input_focus()
            .ok()
            .and_then(|r| r.reply().ok())
            .is_none()
        {
            break;
        }
        // A live server with unavailable ownership information stays pending.
        // Never discard the last recovery record on a transient source error.
        std::thread::sleep(Duration::from_millis(10));
    }
    result
}

/// Root-only local listener. The guardian is a separate systemd service/cgroup.
pub fn run(socket: &Path) -> Result<()> {
    use std::{
        fs,
        os::unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::UnixListener,
        },
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };
    if unsafe { libc::geteuid() } != 0 {
        return Err(error("input guardian requires root"));
    }
    let parent = socket.parent().ok_or("guardian socket parent required")?;
    if !parent.exists() {
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        return Err(error("guardian directory must be private root-owned"));
    }
    if let Ok(metadata) = fs::symlink_metadata(socket) {
        if !metadata.file_type().is_socket() || metadata.uid() != 0 {
            return Err(error("invalid guardian socket owner/type"));
        }
        match UnixStream::connect(socket) {
            Ok(_) => return Err(error("input guardian already running")),
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => fs::remove_file(socket)?,
            Err(e) => return Err(e.into()),
        }
    }
    let listener = UnixListener::bind(socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _cleanup = Cleanup(socket.to_path_buf());
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))?;
    let mut worker: Option<std::thread::JoinHandle<()>> = None;
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Relaxed) {
            if worker.as_ref().is_some_and(|w| w.is_finished()) {
                let _ = worker.take().unwrap().join();
            }
            match listener.accept() {
                Ok((mut peer, _)) => {
                    if !crate::ipc::peer_uid(&peer).is_ok_and(|uid| uid == 0) {
                        continue;
                    }
                    if worker.is_some() {
                        peer.set_write_timeout(Some(TIMEOUT))?;
                        let _ = send(&mut peer, &Reply::Busy);
                        continue;
                    }
                    let worker_stop = Arc::clone(&stop);
                    worker = Some(std::thread::spawn(move || {
                        let _ = serve_until(&mut peer, &worker_stop);
                    }));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Relaxed);
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    result
}
