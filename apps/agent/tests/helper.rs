use remotevnc_agent::helper::Helper;
use remotevnc_protocol::*;
use std::{io::Read, os::unix::net::UnixListener, time::Duration};

#[tokio::test]
async fn helper_eof_and_stop_are_bounded_even_with_partial_large_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (header_tx, header_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        use std::io::Write;
        peer.write_all(&(0x80000000u32 | (ENCODED_FRAME_LIMIT as u32 + 25)).to_be_bytes())
            .unwrap();
        header_tx.send(()).unwrap();
        let mut byte = [0];
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(peer.read(&mut byte).unwrap(), 0);
    });
    let helper = Helper::connect(&path).unwrap();
    header_rx.recv().unwrap();
    helper.disconnect();
    worker.join().unwrap();
}
#[tokio::test]
async fn fake_helper_disconnect_is_reported_without_a_user_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        ready_rx.recv().unwrap();
        write_helper_output(&mut peer, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
    });
    let mut helper = Helper::connect(&path).unwrap();
    ready_tx.send(()).unwrap();
    assert!(matches!(
        helper.events.recv().await,
        Some(HelperEvent::Ready)
    ));
    tokio::time::timeout(Duration::from_secs(2), helper.failed.cancelled())
        .await
        .unwrap();
    worker.join().unwrap();
}

#[tokio::test]
async fn peer_close_during_connection_setup_fails_closed() {
    for _ in 0..100 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("helper.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let child = std::thread::spawn(move || {
            drop(listener.accept().unwrap());
        });
        match Helper::connect(&path) {
            Ok(helper) => tokio::time::timeout(Duration::from_secs(2), helper.failed.cancelled())
                .await
                .unwrap(),
            Err(error) => {
                // Darwin may reject SO_SNDTIMEO on a socket whose peer has already closed.
                assert!(
                    error
                        .to_string()
                        .starts_with("helper write timeout setup failed:"),
                    "{error}"
                );
                eprintln!("expected setup EOF: {error}");
            }
        }
        child.join().unwrap();
    }
}

#[tokio::test]
async fn encoded_queue_pressure_preserves_control_and_recovers_only_at_config_idr() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (phase_tx, phase_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        phase_rx.recv().unwrap();
        let send = |stream: &mut std::os::unix::net::UnixStream, pts_ns, keyframe, data| {
            write_helper_output(
                stream,
                &HelperOutput::Video(EncodedFrame {
                    generation: 1,
                    pts_ns,
                    keyframe,
                    data,
                }),
            )
            .unwrap();
        };
        for pts in 0..8 {
            send(&mut stream, pts, false, vec![0, 0, 1, 0x41, 1]);
        }
        write_helper_output(&mut stream, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
        phase_rx.recv().unwrap();
        send(&mut stream, 8, false, vec![0, 0, 1, 0x41, 1]);
        send(&mut stream, 9, true, vec![0, 0, 1, 0x65, 1]); // IDR without config cannot recover.
        send(
            &mut stream,
            10,
            true,
            vec![0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 1, 0, 0, 1, 0x65, 1],
        );
        send(&mut stream, 11, false, vec![0, 0, 1, 0x41, 1]);
        write_helper_output(&mut stream, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
        phase_rx.recv().unwrap();
    });
    let mut helper = Helper::connect(&path).unwrap();
    helper
        .send(HelperRequest::StartCapture {
            session_id: SessionId::from_uuid(uuid::Uuid::new_v4()),
            generation: 1,
            profile: VideoProfile::default(),
        })
        .unwrap();
    phase_tx.send(()).unwrap();
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(2), helper.events.recv())
                .await
                .unwrap(),
            Some(HelperEvent::Ready)
        ),
        "encoded pressure must retain the control stream"
    );
    assert!(!helper.failed.is_cancelled());
    let started = std::time::Instant::now();
    let (drops, pending) = helper.video_health(started).unwrap();
    assert_eq!(drops, 6);
    assert!(pending);
    assert!(
        !helper.video_health(started).unwrap().1,
        "one notification per attempt"
    );
    let first = helper.video.try_recv().unwrap();
    let second = helper.video.try_recv().unwrap();
    assert_eq!(
        (first.frame.pts_ns, second.frame.pts_ns),
        (0, 1),
        "safe queued references stay FIFO"
    );
    phase_tx.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), helper.events.recv())
            .await
            .unwrap(),
        Some(HelperEvent::Ready)
    ));
    let idr = helper.video.try_recv().unwrap();
    let delta = helper.video.try_recv().unwrap();
    assert_eq!((idr.frame.pts_ns, delta.frame.pts_ns), (10, 11));
    assert!(idr.frame.is_config_keyframe());
    helper
        .send(HelperRequest::UpdateProfile {
            session_id: SessionId::from_uuid(uuid::Uuid::new_v4()),
            generation: 1,
            profile: VideoProfile::default().downshift(),
        })
        .unwrap();
    helper.video_accepted(1, first.recovery_revision);
    assert!(
        helper
            .video_health(started + Duration::from_secs(21))
            .is_err(),
        "old queued frames cannot clear recovery"
    );
    helper.video_accepted(2, idr.recovery_revision);
    assert!(
        helper
            .video_health(started + Duration::from_secs(21))
            .is_err(),
        "wrong generation cannot clear recovery"
    );
    helper.video_accepted(1, idr.recovery_revision); // Service calls only after successful Peer push.
    assert!(
        helper
            .video_health(started + Duration::from_secs(21))
            .is_ok()
    );
    assert!(!helper.failed.is_cancelled());
    phase_tx.send(()).unwrap();
    worker.join().unwrap();
}
#[tokio::test]
async fn helper_control_saturation_still_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        for _ in 0..33 {
            let _ = write_helper_output(&mut stream, &HelperOutput::Event(HelperEvent::Ready));
        }
        done_rx.recv().unwrap();
    });
    let helper = Helper::connect(&path).unwrap();
    tokio::time::timeout(Duration::from_secs(2), helper.failed.cancelled())
        .await
        .unwrap();
    done_tx.send(()).unwrap();
    worker.join().unwrap();
}

#[tokio::test]
async fn malformed_metadata_during_video_recovery_still_fails_closed() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("helper.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let (phase_tx, phase_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        phase_rx.recv().unwrap();
        for pts_ns in 0..8 {
            write_helper_output(
                &mut stream,
                &HelperOutput::Video(EncodedFrame {
                    generation: 1,
                    pts_ns,
                    keyframe: false,
                    data: vec![0, 0, 1, 0x41, 1],
                }),
            )
            .unwrap();
        }
        write_helper_output(&mut stream, &HelperOutput::Event(HelperEvent::Ready)).unwrap();
        phase_rx.recv().unwrap();
        stream.write_all(b"RVF1").unwrap();
        stream
            .write_all(&((ENCODED_FRAME_LIMIT + 1) as u32).to_be_bytes())
            .unwrap();
        phase_rx.recv().unwrap();
    });
    let mut helper = Helper::connect(&path).unwrap();
    helper
        .send(HelperRequest::StartCapture {
            session_id: SessionId::from_uuid(uuid::Uuid::new_v4()),
            generation: 1,
            profile: VideoProfile::default(),
        })
        .unwrap();
    phase_tx.send(()).unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(2), helper.events.recv())
            .await
            .unwrap(),
        Some(HelperEvent::Ready)
    ));
    assert!(!helper.failed.is_cancelled());
    phase_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), helper.failed.cancelled())
        .await
        .unwrap();
    phase_tx.send(()).unwrap();
    worker.join().unwrap();
}
