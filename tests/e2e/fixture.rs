//! Inert unless explicitly launched by the process acceptance harness.
#[cfg(target_os = "linux")]
#[path = "fixture_linux.rs"]
mod linux;
fn main() {
    #[cfg(target_os = "linux")]
    if let Err(error) = linux::run() {
        eprintln!("e2e fixture: {error}");
        std::process::exit(1);
    }
}
