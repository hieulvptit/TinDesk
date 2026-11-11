#![allow(dead_code)]
use axum::{
    Json, Router,
    extract::{Form, State},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use openidconnect::{IssuerUrl, core::CoreProviderMetadata};
use rand::rngs::OsRng;
use remotevnc_backend::{auth::AuthService, config::Config, db::Database};
use rsa::{RsaPrivateKey, pkcs1::EncodeRsaPrivateKey, traits::PublicKeyParts};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy)]
pub enum Fault {
    None,
    Signature,
    Audience,
    Issuer,
    Nonce,
    Expired,
}
#[derive(Clone)]
struct Grant {
    nonce: String,
    challenge: String,
    sub: String,
    fault: Fault,
}
#[derive(Clone)]
struct IssuerState {
    origin: String,
    grants: Arc<Mutex<HashMap<String, Grant>>>,
    rotated: Arc<AtomicBool>,
}
struct FixtureKeys {
    primary: RsaPrivateKey,
    rotated: RsaPrivateKey,
    wrong_signature: RsaPrivateKey,
}
fn fixture_keys() -> &'static FixtureKeys {
    static KEYS: OnceLock<FixtureKeys> = OnceLock::new();
    KEYS.get_or_init(|| FixtureKeys {
        primary: RsaPrivateKey::new(&mut OsRng, 2048).unwrap(),
        rotated: RsaPrivateKey::new(&mut OsRng, 2048).unwrap(),
        wrong_signature: RsaPrivateKey::new(&mut OsRng, 2048).unwrap(),
    })
}
pub struct Issuer {
    state: IssuerState,
    pub metadata: CoreProviderMetadata,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Issuer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Issuer {
    pub async fn start() -> Self {
        // RSA generation can exceed the real client's request deadline on slower CPUs.
        // Finish it before serving requests; handlers only sign with prepared test keys.
        tokio::task::spawn_blocking(fixture_keys).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let state = IssuerState {
            origin: format!("http://{}", listener.local_addr().unwrap()),
            grants: Default::default(),
            rotated: Default::default(),
        };
        let app = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/jwks", get(jwks))
            .route("/token", post(token))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let metadata = CoreProviderMetadata::discover_async(
            IssuerUrl::new(state.origin.clone()).unwrap(),
            &http,
        )
        .await
        .unwrap();
        Self {
            state,
            metadata,
            task,
        }
    }
    pub fn issue(&self, authorization_url: &str, sub: &str, fault: Fault) -> (String, String) {
        let url = url::Url::parse(authorization_url).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["response_type"], "code");
        let code = uuid::Uuid::new_v4().to_string();
        self.state.grants.lock().unwrap().insert(
            code.clone(),
            Grant {
                nonce: params["nonce"].clone(),
                challenge: params["code_challenge"].clone(),
                sub: sub.into(),
                fault,
            },
        );
        (code, params["state"].clone())
    }
    pub fn rotate_keys(&self) {
        self.state.rotated.store(true, Ordering::SeqCst);
    }
    pub fn auth(&self, db: Database) -> AuthService {
        AuthService::new(config(), db, self.metadata.clone()).unwrap()
    }
}
pub fn config() -> Config {
    Config::new(
        "https://backend.example",
        Some("https://backend.example/login/callback"),
        "test-client".into(),
        "test-secret".into(),
    )
    .unwrap()
}
async fn discovery(State(s): State<IssuerState>) -> Json<Value> {
    Json(
        json!({"issuer":s.origin,"authorization_endpoint":format!("{}/authorize",s.origin),"token_endpoint":format!("{}/token",s.origin),"jwks_uri":format!("{}/jwks",s.origin),"response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]}),
    )
}
async fn jwks(State(s): State<IssuerState>) -> Json<Value> {
    let rotated = s.rotated.load(Ordering::SeqCst);
    let keys = fixture_keys();
    let key = if rotated {
        &keys.rotated
    } else {
        &keys.primary
    };
    let kid = if rotated { "rotated-key" } else { "test-key" };
    Json(
        json!({"keys":[{"kty":"RSA","kid":kid,"use":"sig","alg":"RS256","n":URL_SAFE_NO_PAD.encode(key.n().to_bytes_be()),"e":URL_SAFE_NO_PAD.encode(key.e().to_bytes_be())}]}),
    )
}
async fn token(
    State(s): State<IssuerState>,
    Form(form): Form<HashMap<String, String>>,
) -> Result<Json<Value>, axum::http::StatusCode> {
    let grant = s
        .grants
        .lock()
        .unwrap()
        .remove(
            form.get("code")
                .ok_or(axum::http::StatusCode::BAD_REQUEST)?,
        )
        .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
    let verifier = form
        .get("code_verifier")
        .ok_or(axum::http::StatusCode::BAD_REQUEST)?;
    if URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)) != grant.challenge {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut claims = json!({"iss":s.origin,"sub":grant.sub,"aud":"test-client","exp":now+300,"iat":now,"nonce":grant.nonce,"email":"shared@example.test","email_verified":true});
    match grant.fault {
        Fault::Audience => claims["aud"] = json!("other-client"),
        Fault::Issuer => claims["iss"] = json!("https://other-issuer.test"),
        Fault::Nonce => claims["nonce"] = json!("wrong-nonce"),
        Fault::Expired => claims["exp"] = json!(now - 300),
        _ => {}
    }
    let rotated = s.rotated.load(Ordering::SeqCst);
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(if rotated { "rotated-key" } else { "test-key" }.into());
    let keys = fixture_keys();
    let signing = if matches!(grant.fault, Fault::Signature) {
        &keys.wrong_signature
    } else if rotated {
        &keys.rotated
    } else {
        &keys.primary
    };
    // jsonwebtoken without PEM support accepts DER, keeping fixture signing local.
    let der = signing.to_pkcs1_der().unwrap();
    let jwt =
        jsonwebtoken::encode(&header, &claims, &EncodingKey::from_rsa_der(der.as_bytes())).unwrap();
    Ok(Json(
        json!({"access_token":"fixture-access-token","token_type":"Bearer","expires_in":300,"id_token":jwt}),
    ))
}

pub fn device_key(byte: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[byte; 32])
}
pub fn begin(
    service: &remotevnc_backend::devices::DeviceService,
    key: &ed25519_dalek::SigningKey,
) -> binachicken_protocol::Enrollment {
    service
        .begin_enrollment(binachicken_protocol::EnrollmentRequest {
            name: "Office <agent>".into(),
            public_key: URL_SAFE_NO_PAD.encode(key.verifying_key().to_bytes()),
        })
        .unwrap()
}
pub fn poll(
    enrollment: &binachicken_protocol::Enrollment,
    key: &ed25519_dalek::SigningKey,
) -> binachicken_protocol::EnrollmentPoll {
    binachicken_protocol::EnrollmentPoll {
        polling_verifier: enrollment.polling_verifier.clone(),
        signature: binachicken_protocol::sign_challenge(key, &enrollment.challenge).unwrap(),
    }
}
pub async fn confirm(
    issuer: &Issuer,
    auth: &AuthService,
    e: &binachicken_protocol::Enrollment,
    sub: &str,
) {
    let (code, state) = issuer.issue(&auth.start_login(&e.id).unwrap(), sub, Fault::None);
    let login = auth.finish_login(&code, &state).await.unwrap();
    auth.confirm(&login.confirmation, &e.display_code).unwrap();
}
pub fn access(
    service: &remotevnc_backend::devices::DeviceService,
    id: binachicken_protocol::DeviceId,
    key: &ed25519_dalek::SigningKey,
) -> binachicken_protocol::AccessToken {
    let c = service.challenge(id).unwrap();
    service
        .authenticate(binachicken_protocol::DeviceProof {
            challenge_id: c.id,
            signature: binachicken_protocol::sign_challenge(key, &c.challenge).unwrap(),
        })
        .unwrap()
}

pub fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    dir
}

pub struct SessionFixture {
    pub directory: tempfile::TempDir,
    pub db: Database,
    pub devices: remotevnc_backend::devices::DeviceService,
    pub ids: [binachicken_protocol::DeviceId; 4],
    pub tokens: [String; 4],
}
impl SessionFixture {
    pub fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("sessions.sqlite")
    }
}
pub fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
pub fn session_fixture() -> SessionFixture {
    let directory = private_dir();
    let path = directory.path().join("sessions.sqlite");
    let db = Database::open(&path).unwrap();
    let c = rusqlite::Connection::open(&path).unwrap();
    c.execute("INSERT INTO users(subject) VALUES('owner'),('other')", [])
        .unwrap();
    let ids = std::array::from_fn(|_| binachicken_protocol::DeviceId(uuid::Uuid::new_v4()));
    let tokens = std::array::from_fn(|_| uuid::Uuid::new_v4().to_string());
    for index in 0..4 {
        c.execute(
            "INSERT INTO devices(id,owner,name,public_key) VALUES(?,?,?,?)",
            rusqlite::params![
                ids[index].0.to_string(),
                if index == 2 { "other" } else { "owner" },
                format!("device-{index}"),
                URL_SAFE_NO_PAD.encode(device_key(index as u8 + 1).verifying_key().to_bytes())
            ],
        )
        .unwrap();
        c.execute(
            "INSERT INTO access_tokens(token_hash,device_id,expires_at) VALUES(?,?,?)",
            rusqlite::params![
                URL_SAFE_NO_PAD.encode(Sha256::digest(tokens[index].as_bytes())),
                ids[index].0.to_string(),
                unix_ms() as i64 + 300_000
            ],
        )
        .unwrap();
    }
    SessionFixture {
        devices: remotevnc_backend::devices::DeviceService::new(config(), db.clone()),
        db,
        directory,
        ids,
        tokens,
    }
}
