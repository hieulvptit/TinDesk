use super::*;
pub(super) fn create_description(
    shared: &Arc<Shared>,
    engine: &Arc<Engine>,
    offer: bool,
) -> Result<()> {
    if offer && engine.negotiating.swap(true, Ordering::AcqRel) {
        return Err(TransportError::NegotiationBusy);
    }
    let epoch = engine.epoch;
    let weak = Arc::downgrade(shared);
    let weak_engine = Arc::downgrade(engine);
    let promise = gst::Promise::with_change_func(move |reply| {
        let (Some(shared), Some(engine)) = (active(&weak, epoch), weak_engine.upgrade()) else {
            return;
        };
        let key = if offer { "offer" } else { "answer" };
        let description = reply
            .ok()
            .flatten()
            .and_then(|r| r.get::<gst_webrtc::WebRTCSessionDescription>(key).ok());
        let Some(description) = description else {
            shared.fail_at(epoch, TransportError::NativeOperation);
            return;
        };
        let Ok(sdp) = description.sdp().as_text() else {
            shared.fail_at(epoch, TransportError::NativeOperation);
            return;
        };
        let weak = Arc::downgrade(&shared);
        let weak_engine = Arc::downgrade(&engine);
        let promise = gst::Promise::with_change_func(move |reply| {
            let (Some(shared), Some(engine)) = (active(&weak, epoch), weak_engine.upgrade()) else {
                return;
            };
            if reply.is_err() || reply.ok().flatten().is_some_and(|r| r.has_field("error")) {
                shared.fail_at(epoch, TransportError::NativeOperation);
                return;
            }
            let session_id = shared.config.lock().unwrap().session_id;
            let message = if offer {
                SignalMessage::Offer {
                    session_id,
                    epoch,
                    sdp,
                }
            } else {
                engine.negotiating.store(false, Ordering::Release);
                SignalMessage::Answer {
                    session_id,
                    epoch,
                    sdp,
                }
            };
            if encode_control(&message).is_err() {
                shared.fail_at(epoch, TransportError::NativeOperation);
                return;
            }
            shared.emit_at(epoch, PeerEvent::Signal(message));
        });
        engine
            .webrtc
            .emit_by_name::<()>("set-local-description", &[&description, &promise]);
    });
    engine.webrtc.emit_by_name::<()>(
        if offer {
            "create-offer"
        } else {
            "create-answer"
        },
        &[&None::<gst::Structure>, &promise],
    );
    Ok(())
}

pub(super) fn apply_remote(
    shared: &Arc<Shared>,
    engine: &Arc<Engine>,
    sdp: String,
    offer: bool,
) -> Result<()> {
    let parsed = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
        .map_err(|_| ProtocolError::InvalidPayload)?;
    if parsed.medias_len() == 0 || parsed.medias_len() > 8 {
        return Err(ProtocolError::InvalidPayload.into());
    }
    let description = gst_webrtc::WebRTCSessionDescription::new(
        if offer {
            gst_webrtc::WebRTCSDPType::Offer
        } else {
            gst_webrtc::WebRTCSDPType::Answer
        },
        parsed,
    );
    {
        let mut signaling = engine.signaling.lock().unwrap();
        if signaling.applying || (offer && engine.negotiating.swap(true, Ordering::AcqRel)) {
            return Err(TransportError::NegotiationBusy);
        }
        signaling.applying = true;
    }
    let epoch = engine.epoch;
    let weak = Arc::downgrade(shared);
    let weak_engine = Arc::downgrade(engine);
    let promise = gst::Promise::with_change_func(move |reply| {
        let (Some(shared), Some(engine)) = (active(&weak, epoch), weak_engine.upgrade()) else {
            return;
        };
        if reply.is_err() || reply.ok().flatten().is_some_and(|r| r.has_field("error")) {
            shared.fail_at(epoch, TransportError::NativeOperation);
            return;
        }
        {
            let mut signaling = engine.signaling.lock().unwrap();
            signaling.remote_ready = true;
            signaling.applying = false;
            for (index, candidate) in signaling.early_ice.drain(..) {
                engine
                    .webrtc
                    .emit_by_name::<()>("add-ice-candidate", &[&index, &candidate]);
            }
        }
        if offer {
            if let Err(error) = create_description(&shared, &engine, false) {
                shared.fail_at(epoch, error);
            }
        } else {
            engine.negotiating.store(false, Ordering::Release);
        }
    });
    engine
        .webrtc
        .emit_by_name::<()>("set-remote-description", &[&description, &promise]);
    Ok(())
}

pub(super) fn rebuild(shared: &Arc<Shared>, epoch: u64, ice: Option<Vec<IceServer>>) -> Result<()> {
    let _rebuild = shared.rebuild.lock().unwrap();
    rebuild_locked(shared, epoch, ice)
}
pub(super) fn rebuild_locked(
    shared: &Arc<Shared>,
    epoch: u64,
    ice: Option<Vec<IceServer>>,
) -> Result<()> {
    shared.engine()?;
    if epoch
        != shared
            .epoch
            .load(Ordering::Acquire)
            .checked_add(1)
            .ok_or(TransportError::StaleEpoch)?
    {
        return Err(TransportError::StaleEpoch);
    }
    {
        let _gate = shared.event_gate.lock().unwrap();
        if shared.closed.load(Ordering::Acquire) {
            return Err(TransportError::Closed);
        }
        shared.negotiation_ready.store(false, Ordering::Release);
        shared.epoch.store(epoch, Ordering::Release); // invalidate callbacks before teardown
        while shared.events_drain.try_recv().is_ok() {} // discard pending old-session input/media/signals
    }
    let local_restart = ice.is_some();
    if let Some(ice) = ice {
        shared.config.lock().unwrap().ice_servers = ice;
    }
    shared.emit(PeerEvent::ConnectionChanged(ConnectionState::Restarting));
    let old = shared.engine.lock().unwrap().take();
    #[cfg(test)]
    shared.hooks.engine_removed.pause();
    if let Some(old) = old {
        let _ = old.pipeline.set_state(gst::State::Null);
    }
    if shared.closed.load(Ordering::Acquire) {
        return Err(TransportError::Closed);
    }
    shared.outgoing_motion.lock().unwrap().take();
    shared.incoming_motion.lock().unwrap().take();
    shared.frame.lock().unwrap().take();
    *shared.counters.native.lock().unwrap() = crate::stats::NativeStats::default();
    let config = shared.config.lock().unwrap().clone();
    // A transport rebuild must not extend the authorization lease.
    let mut decoder = shared.input.lock().unwrap();
    let last_renewed_ms = decoder.last_renewed_ms();
    *decoder = InputDecoder::new(config.session_id, config.display, last_renewed_ms)?;
    drop(decoder);
    let engine = match build_engine(shared, epoch) {
        Ok(engine) => engine,
        Err(error) => {
            shared.fail(error.clone());
            return Err(error);
        }
    };
    {
        // Public close and native failure transitions use the same gate. A
        // replacement can be published only before a closure transition.
        let gate = shared.event_gate.lock().unwrap();
        if shared.closed.load(Ordering::Acquire) {
            drop(gate);
            let _ = engine.pipeline.set_state(gst::State::Null);
            return Err(TransportError::Closed);
        }
        *shared.engine.lock().unwrap() = Some(engine);
    }
    if let Some(display) = config.display {
        shared.emit_at(epoch, PeerEvent::DisplayChanged(display));
        shared.emit_at(
            epoch,
            PeerEvent::InputReset {
                generation: display.generation,
            },
        );
    }
    if config.display.is_none()
        && let Some(generation) = *shared.display_generation.lock().unwrap()
    {
        shared.emit_at(epoch, PeerEvent::DisplayUnavailable { generation });
        shared.emit_at(epoch, PeerEvent::InputReset { generation });
    }
    if local_restart {
        #[cfg(test)]
        shared.hooks.before_restart_marker.pause();
        shared.engine()?;
        let session_id = shared.config.lock().unwrap().session_id;
        if config.role == PeerRole::Agent
            && let Some(display) = config.display
        {
            shared.emit_at(
                epoch,
                PeerEvent::Signal(SignalMessage::Display {
                    session_id,
                    epoch: epoch - 1,
                    display,
                }),
            );
        }
        if config.role == PeerRole::Agent
            && config.display.is_none()
            && let Some(generation) = *shared.display_generation.lock().unwrap()
        {
            shared.emit_at(
                epoch,
                PeerEvent::Signal(SignalMessage::DisplayUnavailable {
                    session_id,
                    epoch: epoch - 1,
                    generation,
                }),
            );
        }
        shared.emit_at(
            epoch,
            PeerEvent::Signal(SignalMessage::Restart { session_id, epoch }),
        );
    }
    shared.negotiation_ready.store(true, Ordering::Release);
    shared.emit_at(epoch, PeerEvent::NegotiationNeeded);
    Ok(())
}

pub(super) fn validate_ice(servers: &[IceServer]) -> Result<()> {
    let urls = ice_urls(servers)?;
    if urls.iter().filter(|(is_stun, _)| *is_stun).count() > 1 {
        return Err(TransportError::InvalidIce);
    }
    Ok(())
}
fn ice_urls(servers: &[IceServer]) -> Result<Vec<(bool, String)>> {
    if servers.len() > 16 {
        return Err(TransportError::InvalidIce);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| TransportError::InvalidIce)?
        .as_secs();
    let mut urls = Vec::new();
    for server in servers {
        if server.urls.is_empty()
            || server
                .expires_at_unix_secs
                .is_some_and(|expiry| expiry <= now)
        {
            return Err(TransportError::InvalidIce);
        }
        for raw in &server.urls {
            if urls.len() >= 32 || raw.len() > 4096 {
                return Err(TransportError::InvalidIce);
            }
            let (scheme, rest) = raw.split_once(':').ok_or(TransportError::InvalidIce)?;
            if !matches!(scheme, "stun" | "turn" | "turns") {
                return Err(TransportError::InvalidIce);
            }
            let mut url = url::Url::parse(&format!("{scheme}://{}", rest.trim_start_matches('/')))
                .map_err(|_| TransportError::InvalidIce)?;
            if url.host_str().is_none() || url.fragment().is_some() {
                return Err(TransportError::InvalidIce);
            }
            if scheme != "stun" {
                let username = server
                    .username
                    .as_deref()
                    .ok_or(TransportError::InvalidIce)?;
                let credential = server
                    .credential
                    .as_deref()
                    .ok_or(TransportError::InvalidIce)?;
                if username.len() > 4096 || credential.len() > 4096 {
                    return Err(TransportError::InvalidIce);
                }
                url.set_username(username)
                    .map_err(|_| TransportError::InvalidIce)?;
                url.set_password(Some(credential))
                    .map_err(|_| TransportError::InvalidIce)?;
            } else if !url.username().is_empty() || url.password().is_some() {
                return Err(TransportError::InvalidIce);
            }
            urls.push((scheme == "stun", url.to_string()));
        }
    }
    Ok(urls)
}
pub(super) fn configure_ice(webrtc: &gst::Element, servers: &[IceServer]) -> Result<()> {
    let mut stun = false;
    for (is_stun, url) in ice_urls(servers)? {
        if is_stun {
            if stun {
                return Err(TransportError::InvalidIce);
            }
            webrtc.set_property("stun-server", url);
            stun = true;
        } else if !webrtc.emit_by_name::<bool>("add-turn-server", &[&url]) {
            return Err(TransportError::InvalidIce);
        }
    }
    Ok(())
}
