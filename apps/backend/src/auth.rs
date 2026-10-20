use crate::{
    Error, Result,
    config::Config,
    db::{Database, hash, now, random},
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, HttpRequest, HttpResponse, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use rusqlite::{OptionalExtension, params};
use std::time::Duration;

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
#[derive(Clone)]
pub struct AuthService {
    config: Config,
    db: Database,
    metadata: CoreProviderMetadata,
}
/// Browser-only, one-use confirmation authority. Never logged or placed in a URL.
pub struct LoginCompletion {
    pub confirmation: String,
    pub owner_subject: String,
    pub device_name: String,
    pub public_key: String,
}
impl AuthService {
    /// Dependency injection for embedded callers/tests. Production uses `discover_google` exclusively.
    pub fn new(config: Config, db: Database, metadata: CoreProviderMetadata) -> Result<Self> {
        Ok(Self {
            config,
            db,
            metadata,
        })
    }
    pub async fn discover_google(config: Config, db: Database) -> Result<Self> {
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new("https://accounts.google.com".into()).map_err(|_| Error::Internal)?,
            &bounded_http,
        )
        .await
        .map_err(|_| Error::Internal)?;
        Self::new(config, db, metadata)
    }
    fn client(&self, metadata: CoreProviderMetadata) -> Result<Client> {
        Ok(CoreClient::from_provider_metadata(
            metadata,
            ClientId::new(self.config.google_client_id.clone()),
            Some(ClientSecret::new(self.config.google_client_secret.clone())),
        )
        .set_redirect_uri(
            RedirectUrl::new(self.config.redirect_uri.to_string()).map_err(|_| Error::Internal)?,
        ))
    }
    pub fn start_login(&self, enrollment_id: &str) -> Result<String> {
        if enrollment_id.len() > 64 {
            return Err(Error::Invalid);
        }
        let (pkce, verifier) = PkceCodeChallenge::new_random_sha256();
        let (url, state, nonce) = self
            .client(self.metadata.clone())?
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .set_pkce_challenge(pkce)
            .url();
        self.db.transaction(|tx| {
            let expiry = tx
                .query_row(
                    "SELECT expires_at FROM enrollments WHERE id=? AND expires_at>? AND
                 consumed=0 AND owner IS NULL",
                    params![enrollment_id, now()],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            tx.execute(
                "INSERT INTO
                 oauth_transactions(state_hash,enrollment_id,nonce,pkce,expires_at)
                 VALUES(?,?,?,?,?)",
                params![
                    hash(state.secret()),
                    enrollment_id,
                    nonce.secret(),
                    verifier.secret(),
                    expiry
                ],
            )?;
            Ok(())
        })?;
        Ok(url.to_string())
    }
    pub async fn finish_login(&self, code: &str, state: &str) -> Result<LoginCompletion> {
        if code.is_empty() || code.len() > 4096 || state.len() > 128 {
            return Err(Error::Invalid);
        }
        // Consume before any network request, across processes and restarts, including failed exchanges.
        let (enrollment, nonce, pkce, expires) = self
            .db
            .transaction(|tx| {
                let row = tx
                    .query_row(
                        "DELETE FROM oauth_transactions WHERE state_hash=? RETURNING
                 enrollment_id,nonce,pkce,expires_at",
                        [hash(state)],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .optional()?;
                Ok(row)
            })?
            .ok_or(Error::Invalid)?;
        if expires <= now() {
            return Err(Error::Invalid);
        }
        // Refresh discovery/JWKS per login; a long-running process accepts Google's rotated keys.
        let metadata =
            CoreProviderMetadata::discover_async(self.metadata.issuer().clone(), &bounded_http)
                .await
                .map_err(|_| Error::Invalid)?;
        let client = self.client(metadata)?;
        let response = client
            .exchange_code(AuthorizationCode::new(code.to_owned()))
            .map_err(|_| Error::Invalid)?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce))
            .request_async(&bounded_http)
            .await
            .map_err(|_| Error::Invalid)?;
        let token = response.id_token().ok_or(Error::Invalid)?;
        let verifier = client.id_token_verifier();
        let claims = token
            .claims(&verifier, &Nonce::new(nonce))
            .map_err(|_| Error::Invalid)?;
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                response.access_token(),
                token.signing_alg().map_err(|_| Error::Invalid)?,
                token.signing_key(&verifier).map_err(|_| Error::Invalid)?,
            )
            .map_err(|_| Error::Invalid)?;
            if actual != *expected {
                return Err(Error::Invalid);
            }
        }
        let subject = claims.subject().as_str();
        if subject.is_empty() || subject.len() > 255 {
            return Err(Error::Invalid);
        }
        let confirmation = random();
        let (device_name, public_key) = self.db.transaction(|tx| {
            let row = tx
                .query_row(
                    "SELECT name,public_key FROM enrollments WHERE id=? AND expires_at>? AND
                 consumed=0 AND owner IS NULL",
                    params![enrollment, now()],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                )
                .optional()?
                .ok_or(Error::Invalid)?;
            tx.execute(
                "INSERT INTO confirmations(token_hash,enrollment_id,subject,expires_at)
                 VALUES(?,?,?,?)",
                params![hash(&confirmation), enrollment, subject, expires],
            )?;
            Ok(row)
        })?;
        Ok(LoginCompletion {
            confirmation,
            owner_subject: subject.to_owned(),
            device_name,
            public_key,
        })
    }
    pub fn confirm(&self, confirmation: &str, display_code: &str) -> Result<()> {
        if confirmation.len() > 128 || display_code.len() > 64 {
            return Err(Error::Invalid);
        }
        // Commit deletion even on mismatch; brute-force attempts require a fresh Google exchange.
        let accepted = self.db.transaction(|tx| {
            let row = tx
                .query_row(
                    "DELETE FROM confirmations WHERE token_hash=? RETURNING
                 enrollment_id,subject,expires_at",
                    [hash(confirmation)],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, subject, expiry)) = row else {
                return Ok(false);
            };
            if expiry <= now() {
                return Ok(false);
            }
            let valid = tx
                .query_row(
                    "SELECT id FROM enrollments WHERE id=? AND code_hash=? AND consumed=0 AND
                 owner IS NULL AND expires_at>? AND NOT EXISTS(SELECT 1 FROM devices d WHERE
                 d.public_key=enrollments.public_key)",
                    params![id, hash(display_code), now()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .is_some();
            if !valid {
                return Ok(false);
            }
            tx.execute("INSERT OR IGNORE INTO users(subject) VALUES(?)", [&subject])?;
            tx.execute(
                "UPDATE enrollments SET owner=? WHERE id=?",
                params![subject, id],
            )?;
            Ok(true)
        })?;
        if accepted {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}
/// The standard OIDC client uses this bounded transport for discovery, JWKS and token exchange.
async fn bounded_http(request: HttpRequest) -> Result<HttpResponse> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| Error::Internal)?;
    let mut response = client
        .execute(request.try_into().map_err(|_| Error::Internal)?)
        .await
        .map_err(|_| Error::Internal)?;
    let mut builder = axum::http::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Internal)? {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err(Error::Internal);
        }
        body.extend_from_slice(&chunk);
    }
    builder.body(body).map_err(|_| Error::Internal)
}
