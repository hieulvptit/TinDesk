//! Length-prefixed sync frames over a byte stream (agent <-> user helper socket).
use binachicken_protocol::SYNC_FRAME_LIMIT;
use std::io::{self, Write};

pub fn write_frame(writer: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    if frame.len() > SYNC_FRAME_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame too large",
        ));
    }
    let mut out = Vec::with_capacity(4 + frame.len());
    out.extend_from_slice(&(frame.len() as u32).to_be_bytes());
    out.extend_from_slice(frame);
    writer.write_all(&out)
}

/// Incremental reader tolerant of short reads and read timeouts.
#[derive(Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}
impl FrameReader {
    /// The announced length is checked before anything is buffered for it.
    pub fn push(&mut self, bytes: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buf.len() < 4 {
                break;
            }
            let len = u32::from_be_bytes(self.buf[..4].try_into().unwrap()) as usize;
            if len > SYNC_FRAME_LIMIT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "frame too large",
                ));
            }
            if self.buf.len() < 4 + len {
                break;
            }
            frames.push(self.buf[4..4 + len].to_vec());
            self.buf.drain(..4 + len);
        }
        Ok(frames)
    }
}
