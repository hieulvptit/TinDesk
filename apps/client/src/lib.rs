pub mod app;
pub mod clipboard;
mod connection;
pub mod input;
pub mod video;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub mod autostart;
#[cfg(target_os = "macos")]
pub mod media_runtime;
