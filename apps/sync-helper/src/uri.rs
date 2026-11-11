//! `file://` URI lists as used by text/uri-list and x-special/gnome-copied-files.
use std::path::{Path, PathBuf};

fn unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/')
}

pub fn encode_path(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if unreserved(b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Absolute local paths only. Remote hosts, relative paths, NUL bytes and
/// anything that is not `file:` are dropped rather than guessed at.
pub fn parse_list(text: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    text.lines()
        .map(str::trim)
        .filter_map(|line| {
            let rest = line.strip_prefix("file://")?;
            let rest = rest.strip_prefix("localhost").unwrap_or(rest);
            if !rest.starts_with('/') {
                return None;
            }
            let bytes = decode(rest)?;
            if bytes.contains(&0) {
                return None;
            }
            Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
        })
        .collect()
}

/// `text/uri-list` body.
pub fn uri_list(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| encode_path(p) + "\r\n").collect()
}

/// `x-special/gnome-copied-files` body: operation line, then URIs, no trailing newline.
pub fn gnome_copied(paths: &[PathBuf]) -> String {
    let mut out = String::from("copy");
    for p in paths {
        out.push('\n');
        out.push_str(&encode_path(p));
    }
    out
}

/// Accepts either format; the gnome variant carries a leading copy/cut line.
pub fn parse_any(text: &str) -> Vec<PathBuf> {
    parse_list(text)
}
