//! One bounded FIFO per device; final authorization and write share the revoke gate.
use crate::{
    BackendState, Error, Result, bearer,
    db::{authorization_gate, now},
    devices,
    sessions::{SessionService, active_owner, authorized},
};
use axum::{
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{CloseFrame, Message, WebSocket, close_code},
    },
    http::HeaderMap,
    response::Response,
};
use binachicken_protocol::*;
use futures_util::{Sink, SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

pub const OUTPUT_QUEUE_CAPACITY: usize = 32;
pub const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_CONNECTIONS: usize = 4096;
const RATE_MESSAGES: usize = 120;
const RATE_WINDOW: Duration = Duration::from_secs(10);
#[derive(Clone, Default)]
pub(crate) struct Hub(Arc<Mutex<HashMap<DeviceId, Connection>>>);
struct Connection {
    id: Uuid,
    owner: String,
    role: DeviceRole,
    output: mpsc::Sender<Queued>,
    stop: watch::Sender<bool>,
}
pub(crate) struct Queued {
    event: DeviceEvent,
    source_token: Option<String>,
}
struct Subscription {
    hub: Hub,
    device: DeviceId,
    id: Uuid,
    output: mpsc::Receiver<Queued>,
    stop: watch::Receiver<bool>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        // Upgrade failure/cancellation and normal loop exit share identity-scoped cleanup.
        self.hub.remove(self.device, self.id);
    }
}

impl Hub {
    pub(crate) fn enqueue(
        &self,
        device: DeviceId,
        event: DeviceEvent,
        source_token: Option<String>,
        required: bool,
    ) -> Result<()> {
        encode_control(&event).map_err(|_| Error::Invalid)?;
        let peers = self.0.lock().map_err(|_| Error::Internal)?;
        let Some(peer) = peers.get(&device) else {
            return if required {
                Err(Error::Invalid)
            } else {
                Ok(())
            };
        };
        if peer
            .output
            .try_send(Queued {
                event,
                source_token,
            })
            .is_err()
        {
            let _ = peer.stop.send(true);
            return Err(Error::Limit);
        }
        Ok(())
    }
    pub(crate) fn require_connected(&self, device: DeviceId) -> Result<()> {
        if self
            .0
            .lock()
            .map_err(|_| Error::Internal)?
            .get(&device)
            .is_some_and(|peer| !*peer.stop.borrow())
        {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    fn subscribe(
        &self,
        principal: &devices::DevicePrincipal,
        role: DeviceRole,
    ) -> Result<Subscription> {
        let mut peers = self.0.lock().map_err(|_| Error::Internal)?;
        if peers.contains_key(&principal.device_id) || peers.len() >= MAX_CONNECTIONS {
            return Err(Error::Limit);
        }
        let (output, rx) = mpsc::channel(OUTPUT_QUEUE_CAPACITY);
        let (stop, cancel) = watch::channel(false);
        let id = Uuid::new_v4();
        peers.insert(
            principal.device_id,
            Connection {
                id,
                owner: principal.owner_subject.clone(),
                role,
                output,
                stop,
            },
        );
        Ok(Subscription {
            hub: self.clone(),
            device: principal.device_id,
            id,
            output: rx,
            stop: cancel,
        })
    }
    fn remove(&self, device: DeviceId, id: Uuid) {
        if let Ok(mut peers) = self.0.lock()
            && peers.get(&device).is_some_and(|p| p.id == id)
        {
            peers.remove(&device);
        }
    }
    fn presence(&self, owner: &str) -> Result<Vec<DevicePresence>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| Error::Internal)?
            .iter()
            .filter(|(_, p)| p.owner == owner)
            .map(|(id, p)| DevicePresence {
                device_id: *id,
                role: p.role,
                online: true,
            })
            .collect())
    }
    fn announce(
        &self,
        principal: &devices::DevicePrincipal,
        role: DeviceRole,
        online: bool,
    ) -> Result<()> {
        for peer in self.presence(&principal.owner_subject)? {
            let _ = self.enqueue(
                peer.device_id,
                DeviceEvent::Presence {
                    devices: vec![DevicePresence {
                        device_id: principal.device_id,
                        role,
                        online,
                    }],
                },
                None,
                false,
            );
        }
        Ok(())
    }
}
#[derive(Deserialize)]
pub struct EventsQuery {
    role: DeviceRole,
}
pub async fn events(
    State(state): State<BackendState>,
    headers: HeaderMap,
    Query(query): Query<EventsQuery>,
    ws: WebSocketUpgrade,
) -> Result<Response> {
    let token = bearer(&headers)?.to_owned();
    let service = state.sessions;
    let _gate = authorization_gate().lock().await;
    let (principal, sub) = service.prepare_connection(&token, query.role)?;
    let cleanup = service.hub.clone();
    let device = principal.device_id;
    let id = sub.id;
    Ok(ws
        .max_message_size(CONTROL_PAYLOAD_LIMIT)
        .max_frame_size(CONTROL_PAYLOAD_LIMIT)
        .on_failed_upgrade(move |_| cleanup.remove(device, id))
        .on_upgrade(move |socket| run(service, token, principal, query.role, sub, socket)))
}
impl SessionService {
    // Caller holds authorization_gate across preparation/publication.
    fn prepare_connection(
        &self,
        token: &str,
        role: DeviceRole,
    ) -> Result<(devices::DevicePrincipal, Subscription)> {
        let principal = self.db.transaction(|tx| devices::principal(tx, token))?;
        let sub = self.hub.subscribe(&principal, role)?;
        // The shared gate prevents every live publisher from using this registration
        // until its bounded initial queue has been completely built.
        self.initial_events(token, principal.device_id)?;
        Ok((principal, sub))
    }
    fn activate_connection(
        &self,
        token: &str,
        principal: &devices::DevicePrincipal,
        role: DeviceRole,
    ) -> Result<()> {
        self.db.transaction(|tx| devices::principal(tx, token))?;
        self.hub.announce(principal, role, true)
    }

    pub async fn presence_authenticated(&self, token: &str) -> Result<Vec<DevicePresence>> {
        let _gate = authorization_gate().lock().await;
        self.db.transaction(|tx| {
            let principal = devices::principal(tx, token)?;
            Ok(self
                .hub
                .presence(&principal.owner_subject)?
                .into_iter()
                .filter(|p| {
                    active_owner(tx, p.device_id)
                        .is_ok_and(|owner| owner == principal.owner_subject)
                })
                .collect())
        })
    }
    fn initial_events(&self, token: &str, device: DeviceId) -> Result<()> {
        self.db.transaction(|tx| {
            let principal = devices::principal(tx,token)?;
            let presence:Vec<_> = self.hub.presence(&principal.owner_subject)?.into_iter().filter(|p|active_owner(tx,p.device_id).is_ok()).collect();
            for devices in presence.chunks(256) { self.hub.enqueue(device,DeviceEvent::Presence {devices:devices.to_vec()},None,true)?; }
            let mut statement = tx.prepare("SELECT id,refresh_generation,refresh_pending FROM sessions WHERE closed=0 AND ready=1 AND (client_id=? OR agent_id=?) AND lease_expires_at>? LIMIT 1001")?;
            let rows = statement.query_map(rusqlite::params![device.0.to_string(),device.0.to_string(),now()], |r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?;
            let mut grants = Vec::new();
            for row in rows {
                let (id,generation,pending) = row?;
                let session = SessionId(Uuid::parse_str(&id).map_err(|_|Error::Internal)?);
                if let Ok(grant) = authorized(tx,device,session) { grants.push((grant,generation,pending)); }
            }
            self.hub.enqueue(device,DeviceEvent::SessionSnapshot {session_ids:grants.iter().map(|(g,_,_)|g.session_id).collect()},None,true)?;
            for (grant,generation,pending) in grants {
                self.hub.enqueue(device,DeviceEvent::Session {grant:grant.clone()},None,true)?;
                if generation > 0 && pending != 1 {
                    if pending == 2 {
                        let column = if device==grant.agent_id { "agent_installed" } else { "client_installed" };
                        tx.execute(&format!("UPDATE sessions SET {column}=0 WHERE id=?"),[grant.session_id.0.to_string()])?;
                    }
                    self.hub.enqueue(device,DeviceEvent::IceConfiguration {session_id:grant.session_id,generation:generation as u64,requires_ack:pending==2,ice_servers:grant.ice_servers.clone()},None,true)?;
                    if pending==0 && device==grant.agent_id {
                        self.hub.enqueue(device,DeviceEvent::IceRestartReady {session_id:grant.session_id,generation:generation as u64},None,true)?;
                    }
                }
            }
            Ok(())
        })
    }
    fn handle(&self, token: &str, command: DeviceCommand) -> Result<()> {
        match command {
            DeviceCommand::Signal { receiver, message } => self.db.transaction(|tx| {
                let principal = devices::principal(tx, token)?;
                validate_signal(tx, principal.device_id, receiver, &message)?;
                if matches!(&message, SignalMessage::Restart { .. }) {
                    let pending: bool = tx.query_row(
                        "SELECT refresh_pending != 0 FROM sessions WHERE id=?",
                        [signal_session(&message).0.to_string()],
                        |r| r.get(0),
                    )?;
                    if pending {
                        return Err(Error::Invalid);
                    }
                }
                self.hub.enqueue(
                    receiver,
                    DeviceEvent::Signal {
                        sender: principal.device_id,
                        message,
                    },
                    Some(token.to_owned()),
                    true,
                )
            }),
            DeviceCommand::IceInstalled {
                session_id,
                generation,
            } => self.installed(token, session_id, generation),
        }
    }
    fn authorize_output(&self, token: &str, queued: &mut Queued) -> Result<Duration> {
        self.db.transaction(|tx| {
            let principal = devices::principal(tx, token)?;
            let token_expiry: i64 = tx.query_row(
                "SELECT expires_at FROM access_tokens WHERE token_hash=?",
                [crate::db::hash(token)],
                |r| r.get(0),
            )?;
            let session = match &mut queued.event {
                DeviceEvent::Presence { devices } => {
                    for peer in devices {
                        // Offline notifications may refer to a just-revoked device, still same owner.
                        let owner: String = tx.query_row(
                            "SELECT owner FROM devices WHERE id=?",
                            [peer.device_id.0.to_string()],
                            |r| r.get(0),
                        )?;
                        if owner != principal.owner_subject {
                            return Err(Error::Invalid);
                        }
                        if peer.online {
                            active_owner(tx, peer.device_id)?;
                        }
                    }
                    None
                }
                DeviceEvent::SessionSnapshot { session_ids } => {
                    // Reconcile against current authority even if the queued grant
                    // expired or was revoked before the first socket write.
                    let mut active = Vec::with_capacity(session_ids.len());
                    for session in session_ids.drain(..) {
                        match authorized(tx, principal.device_id, session) {
                            Ok(_) => active.push(session),
                            Err(Error::Invalid) => {},
                            Err(error) => return Err(error),
                        }
                    }
                    *session_ids = active;
                    None
                },
                DeviceEvent::SessionClosed { session_id } => {
                    let owned:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE id=? AND owner_subject=? AND (client_id=? OR agent_id=?))",rusqlite::params![session_id.0.to_string(),principal.owner_subject,principal.device_id.0.to_string(),principal.device_id.0.to_string()],|r|r.get(0))?;
                    if !owned || authorized(tx,principal.device_id,*session_id).is_ok() {return Err(Error::Invalid);}
                    None
                },
                DeviceEvent::Session { grant } => Some(grant.session_id),
                DeviceEvent::Signal { sender, message } => {
                    let source = devices::principal(
                        tx,
                        queued.source_token.as_deref().ok_or(Error::Invalid)?,
                    )?;
                    if source.device_id != *sender {
                        return Err(Error::Invalid);
                    }
                    validate_signal(tx, *sender, principal.device_id, message)?;
                    Some(signal_session(message))
                }
                DeviceEvent::IceConfiguration { session_id, .. }
                | DeviceEvent::IceRestartReady { session_id, .. } => Some(*session_id),
            };
            let deadline = if let Some(session) = session {
                (authorized(tx, principal.device_id, session)?.lease_expires_at as i64)
                    .min(token_expiry)
            } else {
                token_expiry
            };
            let remaining = deadline - now();
            if remaining <= 0 {
                return Err(Error::Invalid);
            }
            Ok(WRITE_TIMEOUT.min(Duration::from_millis(remaining as u64)))
        })
    }
    /// The gate remains held through the bounded send, but never a database transaction.
    async fn write<S>(&self, sink: &mut S, token: &str, mut queued: Queued) -> Result<DeviceEvent>
    where
        S: Sink<Message> + Unpin,
    {
        let _gate = authorization_gate().lock().await;
        let deadline = self.authorize_output(token, &mut queued)?;
        let bytes = encode_control(&queued.event).map_err(|_| Error::Invalid)?;
        let text = String::from_utf8(bytes).map_err(|_| Error::Internal)?;
        tokio::time::timeout(deadline, sink.send(Message::Text(text.into())))
            .await
            .map_err(|_| Error::Internal)?
            .map_err(|_| Error::Internal)?;
        Ok(queued.event)
    }
    fn check_known_session(&self, device: DeviceId, known: &mut Option<SessionId>) -> Result<()> {
        if let Some(session) = *known
            && self
                .db
                .transaction(|tx| authorized(tx, device, session))
                .is_err()
        {
            self.hub.enqueue(
                device,
                DeviceEvent::SessionClosed {
                    session_id: session,
                },
                None,
                true,
            )?;
            *known = None;
        }
        Ok(())
    }
}
fn remember_sent_session(event: &DeviceEvent, known: &mut Option<SessionId>) {
    match event {
        DeviceEvent::SessionSnapshot { session_ids } => *known = session_ids.first().copied(),
        DeviceEvent::Session { grant } => *known = Some(grant.session_id),
        DeviceEvent::SessionClosed { session_id } if *known == Some(*session_id) => *known = None,
        _ => {}
    }
}
fn signal_session(message: &SignalMessage) -> SessionId {
    match message {
        SignalMessage::Display { session_id, .. }
        | SignalMessage::DisplayUnavailable { session_id, .. }
        | SignalMessage::Restart { session_id, .. }
        | SignalMessage::Offer { session_id, .. }
        | SignalMessage::Answer { session_id, .. }
        | SignalMessage::IceCandidate { session_id, .. }
        | SignalMessage::End { session_id, .. } => *session_id,
    }
}
fn validate_signal(
    tx: &rusqlite::Transaction<'_>,
    sender: DeviceId,
    receiver: DeviceId,
    message: &SignalMessage,
) -> Result<()> {
    let grant = authorized(tx, sender, signal_session(message))?;
    let expected = if sender == grant.agent_id {
        grant.client_id
    } else {
        grant.agent_id
    };
    if receiver != expected {
        return Err(Error::Invalid);
    }
    if matches!(
        message,
        SignalMessage::Display { .. } | SignalMessage::DisplayUnavailable { .. }
    ) && sender != grant.agent_id
    {
        return Err(Error::Invalid);
    }
    if let SignalMessage::Display { display, .. } = message {
        display.validate().map_err(|_| Error::Invalid)?;
    }
    Ok(())
}
async fn run(
    service: SessionService,
    token: String,
    principal: devices::DevicePrincipal,
    role: DeviceRole,
    mut sub: Subscription,
    socket: WebSocket,
) {
    let (mut sink, mut stream) = socket.split();
    let setup = {
        let _gate = authorization_gate().lock().await;
        service.activate_connection(&token, &principal, role)
    };
    let mut policy_close = setup.is_err();
    let mut known_session = None;
    if setup.is_ok() {
        let mut window = Instant::now();
        let mut count = 0;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                biased;
                _ = sub.stop.changed() => break,
                _ = heartbeat.tick() => {
                    let _gate = authorization_gate().lock().await;
                    if service.db.transaction(|tx| devices::principal(tx,&token)).is_err() { policy_close=true;break; }
                    if service.check_known_session(principal.device_id,&mut known_session).is_err() {break;}
                }
                message = stream.next() => {
                    let Some(Ok(message)) = message else { break; };
                    if window.elapsed() >= RATE_WINDOW { window=Instant::now();count=0; }
                    count += 1;
                    if count > RATE_MESSAGES { policy_close=true;break; }
                    let _gate = authorization_gate().lock().await;
                    if service.db.transaction(|tx| devices::principal(tx,&token)).is_err() { policy_close=true;break; }
                    let command = match message {
                        Message::Text(text) => decode_control(text.as_bytes()),
                        Message::Binary(bytes) => decode_control(&bytes),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        Message::Close(_) => break,
                    };
                    if command.map_err(|_|Error::Invalid).and_then(|c|service.handle(&token,c)).is_err() { policy_close=true;break; }
                }
                queued = sub.output.recv() => {
                    let Some(queued) = queued else { break; };
                    match service.write(&mut sink,&token,queued).await {
                        Ok(event) => remember_sent_session(&event,&mut known_session),
                        Err(Error::Invalid) => {
                            // A queued session payload can become unauthorized before delivery.
                            // Discard it; heartbeat emits SessionClosed to a still-authorized peer.
                            if service.db.transaction(|tx| devices::principal(tx,&token)).is_err() {policy_close=true;break;}
                        }
                        Err(_) => break,
                    }
                }
            }
        }
    }
    let _gate = authorization_gate().lock().await;
    service.hub.remove(principal.device_id, sub.id);
    let _ = service.hub.announce(&principal, role, false);
    if policy_close {
        // This fixed control frame carries no session payload. Never flush after a failed send.
        let _ = tokio::time::timeout(
            WRITE_TIMEOUT,
            sink.send(Message::Close(Some(CloseFrame {
                code: close_code::POLICY,
                reason: "authorization or policy failure".into(),
            }))),
        )
        .await;
    }
    // Drop socket after failed/timed-out writes; no further flush of buffered payloads.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Poll},
    };
    struct PausedSink {
        writes: Arc<AtomicUsize>,
        ready: Arc<AtomicBool>,
        waker: Arc<futures_util::task::AtomicWaker>,
        started: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl Sink<Message> for PausedSink {
        type Error = ();
        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(mut self: Pin<&mut Self>, _: Message) -> std::result::Result<(), ()> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            if let Some(s) = self.started.take() {
                let _ = s.send(());
            }
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            self.waker.register(cx.waker());
            if self.ready.load(Ordering::SeqCst) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }
        fn poll_close(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            self.poll_flush(cx)
        }
    }
    fn fixture() -> (
        tempfile::TempDir,
        rusqlite::Connection,
        devices::DeviceService,
        SessionService,
        [DeviceId; 2],
    ) {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = dir.path().join("race.sqlite");
        let db = crate::db::Database::open(&path).unwrap();
        let sql = rusqlite::Connection::open(path).unwrap();
        sql.execute("INSERT INTO users(subject) VALUES('owner')", [])
            .unwrap();
        let ids = [DeviceId(Uuid::new_v4()), DeviceId(Uuid::new_v4())];
        for (index, id) in ids.iter().enumerate() {
            sql.execute(
                "INSERT INTO devices(id,owner,name,public_key) VALUES(?,'owner','device',?)",
                rusqlite::params![id.0.to_string(), format!("key-{index}")],
            )
            .unwrap();
            sql.execute(
                "INSERT INTO access_tokens(token_hash,device_id,expires_at) VALUES(?,?,?)",
                rusqlite::params![
                    crate::db::hash(&format!("token-{index}")),
                    id.0.to_string(),
                    now() + 300_000
                ],
            )
            .unwrap();
        }
        let config = crate::config::Config::new(
            "https://backend.example",
            None,
            "id".into(),
            "secret".into(),
        )
        .unwrap();
        let devices = crate::devices::DeviceService::new(config, db.clone());
        let service = SessionService::new(db, crate::turn::TurnClient::unavailable());
        (dir, sql, devices, service, ids)
    }
    #[tokio::test]
    async fn revoke_waits_for_inflight_write_and_rechecks_already_queued_payload() {
        let (_dir, sql, devices, service, ids) = fixture();
        let grant = service
            .create_authenticated("token-0", ids[1])
            .await
            .unwrap();
        let payload = || Queued {
            event: DeviceEvent::Signal {
                sender: ids[1],
                message: SignalMessage::Offer {
                    session_id: grant.session_id,
                    epoch: 1,
                    sdp: "queued-before-revoke".into(),
                },
            },
            source_token: Some("token-1".into()),
        };
        let writes = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicBool::new(false));
        let waker = Arc::new(futures_util::task::AtomicWaker::new());
        let (started, wait) = tokio::sync::oneshot::channel();
        let mut sink = PausedSink {
            writes: writes.clone(),
            ready: ready.clone(),
            waker: waker.clone(),
            started: Some(started),
        };
        let sending = service.clone();
        let queued = payload();
        let writer = tokio::spawn(async move {
            let result = sending.write(&mut sink, "token-0", queued).await;
            (sink, result)
        });
        wait.await.unwrap();
        let revoker = tokio::spawn(async move { devices.revoke("token-0", ids[1]).await });
        tokio::task::yield_now().await;
        assert!(!revoker.is_finished());
        let revoked: bool = sql
            .query_row(
                "SELECT revoked FROM devices WHERE id=?",
                [ids[1].0.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!revoked);
        ready.store(true, Ordering::SeqCst);
        waker.wake();
        let (mut sink, result) = writer.await.unwrap();
        result.unwrap();
        revoker.await.unwrap().unwrap();
        assert!(
            service
                .write(&mut sink, "token-0", payload())
                .await
                .is_err()
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        ready.store(false, Ordering::SeqCst);
        let timed_out = service
            .write(
                &mut sink,
                "token-0",
                Queued {
                    event: DeviceEvent::Presence { devices: vec![] },
                    source_token: None,
                },
            )
            .await;
        assert!(
            matches!(timed_out, Err(Error::Internal)),
            "a failed send must end the socket, never continue flushing"
        );
    }
    #[test]
    fn bounded_fifo_overflow_cancels_connection_without_dropping_in_order_messages() {
        let hub = Hub::default();
        let principal = devices::DevicePrincipal {
            device_id: DeviceId(Uuid::new_v4()),
            owner_subject: "owner".into(),
        };
        let mut sub = hub.subscribe(&principal, DeviceRole::Agent).unwrap();
        for generation in 0..OUTPUT_QUEUE_CAPACITY {
            hub.enqueue(
                principal.device_id,
                DeviceEvent::IceRestartReady {
                    session_id: SessionId(Uuid::nil()),
                    generation: generation as u64,
                },
                None,
                true,
            )
            .unwrap();
        }
        assert!(
            hub.enqueue(
                principal.device_id,
                DeviceEvent::Presence { devices: vec![] },
                None,
                true
            )
            .is_err()
        );
        assert!(*sub.stop.borrow());
        for generation in 0..OUTPUT_QUEUE_CAPACITY {
            match sub.output.try_recv().unwrap().event {
                DeviceEvent::IceRestartReady { generation: g, .. } => {
                    assert_eq!(g, generation as u64)
                }
                _ => panic!("wrong event"),
            }
        }
        assert!(hub.require_connected(principal.device_id).is_err());
    }

    #[derive(Default)]
    struct RecordingSink(Vec<Message>);
    impl Sink<Message> for RecordingSink {
        type Error = ();
        fn poll_ready(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
        fn start_send(mut self: Pin<&mut Self>, message: Message) -> std::result::Result<(), ()> {
            self.0.push(message);
            Ok(())
        }
        fn poll_flush(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
        ) -> Poll<std::result::Result<(), ()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn upgrade_callback_delay_preserves_snapshot_order_and_one_pending_configuration() {
        let (_dir, _sql, _devices, initial, ids) = fixture();
        let grant = initial
            .create_authenticated("token-0", ids[1])
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/turn", listener.local_addr().unwrap());
        let provider = tokio::spawn(async move {
            axum::serve(listener,axum::Router::new().route("/turn",axum::routing::post(||async {
                (axum::http::StatusCode::CREATED,axum::Json(serde_json::json!({"iceServers":[{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"fixture","credential":"fixture"}]})))
            }))).await.unwrap();
        });
        let service = SessionService::new(
            initial.db.clone(),
            crate::turn::TurnClient::new(&endpoint, "fixture-token".into()).unwrap(),
        );
        let (_agent, _agent_sub, client, mut client_sub) = {
            let _gate = authorization_gate().lock().await;
            let (agent, agent_sub) = service
                .prepare_connection("token-1", DeviceRole::Agent)
                .unwrap();
            service
                .activate_connection("token-1", &agent, DeviceRole::Agent)
                .unwrap();
            let (client, client_sub) = service
                .prepare_connection("token-0", DeviceRole::Client)
                .unwrap();
            (agent, agent_sub, client, client_sub)
        };
        // Deliberately withhold the upgrade callback while real publishers complete.
        service
            .refresh_turn_authenticated("token-1", grant.session_id)
            .await
            .unwrap();
        service
            .renew_authenticated("token-1", grant.session_id)
            .await
            .unwrap();
        {
            let _gate = authorization_gate().lock().await;
            service
                .handle(
                    "token-1",
                    DeviceCommand::Signal {
                        receiver: ids[0],
                        message: SignalMessage::Offer {
                            session_id: grant.session_id,
                            epoch: 1,
                            sdp: "during-upgrade".into(),
                        },
                    },
                )
                .unwrap();
            service
                .activate_connection("token-0", &client, DeviceRole::Client)
                .unwrap();
        }
        let mut events = Vec::new();
        while let Ok(queued) = client_sub.output.try_recv() {
            events.push(queued.event);
        }
        let configurations = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    DeviceEvent::IceConfiguration {
                        requires_ack: true,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(
            configurations, 1,
            "one installation must never provoke a duplicate ACK"
        );
        let snapshot = events
            .iter()
            .position(|event| matches!(event, DeviceEvent::SessionSnapshot { .. }))
            .unwrap();
        assert!(
            events[..snapshot]
                .iter()
                .all(|event| matches!(event, DeviceEvent::Presence { .. })),
            "snapshot must precede all live session/config/signal events"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DeviceEvent::Signal { .. }))
        );
        let _gate = authorization_gate().lock().await;
        service
            .handle(
                "token-0",
                DeviceCommand::IceInstalled {
                    session_id: grant.session_id,
                    generation: 1,
                },
            )
            .unwrap();
        service
            .handle(
                "token-1",
                DeviceCommand::IceInstalled {
                    session_id: grant.session_id,
                    generation: 1,
                },
            )
            .unwrap();
        provider.abort();
    }

    async fn queued_snapshot_reconciles(invalidate_by_revoke: bool) {
        let (_dir, sql, devices, service, ids) = fixture();
        let grant = service
            .create_authenticated("token-0", ids[1])
            .await
            .unwrap();
        let mut sub = {
            let _gate = authorization_gate().lock().await;
            let (principal, sub) = service
                .prepare_connection("token-0", DeviceRole::Client)
                .unwrap();
            service
                .activate_connection("token-0", &principal, DeviceRole::Client)
                .unwrap();
            sub
        };
        let snapshot = loop {
            let queued = sub.output.try_recv().unwrap();
            if let DeviceEvent::SessionSnapshot { session_ids } = &queued.event {
                assert_eq!(session_ids, &vec![grant.session_id]);
                break queued;
            }
        };
        if invalidate_by_revoke {
            devices.revoke("token-0", ids[1]).await.unwrap();
        } else {
            let _gate = authorization_gate().lock().await;
            sql.execute("UPDATE sessions SET lease_expires_at=0", [])
                .unwrap();
        }
        let mut sink = RecordingSink::default();
        service.write(&mut sink,"token-0",snapshot).await.expect("a surviving peer must receive reconciliation even when its queued grant became invalid");
        assert_eq!(sink.0.len(), 1);
        match &sink.0[0] {
            Message::Text(text) => match decode_control::<DeviceEvent>(text.as_bytes()).unwrap() {
                DeviceEvent::SessionSnapshot { session_ids } => assert!(session_ids.is_empty()),
                _ => panic!("expected authoritative snapshot"),
            },
            _ => panic!("expected control text"),
        }
    }
    #[tokio::test]
    async fn queued_snapshot_then_revoke_delivers_empty_reconciliation() {
        queued_snapshot_reconciles(true).await;
    }
    #[tokio::test]
    async fn queued_snapshot_then_expiry_delivers_empty_reconciliation() {
        queued_snapshot_reconciles(false).await;
    }

    #[tokio::test]
    async fn cancelled_upgrade_removes_registration_and_old_cleanup_preserves_replacement() {
        let (_dir, _sql, _devices, service, _ids) = fixture();
        let _gate = authorization_gate().lock().await;
        let (principal, sub) = service
            .prepare_connection("token-0", DeviceRole::Client)
            .unwrap();
        drop(sub); // The HTTP/upgrade task can be cancelled before its callback starts.
        assert!(service.hub.require_connected(principal.device_id).is_err());
        let (_, old) = service
            .prepare_connection("token-0", DeviceRole::Client)
            .unwrap();
        service.hub.remove(principal.device_id, old.id);
        let (_, replacement) = service
            .prepare_connection("token-0", DeviceRole::Client)
            .unwrap();
        drop(old);
        assert!(service.hub.require_connected(principal.device_id).is_ok());
        drop(replacement);
        assert!(service.hub.require_connected(principal.device_id).is_err());
    }

    #[tokio::test]
    async fn revoke_after_snapshot_before_grant_still_emits_session_closed() {
        let (_dir, _sql, devices, service, ids) = fixture();
        let grant = service
            .create_authenticated("token-0", ids[1])
            .await
            .unwrap();
        let mut sub = {
            let _gate = authorization_gate().lock().await;
            let (principal, sub) = service
                .prepare_connection("token-0", DeviceRole::Client)
                .unwrap();
            service
                .activate_connection("token-0", &principal, DeviceRole::Client)
                .unwrap();
            sub
        };
        let mut snapshot = None;
        let mut queued_grant = None;
        while let Ok(queued) = sub.output.try_recv() {
            match queued.event {
                DeviceEvent::SessionSnapshot { .. } => snapshot = Some(queued),
                DeviceEvent::Session { .. } => queued_grant = Some(queued),
                _ => {}
            }
        }
        let mut sink = RecordingSink::default();
        let sent = service
            .write(&mut sink, "token-0", snapshot.unwrap())
            .await
            .unwrap();
        let mut known = None;
        remember_sent_session(&sent, &mut known);
        assert_eq!(known, Some(grant.session_id));
        devices.revoke("token-0", ids[1]).await.unwrap();
        assert!(
            service
                .write(&mut sink, "token-0", queued_grant.unwrap())
                .await
                .is_err()
        );
        {
            let _gate = authorization_gate().lock().await;
            service.check_known_session(ids[0], &mut known).unwrap();
        }
        let closed = sub.output.try_recv().unwrap();
        let sent = service.write(&mut sink, "token-0", closed).await.unwrap();
        assert!(
            matches!(sent,DeviceEvent::SessionClosed {session_id} if session_id==grant.session_id)
        );
        remember_sent_session(&sent, &mut known);
        assert!(known.is_none());
        let replacement = SessionId(Uuid::new_v4());
        known = Some(replacement);
        remember_sent_session(&sent, &mut known);
        assert_eq!(
            known,
            Some(replacement),
            "an old close cannot erase a newer observed session"
        );
    }
}
