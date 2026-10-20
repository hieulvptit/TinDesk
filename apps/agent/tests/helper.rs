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
