//! Clipboard/file sync endpoint shared by the macOS client and the Ubuntu sync helper.
//! No OS clipboard or network code: adapters implement `Clipboard` and move frames.
mod endpoint;
mod frames;
mod outbound;
mod store;
mod tunnel;

pub use endpoint::Endpoint;
pub use frames::{FrameReader, write_frame};
pub use outbound::{Busy, Outbound, QueuedFiles};
pub use store::{Clipboard, ClipboardSink, purge_older_than};
pub use tunnel::{Mux, TunnelInfo};

/// User-visible outcomes; never include clipboard content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    ReceivedText,
    ReceivedFiles(usize),
    SentText,
    SentFiles(usize),
    /// The peer refused a transfer (disabled, too large, ...).
    Refused(binachicken_protocol::RejectReason),
    Failed(&'static str),
}
