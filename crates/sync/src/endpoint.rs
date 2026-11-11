use crate::{Busy, Clipboard, ClipboardSink, Notice, Outbound};
use binachicken_protocol::*;
use std::collections::VecDeque;
use std::path::PathBuf;

/// One side of the sync channel: receives into a clipboard and sends batches out.
pub struct Endpoint<C: Clipboard> {
    receiver: SyncReceiver<ClipboardSink<C>>,
    pub outbound: Outbound,
    replies: VecDeque<Vec<u8>>,
}
impl<C: Clipboard> Endpoint<C> {
    pub fn new(root: PathBuf, clipboard: C, policy: Policy) -> Self {
        Self {
            receiver: SyncReceiver::new(ClipboardSink::new(root, clipboard), policy),
            outbound: Outbound::new(),
            replies: VecDeque::new(),
        }
    }
    pub fn policy(&self) -> Policy {
        self.receiver.policy
    }
    pub fn set_policy(&mut self, policy: Policy) {
        self.receiver.policy = policy;
    }
    pub fn clipboard(&mut self) -> &mut C {
        &mut self.receiver.sink_mut().clipboard
    }
    /// Abort everything in flight; call on every fresh PeerConnection epoch.
    pub fn reset(&mut self) {
        self.receiver.reset();
        self.outbound.reset();
        self.replies.clear();
    }
    pub fn take_notices(&mut self) -> Vec<Notice> {
        let mut notices = self.receiver.sink_mut().take_notices();
        notices.extend(self.outbound.take_notices());
        notices
    }
    /// Route one received frame. Errors mean a protocol violation by the peer.
    pub fn on_frame(&mut self, bytes: &[u8]) -> Result<(), SyncError> {
        if let SyncFrame::Control(
            message @ (SyncMessage::Progress { .. }
            | SyncMessage::Done { .. }
            | SyncMessage::Reject { .. }),
        ) = decode_frame(bytes)?
        {
            self.outbound.handle(&message);
            return Ok(());
        }
        for reply in self.receiver.handle(bytes)? {
            self.replies.push_back(encode_sync_control(&reply)?);
        }
        Ok(())
    }
    /// Flush replies first, then outgoing transfers.
    pub fn poll(&mut self, send: &mut dyn FnMut(&[u8]) -> Result<(), Busy>) {
        while let Some(frame) = self.replies.front() {
            if send(frame).is_err() {
                return;
            }
            self.replies.pop_front();
        }
        self.outbound.poll(send);
    }
}
