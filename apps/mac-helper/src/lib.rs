//! macOS host helper: speaks the same local IPC as the Linux seat helper, so the
//! unmodified agent can expose a Mac's main display to an enrolled client.
#[cfg(target_os = "macos")]
pub mod capture;
#[cfg(target_os = "macos")]
pub mod display;
#[cfg(target_os = "macos")]
pub mod input;
pub mod keymap;
#[cfg(target_os = "macos")]
pub mod serve;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub(crate) fn error(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message).into()
}

/// Monotonic milliseconds shared by the agent and this helper. Both run on the
/// same machine, so the system-wide monotonic clock is a valid lease timebase.
pub fn clock_ms() -> u64 {
    let mut value = libc_timespec();
    unsafe extern "C" {
        fn clock_gettime(clock: i32, value: *mut Timespec) -> i32;
    }
    // CLOCK_MONOTONIC is 6 on macOS and 1 on Linux.
    #[cfg(target_os = "macos")]
    const CLOCK: i32 = 6;
    #[cfg(not(target_os = "macos"))]
    const CLOCK: i32 = 1;
    if unsafe { clock_gettime(CLOCK, &mut value) } != 0 {
        return u64::MAX;
    }
    value.sec as u64 * 1000 + value.nsec as u64 / 1_000_000
}
#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}
fn libc_timespec() -> Timespec {
    Timespec { sec: 0, nsec: 0 }
}
