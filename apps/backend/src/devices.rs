use crate::{
    Error, Result,
    config::Config,
    db::{Database, hash, now, random},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ed25519_dalek::VerifyingKey;
use rand::RngCore;
use remotevnc_protocol::*;
use rusqlite::{OptionalExtension, Transaction, params};
use uuid::Uuid;

#[derive(Clone)]
pub struct DeviceService {
    config: Config,
    db: Database,
}
/// Verified device authority; revalidate on each HTTP operation and future lease renewal.
#[derive(Clone)]
pub struct DevicePrincipal {
    pub device_id: DeviceId,
    pub owner_subject: String,
}
impl DeviceService {
    pub fn new(config: Config, db: Database) -> Self {
        Self { config, db }
    }
    pub fn begin_enrollment(&self, request: EnrollmentRequest) -> Result<Enrollment> {
        if request.name.trim().is_empty()
            || request.name.len() > 128
            || request.name.chars().any(char::is_control)
        {
            return Err(Error::Invalid);
        }
        key(&request.public_key)?;
        let id = Uuid::new_v4().to_string();
        let expires = now() + 600_000;
        let challenge = new_challenge(
            DeviceId(Uuid::new_v4()),
            ChallengePurpose::RegisterDevice,
            expires as u64,
        );
        let polling_verifier = random();
        // 80 bits, entered explicitly in the browser. Never returned from a public-ID browser route.
        let display_code = random()[..16].to_ascii_uppercase();
        let mut browser = self
            .config
            .public_origin
            .join("/auth/google/start")
            .map_err(|_| Error::Internal)?;
        browser.query_pairs_mut().append_pair("enrollment", &id);
        self.db.transaction(|tx| {
            if tx
                .query_row(
                    "SELECT id FROM devices WHERE public_key=?",
                    [&request.public_key],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .is_some()
            {
                return Err(Error::Invalid);
            }
            tx.execute(
                "INSERT INTO
                 enrollments(id, device_id, name, public_key,
                 code_hash, verifier_hash, challenge, expires_at)
                 VALUES(?,?,?,?,?,?,?,?)",
                params![
                    id,
                    challenge.device_id.0.to_string(),
                    request.name,
                    request.public_key,
                    hash(&display_code),
                    hash(&polling_verifier),
                    json(&challenge)?,
                    expires
                ],
            )?;
            Ok(())
        })?;
        Ok(Enrollment {
            id,
            display_code,
            browser_url: browser.to_string(),
            polling_verifier,
            expires_at_unix_ms: expires as u64,
            challenge,
        })
    }
    pub fn poll_enrollment(&self, id: &str, poll: EnrollmentPoll) -> Result<EnrollmentStatus> {
        if id.len() > 64 || poll.polling_verifier.len() > 128 || poll.signature.len() != 64 {
            return Err(Error::Invalid);
        }
        self.db.transaction(|tx| {
            let row = tx
                .query_row(
                    "SELECT device_id,name,public_key,challenge,owner FROM enrollments WHERE id=?
                 AND verifier_hash=? AND expires_at>? AND consumed=0",
                    params![id, hash(&poll.polling_verifier), now()],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<String>>(4)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            let challenge: DeviceChallenge =
                serde_json::from_str(&row.3).map_err(|_| Error::Internal)?;
            verify_challenge(&key(&row.2)?, &challenge, &poll.signature, now() as u64)
                .map_err(|_| Error::Invalid)?;
            let Some(owner) = row.4 else {
                return Ok(EnrollmentStatus::Pending);
            };
            let active: i64 = tx.query_row(
                "SELECT COUNT(*) FROM devices WHERE owner=? AND revoked=0",
                [&owner],
                |r| r.get(0),
            )?;
            if active >= 1000 {
                return Err(Error::Limit);
            }
            tx.execute(
                "INSERT INTO devices(id,owner,name,public_key) VALUES(?,?,?,?)",
                params![row.0, owner, row.1, row.2],
            )?;
            tx.execute("UPDATE enrollments SET consumed=1 WHERE id=?", [id])?;
            Ok(EnrollmentStatus::Enrolled {
                device_id: challenge.device_id,
            })
        })
    }
    pub fn challenge(&self, id: DeviceId) -> Result<AuthenticationChallenge> {
        self.db.transaction(|tx| {
            tx.query_row(
                "SELECT id FROM devices WHERE id=? AND revoked=0",
                [id.0.to_string()],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(Error::Invalid)?;
            let challenge = new_challenge(
                id,
                ChallengePurpose::AuthenticateDevice,
                (now() + 60_000) as u64,
            );
            let challenge_id = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO device_challenges(id,device_id,challenge,expires_at)
                 VALUES(?,?,?,?)",
                params![
                    challenge_id,
                    id.0.to_string(),
                    json(&challenge)?,
                    challenge.expires_at_unix_ms as i64
                ],
            )?;
            Ok(AuthenticationChallenge {
                id: challenge_id,
                challenge,
            })
        })
    }
    pub fn authenticate(&self, proof: DeviceProof) -> Result<AccessToken> {
        if proof.challenge_id.len() > 64 || proof.signature.len() != 64 {
            return Err(Error::Invalid);
        }
        self.db.transaction(|tx| {
            let (device_id, challenge, public_key) = tx
                .query_row(
                    "SELECT c.device_id,c.challenge,d.public_key FROM device_challenges c JOIN
                 devices d ON c.device_id=d.id WHERE c.id=? AND c.consumed=0 AND
                 c.expires_at>? AND d.revoked=0",
                    params![proof.challenge_id, now()],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            let challenge: DeviceChallenge =
                serde_json::from_str(&challenge).map_err(|_| Error::Internal)?;
            verify_challenge(
                &key(&public_key)?,
                &challenge,
                &proof.signature,
                now() as u64,
            )
            .map_err(|_| Error::Invalid)?;
            tx.execute(
                "UPDATE device_challenges SET consumed=1 WHERE id=?",
                [proof.challenge_id],
            )?;
            let token = AccessToken {
                token: random(),
                expires_at_unix_ms: (now() + 300_000) as u64,
            };
            tx.execute(
                "INSERT INTO access_tokens(token_hash,device_id,expires_at) VALUES(?,?,?)",
                params![
                    hash(&token.token),
                    device_id,
                    token.expires_at_unix_ms as i64
                ],
            )?;
            Ok(token)
        })
    }
    pub fn principal(&self, token: &str) -> Result<DevicePrincipal> {
        self.db.transaction(|tx| principal(tx, token))
    }
    pub fn list(&self, token: &str) -> Result<Vec<DeviceSummary>> {
        self.db.transaction(|tx| {
            let principal = principal(tx, token)?;
            let mut statement = tx.prepare(
                "SELECT id,name,public_key,revoked FROM devices
                 WHERE owner=? AND revoked=0 ORDER BY id LIMIT 1001",
            )?;
            let rows = statement.query_map([principal.owner_subject], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                ))
            })?;
            let devices: Vec<_> = rows
                .map(|row| {
                    let (id, name, public_key, revoked) = row?;
                    Ok(DeviceSummary {
                        id: DeviceId(Uuid::parse_str(&id).map_err(|_| Error::Internal)?),
                        name,
                        public_key,
                        revoked,
                    })
                })
                .collect::<Result<_>>()?;
            if devices.len() > 1000 {
                return Err(Error::Limit);
            }
            Ok(devices)
        })
    }
    pub fn revoke(&self, token: &str, id: DeviceId) -> Result<()> {
        self.db.transaction(|tx| {
            let principal = principal(tx, token)?;
            if tx.execute(
                "UPDATE devices SET revoked=1 WHERE id=? AND owner=?",
                params![id.0.to_string(), principal.owner_subject],
            )? != 1
            {
                return Err(Error::Invalid);
            }
            Ok(())
        })
    }
}
fn principal(tx: &Transaction<'_>, token: &str) -> Result<DevicePrincipal> {
    if token.len() > 128 {
        return Err(Error::Invalid);
    }
    let (id, owner) = tx
        .query_row(
            "SELECT d.id,d.owner FROM access_tokens t JOIN devices d ON t.device_id=d.id
                 WHERE t.token_hash=? AND t.expires_at>? AND d.revoked=0",
            params![hash(token), now()],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or(Error::Invalid)?;
    Ok(DevicePrincipal {
        device_id: DeviceId(Uuid::parse_str(&id).map_err(|_| Error::Internal)?),
        owner_subject: owner,
    })
}
fn new_challenge(
    device_id: DeviceId,
    purpose: ChallengePurpose,
    expires_at_unix_ms: u64,
) -> DeviceChallenge {
    let mut nonce = [0; 32];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    DeviceChallenge {
        purpose,
        nonce,
        device_id,
        expires_at_unix_ms,
    }
}
fn key(public_key: &str) -> Result<VerifyingKey> {
    if public_key.len() != 43 {
        return Err(Error::Invalid);
    }
    let bytes: [u8; 32] = URL_SAFE_NO_PAD
        .decode(public_key)
        .map_err(|_| Error::Invalid)?
        .try_into()
        .map_err(|_| Error::Invalid)?;
    let key = VerifyingKey::from_bytes(&bytes).map_err(|_| Error::Invalid)?;
    if key.is_weak() {
        return Err(Error::Invalid);
    }
    Ok(key)
}
fn json(value: &impl serde::Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|_| Error::Internal)
}
