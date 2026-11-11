use crate::Result;
use std::path::{Path, PathBuf};
const LABEL: &str = "io.binachicken.client";
fn path() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("HOME").ok_or("HOME is unavailable")?)
            .join("Library/LaunchAgents/io.binachicken.client.plist"),
    )
}
fn read_enabled(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(true),
        Ok(_) => Err("Startup setting is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}
pub fn enabled() -> Result<bool> {
    read_enabled(&path()?)
}
/// No startup changes occur until the user explicitly toggles this setting.
/// Preserve CLI options and the backend environment override for the next login.
pub fn set_autostart(enabled: bool) -> Result<()> {
    let mut args = vec![std::env::current_exe()?.into_os_string()];
    args.extend(std::env::args_os().skip(1));
    let args = args
        .into_iter()
        .map(|s| {
            s.into_string()
                .map_err(|_| "Application arguments are not UTF-8".into())
        })
        .collect::<Result<Vec<_>>>()?;
    write_agent(
        &path()?,
        enabled,
        &args,
        std::env::var("BINACHICKEN_BACKEND").ok().as_deref(),
    )
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn write_agent(path: &Path, enabled: bool, args: &[String], backend: Option<&str>) -> Result<()> {
    read_enabled(path)?;
    if enabled {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::create_dir_all(path.parent().ok_or("invalid startup path")?)?;
        let args = args
            .iter()
            .map(|a| format!("<string>{}</string>", xml(a)))
            .collect::<String>();
        let environment=backend.map(|b|format!("<key>EnvironmentVariables</key><dict><key>BINACHICKEN_BACKEND</key><string>{}</string></dict>",xml(b))).unwrap_or_default();
        let contents = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{LABEL}</string><key>ProgramArguments</key><array>{args}</array>{environment}<key>RunAtLoad</key><true/></dict></plist>\n"
        );
        let temp = path.with_extension(format!("{:016x}.tmp", rand::random::<u64>()));
        let result = (|| -> Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temp)?;
            file.write_all(contents.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp, path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(temp);
        }
        result
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_setting_is_read_only_until_explicit_toggle_and_xml_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.plist");
        std::fs::write(&path, b"existing user setting").unwrap();
        assert!(read_enabled(&path).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing user setting");
        write_agent(
            &path,
            true,
            &[
                "/Applications/A&B/BinaChicken".into(),
                "--backend".into(),
                "https://example.test".into(),
            ],
            None,
        )
        .unwrap();
        #[cfg(target_os = "macos")]
        {
            let result = std::process::Command::new("/usr/bin/plutil")
                .args(["-extract", "ProgramArguments.0", "raw", "-o", "-"])
                .arg(&path)
                .output()
                .unwrap();
            assert!(result.status.success());
            assert_eq!(
                String::from_utf8(result.stdout).unwrap().trim(),
                "/Applications/A&B/BinaChicken"
            );
        }
        write_agent(&path, false, &[], None).unwrap();
        assert!(!read_enabled(&path).unwrap());
    }
}
