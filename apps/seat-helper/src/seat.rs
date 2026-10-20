//! Discover the active local Xorg server; never trust DISPLAY from the service environment.
#[cfg(target_os = "linux")]
use crate::x11;
use crate::{Result, error};
use std::{collections::BTreeMap, path::PathBuf};
#[cfg(target_os = "linux")]
use std::{fs, os::unix::fs::MetadataExt, process::Command};

#[derive(Default)]
pub struct SessionProperties(BTreeMap<String, String>);
impl SessionProperties {
    pub fn parse(text: &str) -> Self {
        Self(
            text.lines()
                .filter_map(|l| l.split_once('=').map(|(k, v)| (k.into(), v.into())))
                .collect(),
        )
    }
    fn get(&self, key: &str) -> &str {
        self.0.get(key).map(String::as_str).unwrap_or("")
    }
}
pub fn display_number(display: &str) -> Option<u16> {
    let number = display.strip_prefix(':')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}
pub fn physical_session(s: &SessionProperties) -> bool {
    s.get("Seat") == "seat0"
        && s.get("Type") == "x11"
        && s.get("Remote") == "no"
        && s.get("Active") == "yes"
        && s.get("VTNr").parse::<u32>().is_ok_and(|v| v > 0)
        && display_number(s.get("Display")).is_some()
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerIdentity {
    pub process_start: String,
    pub socket_inode: u64,
    pub authority_inode: u64,
    pub authority_modified_ns: i128,
    /// A persistent probe is lost on reset, even if the PID/socket have not changed.
    pub connection_epoch: u64,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayTarget {
    pub seat: String,
    pub display: String,
    pub authority: PathBuf,
    pub identity: ServerIdentity,
    pub width: u32,
    pub height: u32,
}
impl DisplayTarget {
    pub fn socket_path(&self) -> Result<PathBuf> {
        let number = display_number(&self.display).ok_or_else(|| error("invalid local display"))?;
        Ok(PathBuf::from(format!("/tmp/.X11-unix/X{number}")))
    }
}
#[derive(Default)]
pub struct SeatMonitor {
    #[cfg(target_os = "linux")]
    probe: x11::ServerProbe,
}
#[cfg(target_os = "linux")]
fn loginctl(args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/bin/loginctl").args(args).output()?;
    if !out.status.success() {
        return Err(error("logind query failed"));
    }
    Ok(String::from_utf8(out.stdout)?)
}
impl SeatMonitor {
    pub fn current(&mut self) -> Result<Option<DisplayTarget>> {
        #[cfg(not(target_os = "linux"))]
        {
            Err(error("physical seat discovery requires Linux/logind"))
        }
        #[cfg(target_os = "linux")]
        self.current_linux()
    }
    #[cfg(target_os = "linux")]
    fn current_linux(&mut self) -> Result<Option<DisplayTarget>> {
        let session = loginctl(&["show-seat", "seat0", "-p", "ActiveSession", "--value"])?;
        let session = session.trim();
        if session.is_empty() {
            self.probe.invalidate();
            return Ok(None);
        }
        // Session IDs come from logind and are separate arguments, never shell fragments.
        let props = SessionProperties::parse(&loginctl(&["show-session", session])?);
        if !physical_session(&props) {
            self.probe.invalidate();
            return Ok(None);
        }
        let mut found = None;
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
            {
                continue;
            }
            let dir = entry.path();
            let Ok(cmd) = fs::read(dir.join("cmdline")) else {
                continue;
            };
            let args: Vec<_> = cmd
                .split(|b| *b == 0)
                .filter_map(|v| std::str::from_utf8(v).ok())
                .collect();
            let Some(_) = args.first() else {
                continue;
            };
            // argv[0] alone is forgeable by an unrelated local process.
            let Ok(executable) = fs::read_link(dir.join("exe")) else {
                continue;
            };
            let Ok(executable_metadata) = fs::metadata(&executable) else {
                continue;
            };
            if executable_metadata.uid() != 0 || executable_metadata.mode() & 0o022 != 0 {
                continue;
            }
            let base = executable.file_name().unwrap_or_default();
            if base != "Xorg" && base != "X" {
                continue;
            }
            if !args.contains(&props.get("Display"))
                || !args.contains(&format!("vt{}", props.get("VTNr")).as_str())
            {
                continue;
            }
            if !args.windows(2).any(|v| v == ["-seat", "seat0"]) {
                continue;
            }
            let Some(auth) = args
                .windows(2)
                .find(|v| v[0] == "-auth")
                .map(|v| PathBuf::from(v[1]))
            else {
                continue;
            };
            if !auth.is_absolute() {
                continue;
            }
            let stat = fs::read_to_string(dir.join("stat"))?;
            let start = stat
                .rsplit_once(')')
                .ok_or_else(|| error("invalid process identity"))?
                .1
                .split_whitespace()
                .nth(19)
                .ok_or_else(|| error("missing process start time"))?;
            let mut target = DisplayTarget {
                seat: "seat0".into(),
                display: props.get("Display").into(),
                authority: auth,
                identity: ServerIdentity {
                    process_start: format!(
                        "{}:{}:{start}",
                        fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
                        entry.file_name().to_string_lossy()
                    ),
                    socket_inode: 0,
                    authority_inode: 0,
                    authority_modified_ns: 0,
                    connection_epoch: 0,
                },
                width: 0,
                height: 0,
            };
            let socket = fs::metadata(target.socket_path()?)?;
            let authority = fs::metadata(&target.authority)?;
            target.identity.socket_inode = socket.ino();
            target.identity.authority_inode = authority.ino();
            target.identity.authority_modified_ns =
                i128::from(authority.mtime()) * 1_000_000_000 + i128::from(authority.mtime_nsec());
            if found.is_some() {
                return Err(error("ambiguous physical Xorg server"));
            }
            found = Some(target);
        }
        let Some(mut target) = found else {
            self.probe.invalidate();
            return Ok(None);
        };
        let (epoch, width, height) = self.probe.refresh(&target)?;
        target.identity.connection_epoch = epoch;
        target.width = width;
        target.height = height;
        Ok(Some(target))
    }
}
