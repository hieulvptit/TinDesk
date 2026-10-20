//! Persistent session ownership and short renewable authorization leases.
use crate::{
    Error, Result,
    db::{Database, authorization_gate, now},
    devices,
    signaling::Hub,
    turn::{TURN_REFRESH_BEFORE, TURN_TTL, TurnClient},
};
use remotevnc_protocol::*;
use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

#[derive(Clone)]
pub struct SessionService {
    pub(crate) db: Database,
    turn: TurnClient,
    pub(crate) hub: Hub,
}
#[derive(Clone, Copy)]
enum Caller<'a> {
    Device(DeviceId),
    Token(&'a str),
}
impl Caller<'_> {
    fn resolve(self, tx: &Transaction<'_>) -> Result<devices::DevicePrincipal> {
        match self {
            Self::Token(token) => devices::principal(tx, token),
            Self::Device(device_id) => {
                let owner_subject = active_owner(tx, device_id)?;
                Ok(devices::DevicePrincipal {
                    device_id,
                    owner_subject,
                })
            }
        }
    }
}
pub(crate) fn active_owner(tx: &Transaction<'_>, device: DeviceId) -> Result<String> {
    tx.query_row(
        "SELECT owner FROM devices WHERE id=? AND revoked=0",
        [device.0.to_string()],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(Error::Invalid)
}
pub(crate) fn authorized(
    tx: &Transaction<'_>,
    caller: DeviceId,
    session: SessionId,
) -> Result<SessionGrant> {
    let owner = active_owner(tx, caller)?;
    let (client, agent, expiry, ice) = tx.query_row(
        "SELECT s.client_id,s.agent_id,s.lease_expires_at,s.ice_servers FROM sessions s JOIN devices c ON c.id=s.client_id JOIN devices a ON a.id=s.agent_id WHERE s.id=? AND s.owner_subject=? AND c.owner=s.owner_subject AND a.owner=s.owner_subject AND c.revoked=0 AND a.revoked=0 AND s.closed=0 AND s.ready=1 AND s.lease_expires_at>? AND (s.client_id=? OR s.agent_id=?) AND (s.refresh_pending=0 OR s.refresh_started_at>?)",
        params![session.0.to_string(), owner, now(), caller.0.to_string(), caller.0.to_string(), now()-30_000],
        |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,i64>(2)?, r.get::<_,String>(3)?))
    ).optional()?.ok_or(Error::Invalid)?;
    let ice_servers: Vec<IceServer> = serde_json::from_str(&ice).map_err(|_| Error::Internal)?;
    Ok(SessionGrant {
        session_id: session,
        client_id: DeviceId(Uuid::parse_str(&client).map_err(|_| Error::Internal)?),
        agent_id: DeviceId(Uuid::parse_str(&agent).map_err(|_| Error::Internal)?),
        lease_expires_at: expiry as u64,
        turn_status: if ice_servers.iter().any(|s| {
            s.expires_at_unix_secs
                .is_some_and(|expiry| expiry > now() as u64 / 1000)
        }) {
            TurnStatus::Available
        } else {
            TurnStatus::Unavailable
        },
        ice_servers,
    })
}
impl SessionService {
    /// Construct once and clone for routes. This does not invalidate running grants.
    pub fn new(db: Database, turn: TurnClient) -> Self {
        Self {
            db,
            turn,
            hub: Hub::default(),
        }
    }
    /// Call once at actual backend boot, before accepting connections.
    pub async fn expire_on_startup(&self) -> Result<()> {
        let _gate = authorization_gate().lock().await;
        self.db.transaction(|tx| {
            tx.execute("UPDATE sessions SET closed=1", [])?;
            Ok(())
        })
    }
    /// Trusted in-process entry point; HTTP must use the token-authenticated variant.
    pub async fn create(&self, caller: DeviceId, agent: DeviceId) -> Result<SessionGrant> {
        self.create_for(Caller::Device(caller), agent).await
    }
    pub async fn create_authenticated(&self, token: &str, agent: DeviceId) -> Result<SessionGrant> {
        self.create_for(Caller::Token(token), agent).await
    }
    async fn create_for(&self, caller: Caller<'_>, agent: DeviceId) -> Result<SessionGrant> {
        let id = SessionId(Uuid::new_v4());
        {
            let _gate = authorization_gate().lock().await;
            self.db.transaction(|tx| {
                let principal = caller.resolve(tx)?;
                if principal.device_id == agent || active_owner(tx, agent)? != principal.owner_subject { return Err(Error::Invalid); }
                tx.execute("UPDATE sessions SET closed=1 WHERE closed=0 AND (lease_expires_at<=? OR EXISTS(SELECT 1 FROM devices d WHERE (d.id=sessions.client_id OR d.id=sessions.agent_id) AND d.revoked=1))", [now()])?;
                let busy = |device: DeviceId| -> Result<bool> { Ok(tx.query_row("SELECT EXISTS(SELECT 1 FROM sessions WHERE (agent_id=? OR client_id=?) AND closed=0)", params![device.0.to_string(),device.0.to_string()], |r|r.get(0))?) };
                if busy(principal.device_id)? { return Err(Error::CallerBusy); }
                if busy(agent)? { return Err(Error::AgentBusy); }
                tx.execute("INSERT INTO sessions(id,owner_subject,client_id,agent_id,lease_expires_at) VALUES(?,?,?,?,?)", params![id.0.to_string(), principal.owner_subject, principal.device_id.0.to_string(), agent.0.to_string(), now()+60_000])?;
                Ok(())
            })?;
        }
        // A failed/unconfigured provider still allows direct connectivity, explicitly unavailable.
        let ice = self.turn.issue(TURN_TTL).await.unwrap_or_default();
        let _gate = authorization_gate().lock().await;
        let grant = self.db.transaction(|tx| {
            let principal = caller.resolve(tx)?;
            if active_owner(tx, agent)? != principal.owner_subject { return Err(Error::Invalid); }
            if tx.execute("UPDATE sessions SET ready=1,ice_servers=?,turn_expires_at=? WHERE id=? AND closed=0 AND lease_expires_at>?", params![serde_json::to_string(&ice).map_err(|_|Error::Internal)?, ice.first().and_then(|s|s.expires_at_unix_secs).unwrap_or(0) as i64, id.0.to_string(), now()])? != 1 { return Err(Error::Invalid); }
            authorized(tx, principal.device_id, id)
        })?;
        self.publish_grant(&grant)?;
        Ok(grant)
    }
    pub async fn renew(&self, caller: DeviceId, session: SessionId) -> Result<SessionGrant> {
        self.renew_for(Caller::Device(caller), session).await
    }
    pub async fn renew_authenticated(
        &self,
        token: &str,
        session: SessionId,
    ) -> Result<SessionGrant> {
        self.renew_for(Caller::Token(token), session).await
    }
    async fn renew_for(&self, caller: Caller<'_>, session: SessionId) -> Result<SessionGrant> {
        let _gate = authorization_gate().lock().await;
        let grant = self.db.transaction(|tx| {
            let principal = caller.resolve(tx)?;
            let grant = authorized(tx, principal.device_id, session)?;
            if principal.device_id != grant.agent_id {
                return Err(Error::Invalid);
            }
            tx.execute(
                "UPDATE sessions SET lease_expires_at=? WHERE id=?",
                params![now() + 60_000, session.0.to_string()],
            )?;
            authorized(tx, principal.device_id, session)
        })?;
        self.publish_grant(&grant)?;
        Ok(grant)
    }
    pub async fn close(&self, caller: DeviceId, session: SessionId) -> Result<()> {
        self.close_for(Caller::Device(caller), session).await
    }
    pub async fn close_authenticated(&self, token: &str, session: SessionId) -> Result<()> {
        self.close_for(Caller::Token(token), session).await
    }
    async fn close_for(&self, caller: Caller<'_>, session: SessionId) -> Result<()> {
        let _gate = authorization_gate().lock().await;
        let grant = self.db.transaction(|tx| {
            let principal = caller.resolve(tx)?;
            let grant = authorized(tx, principal.device_id, session)?;
            tx.execute(
                "UPDATE sessions SET closed=1 WHERE id=?",
                [session.0.to_string()],
            )?;
            Ok(grant)
        })?;
        for device in [grant.client_id, grant.agent_id] {
            self.hub.enqueue(
                device,
                DeviceEvent::SessionClosed {
                    session_id: session,
                },
                None,
                false,
            )?;
        }
        Ok(())
    }

    fn publish_grant(&self, grant: &SessionGrant) -> Result<()> {
        for device in [grant.client_id, grant.agent_id] {
            self.hub.enqueue(
                device,
                DeviceEvent::Session {
                    grant: grant.clone(),
                },
                None,
                false,
            )?;
        }
        Ok(())
    }
    /// Agent requests at credential expiry minus 300s, independently of 15s lease renewals.
    pub async fn refresh_turn_authenticated(&self, token: &str, session: SessionId) -> Result<()> {
        let generation = {
            let _gate = authorization_gate().lock().await;
            self.db.transaction(|tx| {
                let principal = devices::principal(tx, token)?;
                let grant = authorized(tx, principal.device_id, session)?;
                if principal.device_id != grant.agent_id { return Err(Error::Invalid); }
                self.hub.require_connected(grant.client_id)?;
                self.hub.require_connected(grant.agent_id)?;
                if !self.turn.is_configured() { return Err(Error::TurnUnavailable); }
                let (expiry, generation, pending): (i64,i64,bool) = tx.query_row("SELECT turn_expires_at,refresh_generation,refresh_pending FROM sessions WHERE id=?", [session.0.to_string()], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                if pending || expiry > now()/1000 + TURN_REFRESH_BEFORE.as_secs() as i64 { return Err(Error::Limit); }
                tx.execute("UPDATE sessions SET refresh_pending=1,refresh_started_at=?,refresh_generation=refresh_generation+1,client_installed=0,agent_installed=0 WHERE id=?", params![now(), session.0.to_string()])?;
                Ok(generation as u64 + 1)
            })?
        };
        let issued = self.turn.issue(TURN_TTL).await;
        let _gate = authorization_gate().lock().await;
        let ice = match issued {
            Ok(ice) => ice,
            Err(error) => {
                self.db.transaction(|tx| {
                    tx.execute(
                        "UPDATE sessions SET refresh_pending=0,refresh_generation=refresh_generation-1 WHERE id=? AND refresh_generation=? AND refresh_pending=1",
                        params![session.0.to_string(), generation as i64],
                    )?;
                    Ok(())
                })?;
                return Err(error);
            }
        };
        let grant = self.db.transaction(|tx| {
            let principal = devices::principal(tx, token)?;
            let grant = authorized(tx, principal.device_id, session)?;
            if principal.device_id != grant.agent_id { return Err(Error::Invalid); }
            tx.execute("UPDATE sessions SET ice_servers=?,turn_expires_at=?,refresh_pending=2 WHERE id=? AND refresh_generation=? AND refresh_pending=1", params![serde_json::to_string(&ice).map_err(|_|Error::Internal)?, ice[0].expires_at_unix_secs.unwrap_or(0) as i64, session.0.to_string(), generation as i64])?;
            Ok(grant)
        })?;
        for device in [grant.client_id, grant.agent_id] {
            self.hub.enqueue(
                device,
                DeviceEvent::IceConfiguration {
                    session_id: session,
                    generation,
                    requires_ack: true,
                    ice_servers: ice.clone(),
                },
                None,
                true,
            )?;
        }
        Ok(())
    }
    pub(crate) fn installed(&self, token: &str, session: SessionId, generation: u64) -> Result<()> {
        let ready = self.db.transaction(|tx| {
            let principal = devices::principal(tx, token)?;
            let grant = authorized(tx, principal.device_id, session)?;
            let column = if principal.device_id == grant.agent_id { "agent_installed" } else { "client_installed" };
            // Ignore no ACKs: stale/replayed/unknown generations are protocol errors.
            if tx.execute(&format!("UPDATE sessions SET {column}=1 WHERE id=? AND refresh_generation=? AND refresh_pending=2 AND {column}=0"), params![session.0.to_string(), i64::try_from(generation).map_err(|_|Error::Invalid)?])? != 1 { return Err(Error::Invalid); }
            let ready: bool = tx.query_row("SELECT client_installed=1 AND agent_installed=1 FROM sessions WHERE id=?", [session.0.to_string()], |r|r.get(0))?;
            if ready { tx.execute("UPDATE sessions SET refresh_pending=0 WHERE id=?", [session.0.to_string()])?; }
            Ok(if ready { Some(grant.agent_id) } else { None })
        })?;
        if let Some(agent) = ready {
            self.hub.enqueue(
                agent,
                DeviceEvent::IceRestartReady {
                    session_id: session,
                    generation,
                },
                None,
                true,
            )?;
        }
        Ok(())
    }
}
