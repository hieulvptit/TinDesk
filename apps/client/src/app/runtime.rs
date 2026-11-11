use super::{Command, Config, ControllerSession, NetworkEvent, ViewState};
use crate::{Result, video::LatestFrame};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use binachicken_device::{CredentialStore, DeviceApi, DeviceIdentity, StoredCredentials};
use binachicken_protocol::*;
use binachicken_transport::PeerStats;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub struct ClientRuntime {
    state: Arc<Mutex<ViewState>>,
    credentials: Arc<dyn ClientCredentialStore>,
    stop: CancellationToken,
    revoke: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    commands: mpsc::Sender<Command>,
    overloaded: Arc<Notify>,
}
impl ClientRuntime {
    pub fn set_video_profile(&self, profile: VideoProfile) -> bool {
        profile.validate().is_ok()
            && profile.encoder == VideoEncoder::Auto
            && self.send(Command::VideoProfile(profile))
    }
    pub fn start(
        config: Config,
        state: Arc<Mutex<ViewState>>,
        frames: LatestFrame,
    ) -> Result<Self> {
        let credentials = native_store(&config)?;
        Self::start_with_store(config, state, frames, credentials)
    }
    pub fn start_with_store(
        config: Config,
        state: Arc<Mutex<ViewState>>,
        frames: LatestFrame,
        credentials: Arc<dyn ClientCredentialStore>,
    ) -> Result<Self> {
        let run_state = state.clone();
        let run_credentials = credentials.clone();
        let stop = CancellationToken::new();
        let child = stop.clone();
        let revoke = Arc::new(AtomicBool::new(false));
        let should_revoke = revoke.clone();
        let (commands, rx) = mpsc::channel(64);
        let overloaded = Arc::new(Notify::new());
        let run_overloaded = overloaded.clone();
        let thread = std::thread::Builder::new()
            .name("binachicken-client".into())
            .spawn(move || {
                let state = run_state;
                let result = (|| -> Result<()> {
                    let runtime = tokio::runtime::Runtime::new()?;
                    runtime.block_on(run(
                        config,
                        state.clone(),
                        frames,
                        rx,
                        child,
                        should_revoke,
                        run_credentials,
                        run_overloaded,
                    ))
                })();
                let mut state = state.lock().unwrap();
                state.signed_in = false;
                state.connected = false;
                state.connection_requested = false;
                state.ready = false;
                state.display = None;
                state.enrollment = None;
                state.presence.clear();
                if let Err(e) = result {
                    // All worker errors are fixed public messages; never log credentials or signals.
                    eprintln!("binachicken-client: worker stopped: {e}");
                    state.status = e.to_string();
                }
            })?;
        Ok(Self {
            state,
            credentials,
            stop,
            revoke,
            thread: Some(thread),
            commands,
            overloaded,
        })
    }
    pub(super) fn send(&self, command: Command) -> bool {
        match self.commands.try_send(command) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Fail closed for remote input without cancelling account authentication.
                self.overloaded.notify_one();
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}
impl ClientRuntime {
    pub fn connect(&self, device: DeviceId) -> bool {
        self.send(Command::Connect(device))
    }
    pub fn disconnect(&self) -> bool {
        self.send(Command::Disconnect)
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub fn sign_out(mut self) -> Result<String> {
        self.revoke.store(true, Ordering::Release);
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| "client worker failed during signout")?;
        }
        self.credentials
            .delete()
            .map_err(|_| "Keychain removal failed; local connection has stopped")?;
        let status = self.state.lock().unwrap().status.clone();
        Ok(if status.starts_with("Signed out") {
            status
        } else {
            "Signed out locally; backend revocation was not confirmed".into()
        })
    }
}

impl Drop for ClientRuntime {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
/// Persistence boundary: production uses only the origin-scoped macOS Keychain.
pub trait ClientCredentialStore: CredentialStore + Send + Sync {
    fn delete(&self) -> binachicken_device::Result<()>;
}
#[cfg(target_os = "macos")]
struct NativeCredentials {
    store: binachicken_device::KeychainStore,
}
#[cfg(target_os = "macos")]
impl CredentialStore for NativeCredentials {
    fn load(&self) -> binachicken_device::Result<Option<StoredCredentials>> {
        self.store.load()
    }
    fn save(&self, credentials: &StoredCredentials) -> binachicken_device::Result<()> {
        self.store.save(credentials)
    }
}
#[cfg(target_os = "macos")]
impl ClientCredentialStore for NativeCredentials {
    fn delete(&self) -> binachicken_device::Result<()> {
        self.store.delete()
    }
}
pub(super) fn native_store(config: &Config) -> Result<Arc<dyn ClientCredentialStore>> {
    #[cfg(target_os = "macos")]
    {
        let executable = std::env::current_exe()?;
        let macos = executable.parent().ok_or("invalid app executable")?;
        let bundled = macos.file_name().is_some_and(|name| name == "MacOS")
            && macos
                .parent()
                .is_some_and(|p| p.file_name().is_some_and(|name| name == "Contents"));
        let store = if bundled {
            // Missing/rejected helper is an error; do not silently prompt through another identity.
            binachicken_device::KeychainStore::with_helper(
                config.backend.clone(),
                macos.parent().unwrap().join("Helpers/binachicken-keychain"),
            )
        } else {
            binachicken_device::KeychainStore::new("io.binachicken.client", &config.backend)
        };
        Ok(Arc::new(NativeCredentials { store }))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = config;
        Err("macOS Keychain is required".into())
    }
}
async fn enroll(
    config: &Config,
    state: &Arc<Mutex<ViewState>>,
    store: &dyn ClientCredentialStore,
) -> Result<(Arc<DeviceApi>, Arc<DeviceIdentity>)> {
    let mut credentials = match store.load()? {
        Some(c) => c,
        None => {
            let c = StoredCredentials {
                device_id: None,
                signing_key: ed25519_dalek::SigningKey::from_bytes(&rand::random()),
            };
            store.save(&c)?;
            c
        }
    };
    let api = DeviceApi::new(&config.backend, credentials.signing_key.clone())?;
    if let Some(device_id) = credentials.device_id {
        let identity = DeviceIdentity {
            device_id,
            signing_key: credentials.signing_key.clone(),
        };
        match api.authenticate(&identity).await {
            Ok(_) => return Ok((Arc::new(api), Arc::new(identity))),
            Err(binachicken_device::Error::Unauthorized) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let public = URL_SAFE_NO_PAD.encode(credentials.signing_key.verifying_key().to_bytes());
    let enrollment = api.begin_enrollment(&config.name, &public).await?;
    credentials.device_id = Some(enrollment.challenge.device_id);
    store.save(&credentials)?;
    {
        let mut s = state.lock().unwrap();
        s.enrollment = Some((
            enrollment.display_code.clone(),
            enrollment.browser_url.clone(),
        ));
        s.status = "Finish Google sign-in in your browser to add this Mac to your account.".into();
    }
    while unix_ms() < enrollment.expires_at_unix_ms {
        match api.poll_enrollment(&enrollment).await? {
            EnrollmentStatus::Pending => tokio::time::sleep(Duration::from_secs(2)).await,
            EnrollmentStatus::Enrolled { device_id } => {
                store.save(&credentials)?;
                return Ok((
                    Arc::new(api),
                    Arc::new(DeviceIdentity {
                        device_id,
                        signing_key: credentials.signing_key,
                    }),
                ));
            }
        }
    }
    Err("Browser confirmation expired; sign in again".into())
}
#[allow(clippy::too_many_arguments)]
async fn run(
    config: Config,
    state: Arc<Mutex<ViewState>>,
    frames: LatestFrame,
    mut commands: mpsc::Receiver<Command>,
    stop: CancellationToken,
    revoke: Arc<AtomicBool>,
    credentials: Arc<dyn ClientCredentialStore>,
    overloaded: Arc<Notify>,
) -> Result<()> {
    let (api, identity) = tokio::select! {biased;_=stop.cancelled()=>return Ok(()),result=enroll(&config,&state,credentials.as_ref())=>result?};
    {
        let mut s = state.lock().unwrap();
        s.signed_in = true;
        s.enrollment = None;
        s.status = "Connecting to device service…".into();
    }
    let (event_tx, mut events) = mpsc::channel(128);
    let (signal_tx, signal_rx) = mpsc::channel(128);
    let (token_tx, token_rx) = watch::channel(None);
    let network = tokio::spawn(network(
        api.clone(),
        identity.clone(),
        event_tx,
        signal_rx,
        token_tx,
        stop.clone(),
    ));
    let mut live: Option<ControllerSession> = None;
    let mut video_profile = VideoProfile::default();
    let mut desired: Option<DeviceId> = None;
    let mut next_connect = Instant::now();
    let mut connect_backoff = Duration::from_millis(250);
    let mut closing = tokio::task::JoinSet::new();
    let mut outage: Option<Instant> = None;
    let mut reconciled = false;
    let mut refresh: Option<
        tokio::task::JoinHandle<binachicken_device::Result<Vec<DeviceSummary>>>,
    > = None;
    let mut next_list = Instant::now();
    // Decoded frames cross this tick before the UI sees them.
    let mut tick = tokio::time::interval(Duration::from_millis(3));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let mut end = false;
        tokio::select! {biased;
            _=stop.cancelled()=>break,
            _=overloaded.notified()=>{
                while commands.try_recv().is_ok() {}
                desired=None;
                end=true;
                state.lock().unwrap().status="Remote input queue overloaded; choose the computer to reconnect".into();
                eprintln!("binachicken-client: UI queue overloaded; remote session stopped, account retained");
            },
            command=commands.recv()=>match command {
                None=>break,
                Some(Command::Disconnect)=>{desired=None;end=true;},
                Some(Command::Refresh)=>next_list=Instant::now(),
                Some(Command::VideoProfile(profile))=>{
                    video_profile=profile;
                    if let Some(l)=live.as_mut(){end=l.set_video_profile(profile).is_err();}
                },
                Some(Command::Input{generation,epoch,events})=>{if let Some(l)=live.as_mut(){end=l.input(generation,epoch,events).is_err();}},
                Some(Command::Connect(agent_id))=>{
                    if live.is_none() {
                        desired=Some(agent_id);
                        next_connect=Instant::now();
                        connect_backoff=Duration::from_millis(250);
                    }
                },
            },
            event=events.recv()=>match event {
                None=>break,
                Some(NetworkEvent::Denied)=>return Err("Reauthorization required; sign out and sign in again".into()),
                Some(NetworkEvent::SessionRejected)=>{
                    desired=None;end=true;
                    state.lock().unwrap().status="Remote session rejected; reconnecting to device service".into();
                    eprintln!("binachicken-client: signaling policy closed the session; rechecking device identity");
                },
                Some(NetworkEvent::Offline)=>{state.lock().unwrap().presence.clear();reconciled=false;outage.get_or_insert(Instant::now()+Duration::from_secs(10));state.lock().unwrap().status="Reconnecting to device service…".into();},
                Some(NetworkEvent::Event(event))=>{
                    let result=(||->Result<()> {match event {
                        DeviceEvent::Presence{devices}=>merge_presence(&mut state.lock().unwrap().presence, devices),
                        DeviceEvent::SessionSnapshot{session_ids}=>{
                            outage=None;reconciled=true;
                            if live.as_ref().is_some_and(|l|!session_ids.contains(&l.grant.session_id)) {end=true;}
                            for id in session_ids {if live.as_ref().is_none_or(|l|l.grant.session_id!=id) {close_remote(&mut closing,api.clone(),&token_rx,id);}}
                            if live.is_none() {state.lock().unwrap().status="Choose an online Ubuntu agent".into();}
                        },
                        DeviceEvent::Session{grant}=>{
                            if let Some(l)=live.as_mut().filter(|l|l.grant.session_id==grant.session_id) {l.refreshed(grant)?;}
                            else {close_remote(&mut closing,api.clone(),&token_rx,grant.session_id);}
                        },
                        DeviceEvent::SessionClosed{session_id}=>{if live.as_ref().is_some_and(|l|l.grant.session_id==session_id){eprintln!("binachicken-client: backend reported session closed");end=true;next_list=Instant::now();}},
                        DeviceEvent::Signal{sender,message}=>{if let Some(l)=live.as_mut(){l.signal(sender,message)?;}},
                        DeviceEvent::IceConfiguration{session_id,generation,requires_ack,ice_servers}=>{
                            if let Some(l)=live.as_mut() {if session_id!=l.grant.session_id {return Err("wrong ICE scope".into())}
                                if let Some(command)=l.install_ice(generation,requires_ack,ice_servers)? {signal_tx.try_send(command).map_err(|_|"signaling unavailable")?;}
                            }
                        },
                        DeviceEvent::IceRestartReady{..}=>return Err("controller received agent-only restart readiness".into()),
                    }Ok(())})();
                    if let Err(e)=result {state.lock().unwrap().status=e.to_string();end=true;}
                },
            },
            _=tick.tick()=>{
                while closing.try_join_next().is_some() {}
                if outage.is_some_and(|deadline|Instant::now()>=deadline) {end=true;}
                if let Some(l)=live.as_mut() {
                    match l.poll() {Ok(commands)=>{for command in commands {if signal_tx.try_send(command).is_err() {end=true;}}},Err(e)=>{state.lock().unwrap().status=e.to_string();end=true;}}
                    let mut s=state.lock().unwrap();s.connected=true;s.display=l.display;s.epoch=l.peer.epoch();s.ready=l.ready;s.stats=l.peer.stats();s.session_id=Some(l.grant.session_id);s.connection=Some(l.connection);
                    if outage.is_none() && !end {s.status=if l.ready {"Connected"} else if l.display.is_some(){"Display transition — waiting for video"}else{"Waiting for a physical display"}.into();}
                }
                let connect_token = token_rx.borrow().clone();
                if !end && live.is_none() && reconciled && closing.is_empty() && Instant::now() >= next_connect
                    && let Some(agent_id) = desired
                    && state.lock().unwrap().presence.iter().any(|p|p.device_id==agent_id && p.role==DeviceRole::Agent && p.online)
                    && let Some(token) = connect_token {
                    state.lock().unwrap().status="Connecting…".into();
                    let started=Instant::now();
                    match tokio::select!{biased;_=stop.cancelled()=>break,result=api.create_session(&token,agent_id)=>result} {
                        Ok(grant)=>{
                            let id=grant.session_id;
                            if grant.client_id!=identity.device_id || grant.agent_id!=agent_id {close_remote(&mut closing,api.clone(),&token_rx,id);return Err("invalid session participants".into())}
                            match ControllerSession::new(grant,started,config.relay_only,frames.clone()) {
                                Ok(mut session)=>{
                                    session.set_video_profile(video_profile)?;
                                    live=Some(session);
                                },
                                Err(e)=>{close_remote(&mut closing,api.clone(),&token_rx,id);state.lock().unwrap().status=e.to_string();}
                            }
                        },
                        Err(binachicken_device::Error::Unauthorized | binachicken_device::Error::Denied)=>return Err("Reauthorization required; sign out and sign in again".into()),
                        Err(e)=>state.lock().unwrap().status=e.to_string(),
                    }
                    next_connect=Instant::now()+connect_backoff;
                    connect_backoff=(connect_backoff*2).min(Duration::from_secs(8));
                }
                if Instant::now()>=next_list && refresh.is_none() {
                    if let Some(token)=token_rx.borrow().clone(){let api=api.clone();refresh=Some(tokio::spawn(async move{api.list_devices(&token).await}));}
                    next_list=Instant::now()+Duration::from_secs(15);
                }
                if refresh.as_ref().is_some_and(|job|job.is_finished()) {
                    match refresh.take().unwrap().await {Ok(Ok(devices))=>{
                        if desired.is_some_and(|id| !devices.iter().any(|d|d.id==id && !d.revoked)) {desired=None;end=true;}
                        state.lock().unwrap().devices=devices.into_iter().filter(|d|d.id!=identity.device_id && !d.revoked).collect();
                    },Ok(Err(binachicken_device::Error::Unauthorized))=>return Err("Reauthorization required".into()),_=>{}}
                }
            },
        }
        state.lock().unwrap().connection_requested = desired.is_some();
        if end {
            eprintln!(
                "binachicken-client: retiring remote session: {}",
                state.lock().unwrap().status
            );
            if let Some(l) = live.take() {
                // A close event may have been lost during socket rotation.
                // Recheck enrollment immediately when any live scope retires.
                next_list = Instant::now();
                let id = l.grant.session_id;
                drop(l);
                close_remote(&mut closing, api.clone(), &token_rx, id);
            }
            next_connect = Instant::now() + connect_backoff;
            let mut s = state.lock().unwrap();
            s.connected = false;
            s.ready = false;
            s.display = None;
            s.stats = PeerStats::default();
            s.session_id = None;
            s.connection = Some(binachicken_transport::ConnectionState::Closed);
            if s.status == "Connected" {
                s.status = "Disconnected".into();
            }
        }
    }
    drop(live);
    {
        let mut s = state.lock().unwrap();
        s.connected = false;
        s.ready = false;
        s.display = None;
    }
    network.abort();
    let _ = network.await;
    if let Some(refresh) = refresh {
        refresh.abort();
        let _ = refresh.await;
    }
    // Close requests share the worker lifetime; no ordinary HTTP work survives
    // into the explicit revocation attempt. JoinSet also cancels on error exits.
    closing.shutdown().await;
    if revoke.load(Ordering::Acquire) {
        let token = token_rx.borrow().clone();
        let confirmed = if let Some(token) = token {
            matches!(
                tokio::time::timeout(
                    Duration::from_secs(3),
                    api.revoke_device(&token, identity.device_id)
                )
                .await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        state.lock().unwrap().status = if confirmed {
            "Signed out; device registration revoked"
        } else {
            "Signed out locally; backend revocation was not confirmed"
        }
        .into();
    }
    Ok(())
}
fn close_remote(
    closing: &mut tokio::task::JoinSet<()>,
    api: Arc<DeviceApi>,
    tokens: &watch::Receiver<Option<AccessToken>>,
    id: SessionId,
) {
    if let Some(token) = tokens.borrow().clone() {
        closing.spawn(async move {
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
        let connect = async {
            let token = api.authenticate(&identity).await?;
            let socket = api.events(&token, DeviceRole::Client).await?;
            Ok::<_, binachicken_device::Error>((token, socket))
        };
        let result = tokio::select! {_=stop.cancelled()=>return,result=connect=>result};
        if matches!(result, Err(binachicken_device::Error::Unauthorized)) {
            let _ = events.try_send(NetworkEvent::Denied);
            return;
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
                        Err(binachicken_device::Error::Unauthorized)=>{let _=events.try_send(NetworkEvent::Denied);return},
                        Err(binachicken_device::Error::PolicyClosed)=>{
                            // A policy close also covers stale/invalid signaling, not only revocation.
                            // Retire remote input and let fresh signed authentication decide account validity.
                            if events.try_send(NetworkEvent::SessionRejected).is_err() {return;}
                            Err(())
                        },
                        _=>Err(())
                    },
                    command=commands.recv()=>match command {Some(command)=>sender.send(&command).await.map_err(|_|()),None=>return},
                };
                if result.is_err() {
                    break;
                }
            }
            let _ = sender.close().await;
        }
        tokens.send_replace(None);
        if events.try_send(NetworkEvent::Offline).is_err() {
            return;
        }
        while commands.try_recv().is_ok() {}
        tokio::select! {_=stop.cancelled()=>return,_=tokio::time::sleep(backoff)=>{}}
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

fn merge_presence(current: &mut Vec<DevicePresence>, devices: Vec<DevicePresence>) {
    for device in devices {
        current.retain(|p| p.device_id != device.device_id);
        current.push(device);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fresh_presence_merges_all_chunks_and_replaces_roles() {
        let id = |n| DeviceId::from_uuid(uuid::Uuid::from_u128(n));
        let online = |n, role| DevicePresence {
            device_id: id(n),
            role,
            online: true,
        };
        let mut presence = vec![online(1, DeviceRole::Agent)];
        presence.clear(); // Every Offline socket boundary, before any new chunks.
        merge_presence(&mut presence, vec![online(2, DeviceRole::Agent)]);
        merge_presence(&mut presence, vec![online(3, DeviceRole::Agent)]);
        assert!(!presence.iter().any(|p| p.device_id == id(1)));
        assert_eq!(presence.len(), 2);
        merge_presence(&mut presence, vec![online(2, DeviceRole::Client)]);
        assert_eq!(presence.len(), 2);
        assert!(
            !presence
                .iter()
                .any(|p| p.device_id == id(2) && p.role == DeviceRole::Agent)
        );
    }
}
