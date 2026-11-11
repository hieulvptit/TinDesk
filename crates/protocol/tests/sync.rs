use binachicken_protocol::*;
use std::collections::HashMap;

#[derive(Default)]
struct Mem {
    open: HashMap<u32, Vec<u8>>,
    done: Vec<(u32, String, Vec<u8>)>,
    names: HashMap<u32, String>,
    aborted: Vec<u32>,
}
impl Sink for Mem {
    fn begin(&mut self, id: u32, _: SyncKind, name: &str, _: u64) -> std::io::Result<()> {
        self.open.insert(id, vec![]);
        self.names.insert(id, name.into());
        Ok(())
    }
    fn write(&mut self, id: u32, d: &[u8]) -> std::io::Result<()> {
        self.open.get_mut(&id).unwrap().extend_from_slice(d);
        Ok(())
    }
    fn commit(&mut self, id: u32) -> std::io::Result<()> {
        let data = self.open.remove(&id).unwrap();
        self.done.push((id, self.names[&id].clone(), data));
        Ok(())
    }
    fn abort(&mut self, id: u32) {
        self.open.remove(&id);
        self.aborted.push(id);
    }
}
const ON: Policy = Policy {
    clipboard: true,
    files: true,
};

fn send(rx: &mut SyncReceiver<Mem>, kind: SyncKind, name: &str, data: &[u8]) -> Vec<SyncMessage> {
    let (mut tx, offer) = SyncSender::new(1, kind, name, data.len() as u64).unwrap();
    let mut replies = rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    for part in data.chunks(SYNC_CHUNK_SIZE) {
        let r = rx.handle(&tx.chunk(part).unwrap()).unwrap();
        for m in &r {
            if let SyncMessage::Progress { received, .. } = m {
                tx.on_progress(*received);
            }
        }
        replies.extend(
            r.into_iter()
                .filter(|m| !matches!(m, SyncMessage::Progress { .. })),
        );
    }
    replies.extend(rx.handle(&tx.finish().unwrap()).unwrap());
    replies
}

#[test]
fn multi_chunk_file_round_trips_and_is_acknowledged() {
    let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    let replies = send(&mut rx, SyncKind::File, "a.bin", &data);
    assert!(matches!(replies.as_slice(), [SyncMessage::Done { id: 1 }]));
    assert_eq!(rx.sink().done, vec![(1, "a.bin".into(), data)]);
}

#[test]
fn text_and_empty_file_work() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    send(&mut rx, SyncKind::Text, "ignored", "xin chào".as_bytes());
    assert_eq!(rx.sink().done[0].2, "xin chào".as_bytes());
    assert_eq!(rx.sink().done[0].1, "");
    send(&mut rx, SyncKind::File, "empty", b"");
    assert_eq!(rx.sink().done.len(), 2);
}

#[test]
fn disabled_policy_rejects_before_storing() {
    let mut rx = SyncReceiver::new(
        Mem::default(),
        Policy {
            clipboard: false,
            files: true,
        },
    );
    let (_, offer) = SyncSender::new(1, SyncKind::Text, "", 3).unwrap();
    let r = rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    assert!(matches!(
        r[..],
        [SyncMessage::Reject {
            reason: RejectReason::Disabled,
            ..
        }]
    ));
    assert!(rx.sink().open.is_empty());
}

#[test]
fn hostile_names_are_refused() {
    for name in ["../x", "a/b", "a\\b", "", "..", ".", "a\0b", "a\nb"] {
        assert!(
            SyncSender::new(1, SyncKind::File, name, 1).is_err(),
            "{name:?}"
        );
        let offer = SyncMessage::Offer {
            id: 1,
            kind: SyncKind::File,
            name: name.into(),
            size: 1,
        };
        let mut rx = SyncReceiver::new(Mem::default(), ON);
        let r = rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
        assert!(
            matches!(
                r[..],
                [SyncMessage::Reject {
                    reason: RejectReason::InvalidName,
                    ..
                }]
            ),
            "{name:?}"
        );
        assert!(rx.sink().open.is_empty());
    }
}

#[test]
fn oversize_offers_are_refused() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    for (kind, size) in [
        (SyncKind::File, FILE_LIMIT + 1),
        (SyncKind::Text, CLIPBOARD_TEXT_LIMIT + 1),
    ] {
        let offer = SyncMessage::Offer {
            id: 1,
            kind,
            name: "f".into(),
            size,
        };
        let r = rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
        assert!(matches!(
            r[..],
            [SyncMessage::Reject {
                reason: RejectReason::TooLarge,
                ..
            }]
        ));
    }
}

#[test]
fn corrupted_or_truncated_transfers_never_commit() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    let (mut tx, offer) = SyncSender::new(1, SyncKind::File, "f", 4).unwrap();
    rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    rx.handle(&tx.chunk(b"abcd").unwrap()).unwrap();
    // Flip a byte in flight by sending a different hash.
    let bad = encode_sync_control(&SyncMessage::Finish {
        id: 1,
        sha256: "00".into(),
    })
    .unwrap();
    let r = rx.handle(&bad).unwrap();
    assert!(matches!(
        r[..],
        [SyncMessage::Reject {
            reason: RejectReason::IntegrityFailed,
            ..
        }]
    ));
    assert!(rx.sink().done.is_empty());
    // Truncated: finish before all bytes arrived.
    let (mut tx, offer) = SyncSender::new(2, SyncKind::File, "f", 8).unwrap();
    rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    rx.handle(&tx.chunk(b"abcd").unwrap()).unwrap();
    assert!(tx.finish().is_err());
    let early = encode_sync_control(&SyncMessage::Finish {
        id: 2,
        sha256: "x".into(),
    })
    .unwrap();
    assert!(matches!(
        rx.handle(&early).unwrap()[..],
        [SyncMessage::Reject { .. }]
    ));
    assert!(rx.sink().done.is_empty());
}

#[test]
fn out_of_order_overflow_and_unknown_chunks_are_rejected() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    let (_, offer) = SyncSender::new(1, SyncKind::File, "f", 4).unwrap();
    rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    let skip = encode_chunk(1, 2, b"xx").unwrap();
    assert!(matches!(
        rx.handle(&skip).unwrap()[..],
        [SyncMessage::Reject { .. }]
    ));
    assert!(rx.sink().open.is_empty());
    assert_eq!(rx.handle(&encode_chunk(9, 0, b"x").unwrap()), Ok(vec![]));
    let (_, offer) = SyncSender::new(3, SyncKind::File, "f", 2).unwrap();
    rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    let over = encode_chunk(3, 0, b"xyz").unwrap();
    assert!(matches!(
        rx.handle(&over).unwrap()[..],
        [SyncMessage::Reject { .. }]
    ));
}

#[test]
fn concurrency_cap_and_reset() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    for id in 0..MAX_ACTIVE_TRANSFERS as u32 {
        let o = SyncMessage::Offer {
            id,
            kind: SyncKind::File,
            name: "f".into(),
            size: 1,
        };
        assert!(
            rx.handle(&encode_sync_control(&o).unwrap())
                .unwrap()
                .is_empty()
        );
    }
    let o = SyncMessage::Offer {
        id: 99,
        kind: SyncKind::File,
        name: "f".into(),
        size: 1,
    };
    assert!(matches!(
        rx.handle(&encode_sync_control(&o).unwrap()).unwrap()[..],
        [SyncMessage::Reject {
            reason: RejectReason::TooMany,
            ..
        }]
    ));
    rx.reset();
    assert!(rx.sink().open.is_empty());
    assert_eq!(rx.sink().aborted.len(), MAX_ACTIVE_TRANSFERS);
}

#[test]
fn malformed_and_oversize_frames_are_errors() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    assert_eq!(rx.handle(&[]), Err(SyncError::Malformed));
    assert_eq!(rx.handle(&[7, 1, 2]), Err(SyncError::Malformed));
    assert_eq!(rx.handle(&[1, 0, 0]), Err(SyncError::Malformed));
    assert_eq!(
        rx.handle(&vec![0; SYNC_FRAME_LIMIT + 1]),
        Err(SyncError::FrameTooLarge)
    );
    assert!(encode_chunk(1, 0, &vec![0; SYNC_FRAME_LIMIT]).is_err());
}

#[test]
fn window_blocks_sender_until_receiver_reports_progress() {
    let mut rx = SyncReceiver::new(Mem::default(), ON);
    let size = SYNC_WINDOW * 3;
    let (mut tx, offer) = SyncSender::new(1, SyncKind::File, "big", size).unwrap();
    rx.handle(&encode_sync_control(&offer).unwrap()).unwrap();
    let block = vec![7u8; SYNC_CHUNK_SIZE];
    let mut frames = vec![];
    while tx.can_send() {
        frames.push(tx.chunk(&block).unwrap());
    }
    assert_eq!(frames.len() as u64 * SYNC_CHUNK_SIZE as u64, SYNC_WINDOW);
    assert_eq!(tx.chunk(&block), Err(SyncError::WindowFull));
    let mut last = None;
    for f in &frames {
        for m in rx.handle(f).unwrap() {
            last = Some(m);
        }
    }
    let Some(SyncMessage::Progress { received, .. }) = last else {
        panic!("no progress")
    };
    tx.on_progress(received);
    assert!(tx.can_send());
    // A lying receiver cannot acknowledge bytes that were never sent.
    tx.on_progress(u64::MAX);
    assert_eq!(tx.chunk(&block).map(|_| ()), Ok(()));
}
