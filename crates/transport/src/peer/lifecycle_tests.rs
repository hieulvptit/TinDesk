//! Deterministic interleavings against real native engines. Hooks are test-only.
use super::*;

#[derive(Default)]
pub(super) struct Hooks {
    pub engine_removed: Point,
    pub before_restart_marker: Point,
    pub end_validated: Point,
    pub close_requested: Point,
    pub negotiation_requested: Point,
    pub monitor_before_lock: Point,
}
#[derive(Default)]
pub(super) struct Point(Mutex<Option<Pause>>);
struct Pause {
    arrived: Sender<()>,
    resume: Receiver<()>,
}
struct Ticket {
    arrived: Receiver<()>,
    resume: Option<Sender<()>>,
}
impl Point {
    fn install(&self) -> Ticket {
        let (arrived_tx, arrived) = bounded(1);
        let (resume, resume_rx) = bounded(1);
        *self.0.lock().unwrap() = Some(Pause {
            arrived: arrived_tx,
            resume: resume_rx,
        });
        Ticket {
            arrived,
            resume: Some(resume),
        }
    }
    pub(super) fn pause(&self) {
        let pause = self.0.lock().unwrap().take();
        if let Some(pause) = pause
            && pause.arrived.send(()).is_ok()
        {
            pause
                .resume
                .recv_timeout(Duration::from_secs(10))
                .expect("lifecycle point was not resumed");
        }
    }
}
impl Ticket {
    fn wait(&self) {
        self.arrived
            .recv_timeout(Duration::from_secs(10))
            .expect("lifecycle point not reached");
    }
    fn release(&mut self) {
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.release();
    }
}
fn peer() -> Arc<Peer> {
    Arc::new(Peer::new(super::tests::config(PeerRole::Controller)).unwrap())
}

fn next_signal(events: &Receiver<PeerEvent>) -> SignalMessage {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match events
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .expect("native signaling deadline expired")
        {
            PeerEvent::Signal(signal) => return signal,
            PeerEvent::Error(error) => panic!("{error}"),
            _ => {}
        }
    }
}

#[test]
fn lifecycle_close_during_engine_removal_cannot_leave_a_playing_replacement() {
    let peer = peer();
    let old_engine = peer.shared.engine().unwrap();
    let mut removed = peer.shared.hooks.engine_removed.install();
    let mut closing = peer.shared.hooks.close_requested.install();
    let restart_peer = peer.clone();
    let restart = thread::spawn(move || restart_peer.restart_ice(vec![]));
    removed.wait();
    let close_peer = peer.clone();
    let close = thread::spawn(move || close_peer.close());
    closing.wait(); // close has set closed while rebuild owns no published engine
    closing.release();
    removed.release();
    let result = restart.join().unwrap();
    close.join().unwrap();
    assert_eq!(result, Err(TransportError::Closed));
    assert_eq!(old_engine.pipeline.current_state(), gst::State::Null);
    assert!(peer.is_closed());
    assert!(
        peer.shared
            .engine
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|e| e.pipeline.current_state() == gst::State::Null)
    );
}

#[test]
fn lifecycle_restart_marker_precedes_concurrent_native_offer_and_ice() {
    let peer = peer();
    let mut monitor = peer.shared.hooks.monitor_before_lock.install();
    monitor.wait();
    let mut marker = peer.shared.hooks.before_restart_marker.install();
    let mut requested = peer.shared.hooks.negotiation_requested.install();
    let restart_peer = peer.clone();
    let restart = thread::spawn(move || restart_peer.restart_ice(vec![]));
    marker.wait();
    let offer_peer = peer.clone();
    let offer = thread::spawn(move || offer_peer.start_negotiation());
    requested.wait();
    // Both competing lifecycle users are held outside the gate. This makes
    // inspection of the restart transaction independent of scheduler timing.
    let marker_is_serialized = peer.shared.rebuild.try_lock().is_err();
    let events = peer.shared.events_drain.clone();
    let premature_signal = if !marker_is_serialized {
        requested.release();
        Some(next_signal(&events))
    } else {
        None
    };
    marker.release();
    requested.release();
    monitor.release();
    assert_eq!(restart.join().unwrap(), Ok(()));
    assert_eq!(offer.join().unwrap(), Ok(()));
    let first_signal = premature_signal.unwrap_or_else(|| next_signal(&events));
    let marker_was_first = matches!(first_signal, SignalMessage::Restart { epoch: 1, .. });
    let mut saw_offer = matches!(&first_signal, SignalMessage::Offer { epoch: 1, .. });
    let mut saw_ice = matches!(&first_signal, SignalMessage::IceCandidate { epoch: 1, .. });
    while !saw_offer || !saw_ice {
        match next_signal(&events) {
            SignalMessage::Offer { epoch: 1, .. } => saw_offer = true,
            SignalMessage::IceCandidate { epoch: 1, .. } => saw_ice = true,
            _ => {}
        }
    }
    peer.close();
    assert!(marker_was_first, "new epoch signaling preceded Restart");
    assert!(
        marker_is_serialized,
        "restart released lifecycle gate before its marker"
    );
}

#[test]
fn lifecycle_end_validated_before_restart_cannot_close_the_new_epoch() {
    let peer = peer();
    let mut validated = peer.shared.hooks.end_validated.install();
    let end_peer = peer.clone();
    let end = thread::spawn(move || {
        end_peer.handle_signal(SignalMessage::End {
            session_id: super::tests::config(PeerRole::Controller).session_id,
            epoch: 0,
        })
    });
    validated.wait();
    assert_eq!(peer.restart_ice(vec![]), Ok(()));
    validated.release();
    let result = end.join().unwrap();
    let replacement_open = !peer.shared.closed.load(Ordering::Acquire);
    let replacement_playing = peer
        .shared
        .engine
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|e| e.epoch == 1 && e.pipeline.current_state() == gst::State::Playing);
    peer.close();
    assert_eq!(result, Err(TransportError::StaleEpoch));
    assert!(replacement_open && replacement_playing);
}
