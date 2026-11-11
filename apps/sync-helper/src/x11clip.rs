//! X11 CLIPBOARD owner and watcher. One thread owns the connection; the rest of the
//! helper talks to it through channels. Large (INCR) selections are ignored, never guessed.
use crate::uri;
use binachicken_protocol::CLIPBOARD_TEXT_LIMIT;
use binachicken_sync::Clipboard;
use std::{
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xfixes::{self, ConnectionExt as _},
        xproto::*,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

const FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// What another X11 application put on the clipboard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipEvent {
    Text(String),
    Files(Vec<PathBuf>),
}

enum Command {
    Text(String),
    Files(Vec<PathBuf>),
}

/// Cloneable handle; implements the sync crate's `Clipboard`.
#[derive(Clone)]
pub struct X11Clipboard {
    commands: Sender<Command>,
}
impl Clipboard for X11Clipboard {
    fn set_text(&mut self, text: String) -> io::Result<()> {
        self.commands
            .send(Command::Text(text))
            .map_err(|_| io::Error::other("clipboard thread stopped"))
    }
    fn set_files(&mut self, paths: &[PathBuf]) -> io::Result<()> {
        self.commands
            .send(Command::Files(paths.to_vec()))
            .map_err(|_| io::Error::other("clipboard thread stopped"))
    }
}

struct Atoms {
    clipboard: Atom,
    targets: Atom,
    timestamp: Atom,
    incr: Atom,
    utf8: Atom,
    string: Atom,
    plain_utf8: Atom,
    uri_list: Atom,
    gnome: Atom,
    prop: Atom,
}
fn atom(conn: &RustConnection, name: &str) -> io::Result<Atom> {
    Ok(conn
        .intern_atom(false, name.as_bytes())
        .map_err(io::Error::other)?
        .reply()
        .map_err(io::Error::other)?
        .atom)
}

enum Owned {
    Text(String),
    Files(Vec<PathBuf>),
}
#[derive(PartialEq)]
enum Want {
    Targets,
    Text,
    Files,
}
struct Fetch {
    want: Want,
    started: Instant,
}

/// Connects using DISPLAY/XAUTHORITY of the current session.
pub fn start() -> io::Result<(X11Clipboard, Receiver<ClipEvent>)> {
    let (conn, screen_num) = RustConnection::connect(None).map_err(io::Error::other)?;
    let root = conn.setup().roots[screen_num].root;
    let atoms = Atoms {
        clipboard: atom(&conn, "CLIPBOARD")?,
        targets: atom(&conn, "TARGETS")?,
        timestamp: atom(&conn, "TIMESTAMP")?,
        incr: atom(&conn, "INCR")?,
        utf8: atom(&conn, "UTF8_STRING")?,
        string: AtomEnum::STRING.into(),
        plain_utf8: atom(&conn, "text/plain;charset=utf-8")?,
        uri_list: atom(&conn, "text/uri-list")?,
        gnome: atom(&conn, "x-special/gnome-copied-files")?,
        prop: atom(&conn, "BINACHICKEN_SELECTION")?,
    };
    let win = conn.generate_id().map_err(io::Error::other)?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )
    .map_err(io::Error::other)?;
    conn.xfixes_query_version(5, 0)
        .map_err(io::Error::other)?
        .reply()
        .map_err(io::Error::other)?;
    conn.xfixes_select_selection_input(
        win,
        atoms.clipboard,
        xfixes::SelectionEventMask::SET_SELECTION_OWNER,
    )
    .map_err(io::Error::other)?;
    conn.flush().map_err(io::Error::other)?;
    let (commands, command_rx) = mpsc::channel();
    let (event_tx, events) = mpsc::channel();
    thread::Builder::new()
        .name("binachicken-x11-clipboard".into())
        .spawn(move || {
            let _ = run(&conn, win, &atoms, &command_rx, &event_tx);
        })?;
    Ok((X11Clipboard { commands }, events))
}

fn run(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    commands: &Receiver<Command>,
    events: &Sender<ClipEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut owned: Option<Owned> = None;
    let mut fetch: Option<Fetch> = None;
    loop {
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    owned = Some(match command {
                        Command::Text(t) => Owned::Text(t),
                        Command::Files(f) => Owned::Files(f),
                    });
                    fetch = None;
                    conn.set_selection_owner(win, atoms.clipboard, Time::CURRENT_TIME)?;
                    conn.flush()?;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Ok(()),
            }
        }
        let mut idle = true;
        while let Some(event) = conn.poll_for_event()? {
            idle = false;
            match event {
                Event::XfixesSelectionNotify(e) if e.selection == atoms.clipboard => {
                    // Our own ownership changes (and a cleared owner) are never echoed.
                    if e.owner == win || e.owner == x11rb::NONE {
                        continue;
                    }
                    conn.convert_selection(
                        win,
                        atoms.clipboard,
                        atoms.targets,
                        atoms.prop,
                        Time::CURRENT_TIME,
                    )?;
                    fetch = Some(Fetch {
                        want: Want::Targets,
                        started: Instant::now(),
                    });
                    conn.flush()?;
                }
                Event::SelectionClear(e) if e.selection == atoms.clipboard => owned = None,
                Event::SelectionRequest(e) => {
                    let property = serve(conn, atoms, owned.as_ref(), &e)?;
                    let notify = SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: e.time,
                        requestor: e.requestor,
                        selection: e.selection,
                        target: e.target,
                        property,
                    };
                    conn.send_event(false, e.requestor, EventMask::NO_EVENT, notify)?;
                    conn.flush()?;
                }
                Event::SelectionNotify(e) if e.selection == atoms.clipboard => {
                    let Some(current) = fetch.take() else {
                        continue;
                    };
                    if e.property == x11rb::NONE {
                        continue;
                    }
                    let reply = conn
                        .get_property(true, win, e.property, AtomEnum::ANY, 0, u32::MAX / 4)?
                        .reply()?;
                    if reply.type_ == atoms.incr || reply.bytes_after != 0 {
                        continue;
                    }
                    match current.want {
                        Want::Targets => {
                            let offered: Vec<Atom> =
                                reply.value32().map(|v| v.collect()).unwrap_or_default();
                            let (want, target) = if offered.contains(&atoms.gnome) {
                                (Want::Files, atoms.gnome)
                            } else if offered.contains(&atoms.uri_list) {
                                (Want::Files, atoms.uri_list)
                            } else if offered.contains(&atoms.utf8) {
                                (Want::Text, atoms.utf8)
                            } else {
                                continue;
                            };
                            conn.convert_selection(
                                win,
                                atoms.clipboard,
                                target,
                                atoms.prop,
                                Time::CURRENT_TIME,
                            )?;
                            fetch = Some(Fetch {
                                want,
                                started: Instant::now(),
                            });
                            conn.flush()?;
                        }
                        Want::Text => {
                            if reply.value.len() as u64 <= CLIPBOARD_TEXT_LIMIT
                                && let Ok(text) = String::from_utf8(reply.value)
                                && !text.is_empty()
                                && events.send(ClipEvent::Text(text)).is_err()
                            {
                                return Ok(());
                            }
                        }
                        Want::Files => {
                            let text = String::from_utf8_lossy(&reply.value);
                            let files = uri::parse_any(&text);
                            if !files.is_empty() && events.send(ClipEvent::Files(files)).is_err() {
                                return Ok(());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if fetch
            .as_ref()
            .is_some_and(|f| f.started.elapsed() > FETCH_TIMEOUT)
        {
            fetch = None;
        }
        if idle {
            thread::sleep(Duration::from_millis(8));
        }
    }
}

/// Answers one SelectionRequest; returns the property written, or NONE to refuse.
fn serve(
    conn: &RustConnection,
    atoms: &Atoms,
    owned: Option<&Owned>,
    request: &SelectionRequestEvent,
) -> Result<Atom, Box<dyn std::error::Error>> {
    let property = if request.property == x11rb::NONE {
        request.target
    } else {
        request.property
    };
    let Some(owned) = owned.filter(|_| request.selection == atoms.clipboard) else {
        return Ok(x11rb::NONE);
    };
    let t = request.target;
    if t == atoms.targets {
        let mut list = vec![atoms.targets, atoms.timestamp];
        match owned {
            Owned::Text(_) => list.extend([atoms.utf8, atoms.plain_utf8, atoms.string]),
            Owned::Files(_) => list.extend([atoms.gnome, atoms.uri_list]),
        }
        conn.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::ATOM,
            &list,
        )?;
    } else if t == atoms.timestamp {
        conn.change_property32(
            PropMode::REPLACE,
            request.requestor,
            property,
            AtomEnum::INTEGER,
            &[request.time],
        )?;
    } else {
        let (data, kind): (Vec<u8>, Atom) = match owned {
            Owned::Text(text) if t == atoms.utf8 || t == atoms.plain_utf8 => {
                (text.clone().into_bytes(), t)
            }
            Owned::Text(text) if t == atoms.string => (text.clone().into_bytes(), atoms.string),
            Owned::Files(paths) if t == atoms.uri_list => {
                (uri::uri_list(paths).into_bytes(), atoms.uri_list)
            }
            Owned::Files(paths) if t == atoms.gnome => {
                (uri::gnome_copied(paths).into_bytes(), atoms.gnome)
            }
            _ => return Ok(x11rb::NONE),
        };
        conn.change_property8(PropMode::REPLACE, request.requestor, property, kind, &data)?;
    }
    Ok(property)
}
