use binachicken_protocol::*;

fn round(frame: TunnelFrame<'_>) {
    let bytes = encode_tunnel(&frame).unwrap();
    assert!(is_tunnel_frame(&bytes));
    assert_eq!(decode_tunnel(&bytes).unwrap(), frame);
}

#[test]
fn every_frame_round_trips() {
    round(TunnelFrame::Hello);
    round(TunnelFrame::Info {
        enabled: true,
        user: "hieulv".into(),
    });
    round(TunnelFrame::Info {
        enabled: false,
        user: String::new(),
    });
    round(TunnelFrame::Open { conn: 7 });
    round(TunnelFrame::Accepted { conn: 7 });
    round(TunnelFrame::Refused {
        conn: 7,
        reason: RefuseReason::Locked,
    });
    round(TunnelFrame::Data {
        conn: 9,
        data: &[0, 1, 2, 255],
    });
    round(TunnelFrame::Data { conn: 9, data: &[] });
    round(TunnelFrame::Credit {
        conn: 9,
        bytes: 65536,
    });
    round(TunnelFrame::Close { conn: u32::MAX });
}

#[test]
fn account_names_cannot_smuggle_shell_syntax() {
    for ok in ["hieulv", "a", "user.name", "user_1", "a-b"] {
        assert!(valid_user(ok), "{ok}");
    }
    for bad in [
        "",
        "-oProxyCommand=x",
        ".hidden",
        "a b",
        "a;b",
        "a$(x)",
        "a\nb",
        "a`b`",
        "a/b",
        "a'b",
        "ünï",
        &"x".repeat(65),
    ] {
        assert!(!valid_user(bad), "{bad:?}");
        assert!(
            encode_tunnel(&TunnelFrame::Info {
                enabled: true,
                user: bad.into()
            })
            .is_err()
                || bad.is_empty()
        );
    }
    // A hostile peer's Info is rejected on decode too.
    let mut raw = encode_tunnel(&TunnelFrame::Info {
        enabled: true,
        user: "ok".into(),
    })
    .unwrap();
    raw.truncate(7);
    raw.extend_from_slice(b"a;rm -rf");
    assert_eq!(decode_tunnel(&raw), Err(TunnelError::InvalidUser));
}

#[test]
fn malformed_and_oversize_frames_are_errors() {
    assert!(decode_tunnel(&[]).is_err());
    assert!(decode_tunnel(&[2, 6]).is_err());
    assert!(decode_tunnel(&[0, 6, 0, 0, 0, 0]).is_err());
    assert!(decode_tunnel(&[2, 99, 0, 0, 0, 1]).is_err());
    assert!(
        decode_tunnel(&[2, 3, 0, 0, 0, 1, 9]).is_err(),
        "Open carries no body"
    );
    assert!(
        decode_tunnel(&[2, 7, 0, 0, 0, 1, 0, 0]).is_err(),
        "short credit"
    );
    let big = vec![0u8; TUNNEL_CHUNK + 1];
    assert!(
        encode_tunnel(&TunnelFrame::Data {
            conn: 1,
            data: &big
        })
        .is_err()
    );
    let mut raw = vec![2, 6, 0, 0, 0, 1];
    raw.extend(vec![0u8; TUNNEL_CHUNK + 1]);
    assert!(decode_tunnel(&raw).is_err());
}

#[test]
fn windows_bound_both_directions_and_reject_inflated_credit() {
    let mut send = SendWindow::new();
    assert_eq!(send.avail(), TUNNEL_WINDOW);
    send.consume(TUNNEL_CHUNK);
    assert_eq!(send.avail(), TUNNEL_WINDOW - TUNNEL_CHUNK as u64);
    send.grant(TUNNEL_CHUNK as u32).unwrap();
    assert_eq!(
        send.grant(1),
        Err(TunnelError::WindowExceeded),
        "cannot exceed what was sent"
    );

    let mut recv = RecvFlow::default();
    recv.on_data(TUNNEL_WINDOW as usize).unwrap();
    assert_eq!(
        recv.on_data(1),
        Err(TunnelError::WindowExceeded),
        "peer ignored the window"
    );
    assert_eq!(recv.consumed(1000), None, "credit is batched");
    let granted = recv.consumed(CREDIT_STEP as usize).unwrap();
    assert_eq!(granted as u64, 1000 + CREDIT_STEP);
    recv.on_data(10).unwrap();
}
