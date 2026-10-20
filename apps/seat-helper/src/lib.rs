pub mod capture;
pub mod input;
pub mod ipc;
pub mod recovery;
pub mod seat;
pub mod x11;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub(crate) fn error(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other(message).into()
}
