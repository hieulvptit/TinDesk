use crate::Result;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio_util::sync::CancellationToken;

/// Authority comes only from authenticated renewal, conservatively dated at request start.
#[derive(Clone, Default)]
pub struct Authority {
    pub deadline: Arc<AtomicU64>,
}
impl Authority {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn renewed(&self, request_start: u64, now: u64) -> Result<()> {
        let deadline = request_start.checked_add(60_000).ok_or("lease overflow")?;
        let previous = self.deadline.load(Ordering::Acquire);
        if now >= deadline || deadline < previous || (previous != 0 && now >= previous) {
            return Err("invalid renewal deadline".into());
        }
        self.deadline.store(deadline, Ordering::Release);
        Ok(())
    }
    pub async fn watch(&self, now: impl Fn() -> u64, stop: CancellationToken) {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(5));
        loop {
            tokio::select! {
              _=stop.cancelled()=>return,
              _=tick.tick()=> {
                let deadline=self.deadline.load(Ordering::Acquire);
                if deadline != 0 && now() >= deadline { stop.cancel(); return; }
              }
            }
        }
    }
}

pub fn boot_time_ms() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut value = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut value) } != 0 {
            return u64::MAX;
        }
        value.tv_sec as u64 * 1000 + value.tv_nsec as u64 / 1_000_000
    }
    #[cfg(not(target_os = "linux"))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START
            .get_or_init(std::time::Instant::now)
            .elapsed()
            .as_millis() as u64
    }
}

use crate::{config::Config, helper::Helper};
use remotevnc_device::{DeviceApi, DeviceIdentity};
use remotevnc_protocol::*;
use remotevnc_transport::{ConnectionState, Peer, PeerConfig, PeerEvent, PeerRole, TransportError};
use std::{
    collections::HashSet,
    net::Shutdown,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
    time::Instant,
};

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(u64::MAX, |v| v.as_millis() as u64)
}
enum NetworkEvent {
    Event(DeviceEvent),
    Offline,
    PolicyClosed,
}

pub struct AgentService {
    config: Config,
}
impl AgentService {
    pub fn new(config: Config) -> Result<Self> {
        config.validate()?;
        Ok(Self { config })
    }
    pub async fn run(self, shutdown: CancellationToken) -> Result<()> {
        let _lock = self.config.lock()?;
        if self.config.revoked_path().exists() {
            return Err("locally revoked; enroll explicitly before running".into());
        }
        let credentials = self.config.credentials()?;
        let identity = Arc::new(DeviceIdentity {
            device_id: credentials.device_id.ok_or("enrollment incomplete")?,
            signing_key: credentials.signing_key,
        });
        let api = Arc::new(DeviceApi::new(
            &self.config.backend_origin,
            identity.signing_key.clone(),
        )?);
        let stop = shutdown.child_token();
        // This OS thread can cancel credentials and helper authority even if native work stalls.
        let revoke_stop = stop.clone();
        let revoked = self.config.revoked_path();
        std::thread::Builder::new()
            .name("agent-local-revoke".into())
            .spawn(move || {
                while !revoke_stop.is_cancelled() {
                    if revoked.exists() {
                        revoke_stop.cancel();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })?;
        let (event_tx, mut events) = mpsc::channel(32);
        let (command_tx, command_rx) = mpsc::channel(32);
        let (token_tx, token_rx) = watch::channel(None);
        let network = tokio::spawn(network(
            api.clone(),
            identity.clone(),
            event_tx,
            command_rx,
            token_tx,
            stop.clone(),
        ));
        let mut live: Option<Live> = None;
        let mut renewal: Option<
            JoinHandle<(SessionId, u64, remotevnc_device::Result<SessionGrant>)>,
        > = None;
        let mut pending: Option<SessionGrant> = None;
        let mut replayed = HashSet::new();
        let mut next_renew = Instant::now() + Duration::from_secs(15);
        let mut offline_until: Option<Instant> = None;
        let mut turn_job: Option<JoinHandle<remotevnc_device::Result<()>>> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let mut end = false;
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                message = events.recv() => match message {
                    None => break,
                    Some(NetworkEvent::Offline) => {
                        offline_until.get_or_insert(Instant::now() + Duration::from_secs(10));
                    }
                    Some(NetworkEvent::PolicyClosed) => {
                        end = true;
                    }
                    Some(NetworkEvent::Event(event)) => {
                        let result = (|| -> Result<()> {
                            match event {
                                DeviceEvent::Presence { .. } => {}
                                DeviceEvent::SessionSnapshot { session_ids } => {
                                    offline_until = None;
                                    if live
                                        .as_ref()
                                        .is_some_and(|l| !session_ids.contains(&l.grant.session_id))
                                    {
                                        end = true;
                                    }
                                    // Unknown replayed sessions cannot resurrect a previous process's epoch.
                                    for id in session_ids {
                                        if !live.as_ref().is_some_and(|l| l.grant.session_id == id)
                                            && !pending.as_ref().is_some_and(|g| g.session_id == id)
                                        {
                                            replayed.insert(id);
                                            close_remote(api.clone(), &token_rx, id);
                                        }
                                    }
                                }
                                DeviceEvent::Session { grant } => {
                                    if grant.agent_id != identity.device_id
                                        || grant.client_id == identity.device_id
                                    {
                                        return Err("wrong grant participants".into());
                                    }
                                    if live
                                        .as_ref()
                                        .is_some_and(|l| l.grant.session_id == grant.session_id)
                                        || pending
                                            .as_ref()
                                            .is_some_and(|g| g.session_id == grant.session_id)
                                    {
                                        return Ok(());
                                    }
                                    if replayed.contains(&grant.session_id)
                                        || live.is_some()
                                        || pending.is_some()
                                    {
                                        close_remote(api.clone(), &token_rx, grant.session_id);
                                        return Ok(());
                                    }
                                    let token = token_rx.borrow().clone().ok_or("no authenticated token")?;
                                    let id = grant.session_id;
                                    pending = Some(grant);
                                    next_renew = Instant::now() + Duration::from_secs(15);
                                    renewal = Some(renew(api.clone(), token, id));
                                }
                                DeviceEvent::SessionClosed { session_id } => {
                                    replayed.remove(&session_id);
                                    if live
                                        .as_ref()
                                        .is_some_and(|l| l.grant.session_id == session_id)
                                        || pending.as_ref().is_some_and(|g| g.session_id == session_id)
                                    {
                                        end = true;
                                    }
                                }
                                DeviceEvent::Signal { sender, message } => {
                                    let l = live.as_mut().ok_or("signal without active peer")?;
                                    l.signal(sender, message)?;
                                }
                                DeviceEvent::IceConfiguration {
                                    session_id,
                                    generation,
                                    requires_ack,
                                    ice_servers,
                                } => {
                                    let l = live
                                        .as_mut()
                                        .ok_or("ICE configuration without active peer")?;
                                    if session_id != l.grant.session_id {
                                        return Err("wrong ICE session".into());
                                    }
                                    if generation < l.installed_generation {
                                        return Ok(());
                                    }
                                    if generation == l.installed_generation && l.ice != ice_servers {
                                        return Err("conflicting ICE generation".into());
                                    }
                                    l.peer.install_ice(ice_servers.clone())?;
                                    l.ice = ice_servers;
                                    l.installed_generation = generation;
                                    l.turn_due = l
                                        .ice
                                        .iter()
                                        .filter_map(|i| i.expires_at_unix_secs)
                                        .min()
                                        .map(|v| v.saturating_sub(300));
                                    if requires_ack {
                                        command_tx
                                            .try_send(DeviceCommand::IceInstalled {
                                                session_id,
                                                generation,
                                            })
                                            .map_err(|_| "signaling queue full")?;
                                    }
                                }
                                DeviceEvent::IceRestartReady {
                                    session_id,
                                    generation,
                                } => {
                                    let l = live
                                        .as_mut()
                                        .ok_or("restart readiness without active peer")?;
                                    if session_id != l.grant.session_id || generation > l.installed_generation {
                                        return Err("invalid restart readiness".into());
                                    }
                                    if generation > l.restarted_generation {
                                        l.release()?;
                                        l.peer.restart_ice(l.ice.clone())?;
                                        l.restarted_generation = generation;
                                        l.negotiation_until = Some(Instant::now() + Duration::from_secs(20));
                                    }
                                }
                            }
                            Ok(())
                        })();
                        if result.is_err() {
                            end = true;
                        }
                    }
                },
                _ = tick.tick() => {
                    if offline_until.is_some_and(|deadline| Instant::now() >= deadline) {
                        end = true;
                    }
                    if renewal.as_ref().is_some_and(|j| j.is_finished()) {
                        let result = renewal.take().unwrap().await;
                        match result {
                            Ok((id, started, Ok(grant)))
                                if grant.session_id == id
                                    && grant.agent_id == identity.device_id
                                    && grant.lease_expires_at > unix_ms() =>
                            {
                                if let Some(l) = live.as_mut() {
                                    if l.grant.session_id != id
                                        || l.grant.client_id != grant.client_id
                                        || l.renewed(started).is_err()
                                    {
                                        end = true;
                                    }
                                } else if let Some(expected) = pending.take() {
                                    if expected.session_id != id || expected.client_id != grant.client_id {
                                        end = true;
                                    } else {
                                        match Live::new(&self.config, grant, started, stop.child_token()) {
                                            Ok(l) => live = Some(l),
                                            Err(_) => {
                                                close_remote(api.clone(), &token_rx, id);
                                                end = true;
                                            }
                                        }
                                    }
                                }
                            }
                            Ok((_, _, Err(remotevnc_device::Error::Unauthorized))) => end = true,
                            // A transient HTTP failure cannot create new authority. Retain the
                            // last authenticated deadline; the independent watchdog still expires.
                            Ok((_, _, Err(_))) if live.is_some() => {}
                            _ => end = true,
                        }
                    }
                    if let Some(l) = live.as_mut() {
                        if l.poll(&command_tx).is_err() || l.stop.is_cancelled() {
                            end = true;
                        }
                        if Instant::now() >= next_renew && renewal.is_none() {
                            if let Some(token) = token_rx.borrow().clone() {
                                renewal = Some(renew(api.clone(), token, l.grant.session_id));
                            }
                            next_renew = Instant::now() + Duration::from_secs(15);
                        }
                        if l.turn_due.is_some_and(|t| unix_ms() / 1000 >= t)
                            && turn_job.is_none()
                            && let Some(token) = token_rx.borrow().clone()
                        {
                            let api = api.clone();
                            let id = l.grant.session_id;
                            turn_job = Some(tokio::spawn(
                                async move { api.refresh_turn(&token, id).await },
                            ));
                            l.turn_due = None;
                        }
                    }
                    if turn_job.as_ref().is_some_and(|j| j.is_finished())
                        && !matches!(turn_job.take().unwrap().await, Ok(Ok(())))
                    {
                        end = true;
                    }
                }
            }
            if end {
                if let Some(job) = renewal.take() {
                    job.abort();
                }
                if let Some(job) = turn_job.take() {
                    job.abort();
                }
                if let Some(l) = live.take() {
                    let id = l.grant.session_id;
                    drop(l);
                    close_remote(api.clone(), &token_rx, id);
                }
                if let Some(grant) = pending.take() {
                    close_remote(api.clone(), &token_rx, grant.session_id);
                }
            }
        }
        stop.cancel();
        if let Some(job) = renewal {
            job.abort();
        }
        if let Some(job) = turn_job {
            job.abort();
        }
        if let Some(l) = live {
            let id = l.grant.session_id;
            drop(l);
            close_remote(api.clone(), &token_rx, id);
        }
        network.abort();
        let _ = network.await;
        Ok(())
    }
}
fn renew(
    api: Arc<DeviceApi>,
    token: AccessToken,
    id: SessionId,
) -> JoinHandle<(SessionId, u64, remotevnc_device::Result<SessionGrant>)> {
    tokio::spawn(async move {
        let started = boot_time_ms();
        (id, started, api.renew_session(&token, id).await)
    })
}
fn close_remote(api: Arc<DeviceApi>, token: &watch::Receiver<Option<AccessToken>>, id: SessionId) {
    if let Some(token) = token.borrow().clone() {
        tokio::spawn(async move {
            let _ = api.close_session(&token, id).await;
        });
    }
}
async fn network(
    api: Arc<DeviceApi>,
    identity: Arc<DeviceIdentity>,
    events: mpsc::Sender<NetworkEvent>,
    mut commands: mpsc::Receiver<DeviceCommand>,
    tokens: watch::Sender<Option<AccessToken>>,
    stop: CancellationToken,
) {
    let mut backoff = Duration::from_millis(250);
    loop {
        let connection = async {
            let token = api.authenticate(&identity).await?;
            let socket = api.events(&token, DeviceRole::Agent).await?;
            Ok::<_, remotevnc_device::Error>((token, socket))
        };
        let result = tokio::select! {_=stop.cancelled()=>return,result=connection=>result};
        if matches!(result, Err(remotevnc_device::Error::Unauthorized)) {
            let _ = events.try_send(NetworkEvent::PolicyClosed);
        }
        if let Ok((token, socket)) = result {
            backoff = Duration::from_millis(250);
            let rotation = Duration::from_millis(
                token
                    .expires_at_unix_ms
                    .saturating_sub(unix_ms())
                    .saturating_sub(30_000)
                    .clamp(1, 240_000),
            );
            tokens.send_replace(Some(token));
            let (mut sender, mut receiver) = socket.split();
            let rotate = tokio::time::sleep(rotation);
            tokio::pin!(rotate);
            loop {
                let result = tokio::select! {
                  _=stop.cancelled()=>return,
                  _=&mut rotate=>break,
                  event=receiver.next()=>match event {
                    Ok(Some(event))=>events.try_send(NetworkEvent::Event(event)).map_err(|_|()),
                    Err(remotevnc_device::Error::PolicyClosed)=>{let _=events.try_send(NetworkEvent::PolicyClosed);Err(())},
                    _=>Err(()),
                  },
                  command=commands.recv()=>match command {Some(command)=>sender.send(&command).await.map_err(|_|()),None=>return},
                };
                if result.is_err() {
                    break;
                }
            }
            // Dropping both halves releases registration; bounded retry handles 409 cleanup.
            let _ = sender.close().await;
        }
        tokens.send_replace(None);
        if events.try_send(NetworkEvent::Offline).is_err() {
            return;
        }
        // No signal queued in an old epoch is replayed after reconnect.
        while commands.try_recv().is_ok() {}
        tokio::select! {_=stop.cancelled()=>return,_=tokio::time::sleep(backoff)=>{}}
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

struct Live {
    grant: SessionGrant,
    helper: Helper,
    peer: Arc<Peer>,
    events: remotevnc_transport::Receiver<PeerEvent>,
    authority: Authority,
    stop: CancellationToken,
    display: Option<DisplayInfo>,
    guard: Option<InputGuard>,
    acknowledged: bool,
    ice: Vec<IceServer>,
    installed_generation: u64,
    restarted_generation: u64,
    turn_due: Option<u64>,
    negotiation_until: Option<Instant>,
    helper_transition_until: Option<Instant>,
    profile: VideoProfile,
    adaptation: remotevnc_transport::stats::AdaptiveProfile,
    next_media_sample: Instant,
}
impl Live {
    fn new(
        config: &Config,
        grant: SessionGrant,
        started: u64,
        stop: CancellationToken,
    ) -> Result<Self> {
        let helper = Helper::connect(&config.helper_socket)?;
        let authority = Authority::new();
        authority.renewed(started, boot_time_ms())?;
        let socket = helper.shutdown_socket();
        let watch_authority = authority.clone();
        let watch_stop = stop.clone();
        let helper_failure = helper.failed.clone();
        let watched_peer: Arc<std::sync::Mutex<Option<Arc<Peer>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let peer_slot = watched_peer.clone();
        std::thread::Builder::new()
            .name("agent-authority".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .build();
                if let Ok(runtime) = runtime {
                    runtime.block_on(async {
                        tokio::select! {
                          _=watch_authority.watch(boot_time_ms,watch_stop.clone())=>{},
                          _=helper_failure.cancelled()=>{},
                        }
                    });
                }
                watch_stop.cancel();
                // No HTTP, frame parsing or native lifecycle lock precedes helper shutdown.
                let _ = socket.shutdown(Shutdown::Both);
                let peer = peer_slot.lock().unwrap().take();
                if let Some(peer) = peer {
                    peer.close();
                }
            })?;
        let result = (|| -> Result<_> {
            let mut peer = Peer::new(PeerConfig {
                session_id: grant.session_id,
                local_peer_id: grant.agent_id,
                remote_peer_id: grant.client_id,
                role: PeerRole::Agent,
                display: None,
                ice_servers: grant.ice_servers.clone(),
                relay_only: config.relay_only,
            })?;
            peer.enable_h264()?;
            let events = peer.events().clone();
            let peer = Arc::new(peer);
            *watched_peer.lock().unwrap() = Some(peer.clone());
            if stop.is_cancelled() {
                peer.close();
                return Err("authority ended during media initialization".into());
            }
            Ok((peer, events))
        })();
        let (peer, events) = match result {
            Ok(v) => v,
            Err(e) => {
                stop.cancel();
                return Err(e);
            }
        };
        Ok(Self {
            turn_due: grant.turn_refresh_at_unix_secs(),
            ice: grant.ice_servers.clone(),
            grant,
            helper,
            peer,
            events,
            authority,
            stop,
            display: None,
            guard: None,
            acknowledged: false,
            installed_generation: 0,
            restarted_generation: 0,
            negotiation_until: None,
            helper_transition_until: None,
            profile: VideoProfile::default(),
            adaptation: remotevnc_transport::stats::AdaptiveProfile::new(VideoProfile::default())?,
            next_media_sample: Instant::now() + Duration::from_secs(1),
        })
    }
    fn renewed(&mut self, started: u64) -> Result<()> {
        self.authority.renewed(started, boot_time_ms())?;
        self.peer.renew_input_lease()?;
        if let Some(guard) = self.guard.as_mut() {
            guard.renew_lease(started);
        }
        if let Some(display) = self.display {
            self.helper.send(HelperRequest::RenewLease {
                session_id: self.grant.session_id,
                generation: display.generation,
                deadline_boot_ms: self.authority.deadline.load(Ordering::Acquire),
            })?;
        }
        Ok(())
    }
    fn release(&mut self) -> Result<()> {
        self.acknowledged = false;
        if let Some(display) = self.display {
            self.helper.send(HelperRequest::ReleaseAll {
                session_id: self.grant.session_id,
                generation: display.generation,
            })?;
        }
        Ok(())
    }
    fn signal(&mut self, sender: DeviceId, message: SignalMessage) -> Result<()> {
        let (id, epoch) = signal_scope(&message);
        if sender != self.grant.client_id || id != self.grant.session_id {
            return Err("wrong signaling scope".into());
        }
        if epoch < self.peer.epoch() {
            return Ok(());
        }
        // Agent exclusively owns rebuild initiation.
        if matches!(message, SignalMessage::Restart { .. }) {
            return Err("controller cannot restart agent".into());
        }
        self.peer.handle_signal(message)?;
        Ok(())
    }
    fn poll(&mut self, commands: &mpsc::Sender<DeviceCommand>) -> Result<()> {
        if self.stop.is_cancelled()
            || self.helper.failed.is_cancelled()
            || self.peer.is_closed()
            || boot_time_ms() >= self.authority.deadline.load(Ordering::Acquire)
        {
            return Err("local authority or helper ended".into());
        }
        // Bound work per tick so output cannot starve control/deadline processing.
        for _ in 0..32 {
            let Ok(event) = self.helper.events.try_recv() else {
                break;
            };
            match event {
                HelperEvent::Ready => {}
                HelperEvent::Error { .. } => {
                    eprintln!(
                        "remotevnc-agent: helper input unavailable or recovery pending; session closed"
                    );
                    return Err("helper unavailable or input recovery pending".into());
                }
                HelperEvent::Stopped { session_id } => {
                    if session_id != self.grant.session_id {
                        return Err("wrong helper session".into());
                    }
                    self.display = None;
                    self.guard = None;
                    self.acknowledged = false;
                    self.helper_transition_until = Some(Instant::now() + Duration::from_secs(1));
                }
                HelperEvent::DisplayUnavailable { generation } => {
                    self.helper.retire_video();
                    self.helper_transition_until = None;
                    self.display = None;
                    self.guard = None;
                    self.acknowledged = false;
                    self.peer.display_unavailable(generation)?;
                    self.negotiation_until = None;
                }
                HelperEvent::DisplayChanged(display) => {
                    self.helper_transition_until = None;
                    let deadline = self.authority.deadline.load(Ordering::Acquire);
                    self.helper.send(HelperRequest::Start {
                        session_id: self.grant.session_id,
                        display,
                        deadline_boot_ms: deadline,
                    })?;
                    self.helper.send(HelperRequest::StartCapture {
                        session_id: self.grant.session_id,
                        generation: display.generation,
                        profile: self.profile,
                    })?;
                    self.guard = Some(InputGuard::new(
                        self.grant.session_id,
                        display,
                        deadline.saturating_sub(60_000),
                    )?);
                    self.display = Some(display);
                    self.acknowledged = false;
                    self.peer.update_display(display)?;
                    self.negotiation_until = Some(Instant::now() + Duration::from_secs(20));
                }
            }
        }
        for _ in 0..64 {
            let Ok(event) = self.events.try_recv() else {
                break;
            };
            match event {
                PeerEvent::Signal(message) => commands
                    .try_send(DeviceCommand::Signal {
                        receiver: self.grant.client_id,
                        message,
                    })
                    .map_err(|_| "signaling unavailable")?,
                PeerEvent::Input(input) => {
                    self.guard
                        .as_mut()
                        .ok_or("input without physical display")?
                        .accept(&input, boot_time_ms())?;
                    match input.event {
                        InputEvent::DisplayAck => {
                            self.acknowledged = true;
                            self.negotiation_until = None;
                        }
                        InputEvent::ReleaseAll => {}
                        _ if !self.acknowledged => return Err("display is not acknowledged".into()),
                        _ => {}
                    }
                    self.helper.send(HelperRequest::Input(input))?;
                }
                PeerEvent::InputReset { .. } => {
                    self.release()?;
                }
                PeerEvent::KeyframeNeeded => {
                    if let Some(display) = self.display {
                        self.helper.send(HelperRequest::RequestKeyframe {
                            session_id: self.grant.session_id,
                            generation: display.generation,
                        })?;
                    }
                }
                PeerEvent::ConnectionChanged(ConnectionState::Failed | ConnectionState::Closed) => {
                    return Err("media connection ended".into());
                }
                PeerEvent::ConnectionChanged(ConnectionState::Disconnected) => {
                    self.release()?;
                    self.negotiation_until
                        .get_or_insert(Instant::now() + Duration::from_secs(10));
                }
                PeerEvent::Error(_) => return Err("media failure".into()),
                _ => {}
            }
        }
        if self
            .helper_transition_until
            .is_some_and(|t| Instant::now() >= t)
        {
            return Err("helper stopped without replacement display".into());
        }
        if self.negotiation_until.is_some_and(|t| Instant::now() >= t) {
            return Err("fresh session required after lost signaling".into());
        }
        let now = Instant::now();
        let (helper_drops, request_keyframe) = self.helper.video_health(now.into_std())?;
        if let Some(display) = self.display {
            if request_keyframe {
                self.helper.send(HelperRequest::RequestKeyframe {
                    session_id: self.grant.session_id,
                    generation: display.generation,
                })?;
            }
            if now >= self.next_media_sample {
                self.next_media_sample = now + Duration::from_secs(1);
                let mut stats = self.peer.stats();
                stats.dropped_encoded_frames =
                    stats.dropped_encoded_frames.saturating_add(helper_drops);
                if let Some(profile) = self.adaptation.observe(&stats) {
                    self.helper.send(HelperRequest::UpdateProfile {
                        session_id: self.grant.session_id,
                        generation: display.generation,
                        profile,
                    })?;
                    self.profile = profile;
                }
            }
        }
        for _ in 0..2 {
            let Ok(received) = self.helper.video.try_recv() else {
                break;
            };
            let frame = received.frame;
            if self
                .display
                .is_some_and(|d| d.generation == frame.generation)
            {
                let generation = frame.generation;
                let config = frame.is_config_keyframe();
                match self.peer.push_video(frame) {
                    Ok(()) if config => self
                        .helper
                        .video_accepted(generation, received.recovery_revision),
                    Ok(()) => {}
                    Err(TransportError::QueueFull | TransportError::NotReady) => self
                        .helper
                        .video_rejected(generation, std::time::Instant::now()),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(())
    }
}
impl Drop for Live {
    fn drop(&mut self) {
        self.stop.cancel();
        self.helper.disconnect();
        self.peer.close();
    }
}
fn signal_scope(message: &SignalMessage) -> (SessionId, u64) {
    match message {
        SignalMessage::Display {
            session_id, epoch, ..
        }
        | SignalMessage::DisplayUnavailable {
            session_id, epoch, ..
        }
        | SignalMessage::Restart { session_id, epoch }
        | SignalMessage::Offer {
            session_id, epoch, ..
        }
        | SignalMessage::Answer {
            session_id, epoch, ..
        }
        | SignalMessage::IceCandidate {
            session_id, epoch, ..
        }
        | SignalMessage::End { session_id, epoch } => (*session_id, *epoch),
    }
}

#[cfg(all(test, target_os = "linux"))]
mod native_tests;
