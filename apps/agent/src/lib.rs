pub mod service;
pub mod sync_bridge;
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub mod config;
pub mod helper;
