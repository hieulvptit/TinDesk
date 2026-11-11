//! Per-user clipboard/file sync helper. Runs inside the logged-in graphical session,
//! so staged files and clipboard access belong to that user, never to root.
pub mod bridge;
pub mod uri;
pub mod x11clip;
