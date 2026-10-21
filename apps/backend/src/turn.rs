//! Cloudflare's generate-ice-servers contract. Provider secrets stay in this client.
use crate::{Error, Result};
use remotevnc_protocol::IceServer;
use serde::Deserialize;
use std::{
    collections::HashSet,
    fs::{self, File},
    io::Read,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

pub const TURN_TTL: Duration = Duration::from_secs(3600);
pub const TURN_REFRESH_BEFORE: Duration = Duration::from_secs(300);
const MAX_KEYS: usize = 16;
const MAX_CONFIG_BYTES: u64 = 128 * 1024;

// These types intentionally do not implement Debug: they contain provider secrets.
struct Provider {
    endpoint: Url,
    token: String,
}
struct Pool {
    providers: Vec<Provider>,
    http: reqwest::Client,
    next: AtomicUsize,
}
#[derive(Clone)]
pub struct TurnClient {
    pool: Option<Arc<Pool>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyFile {
    keys: Vec<Key>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Key {
    key_id: String,
    api_token: String,
}
fn valid_key_id(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn valid_token(token: &str) -> bool {
    !token.trim().is_empty() && token.len() <= 4096 && !token.chars().any(char::is_control)
}
impl TurnClient {
    /// Endpoint injection supports local HTTP fixtures; production uses `from_env`.
    pub fn new(endpoint: &str, token: String) -> Result<Self> {
        let endpoint = Url::parse(endpoint).map_err(|_| Error::Invalid)?;
        let loopback = match endpoint.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        };
        if !valid_token(&token)
            || (endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && loopback))
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.host().is_none()
        {
            return Err(Error::Invalid);
        }
        Self::configured(vec![Provider { endpoint, token }])
    }
    fn configured(providers: Vec<Provider>) -> Result<Self> {
        if providers.is_empty() || providers.len() > MAX_KEYS {
            return Err(Error::Invalid);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::Internal)?;
        Ok(Self {
            pool: Some(Arc::new(Pool {
                providers,
                http,
                next: AtomicUsize::new(0),
            })),
        })
    }
    fn from_keys(keys: Vec<Key>) -> Result<Self> {
        if keys.is_empty() || keys.len() > MAX_KEYS {
            return Err(Error::Invalid);
        }
        let mut ids = HashSet::new();
        let mut providers = Vec::new();
        for key in keys {
            if !valid_key_id(&key.key_id)
                || !valid_token(&key.api_token)
                || !ids.insert(key.key_id.clone())
            {
                return Err(Error::Invalid);
            }
            let endpoint = Url::parse(&format!(
                "https://rtc.live.cloudflare.com/v1/turn/keys/{}/credentials/generate-ice-servers",
                key.key_id
            ))
            .map_err(|_| Error::Invalid)?;
            providers.push(Provider {
                endpoint,
                token: key.api_token,
            });
        }
        Self::configured(providers)
    }
    fn from_file(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).map_err(|_| Error::Invalid)?;
        if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
            return Err(Error::Invalid);
        }
        let file = File::open(path).map_err(|_| Error::Invalid)?;
        let opened = file.metadata().map_err(|_| Error::Invalid)?;
        let current = fs::symlink_metadata(path).map_err(|_| Error::Invalid)?;
        if !opened.is_file() || !current.is_file() || opened.len() > MAX_CONFIG_BYTES {
            return Err(Error::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if opened.permissions().mode() & 0o077 != 0
                || metadata.dev() != opened.dev()
                || metadata.ino() != opened.ino()
                || current.dev() != opened.dev()
                || current.ino() != opened.ino()
            {
                return Err(Error::Invalid);
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| Error::Invalid)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(Error::Invalid);
        }
        let configuration: KeyFile = serde_json::from_slice(&bytes).map_err(|_| Error::Invalid)?;
        Self::from_keys(configuration.keys)
    }
    pub fn from_env() -> Result<Self> {
        if let Some(path) = std::env::var_os("REMOTEVNC_TURN_KEYS_FILE") {
            if [
                "REMOTEVNC_TURN_KEY_ID",
                "REMOTEVNC_TURN_API_TOKEN",
                "REMOTEVNC_TURN_API_TOKEN_FILE",
            ]
            .iter()
            .any(|name| std::env::var_os(name).is_some())
            {
                return Err(Error::Invalid);
            }
            return Self::from_file(Path::new(&path));
        }
        match (
            std::env::var("REMOTEVNC_TURN_KEY_ID"),
            std::env::var("REMOTEVNC_TURN_API_TOKEN"),
        ) {
            (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => {
                Ok(Self::unavailable())
            }
            (Ok(key_id), Ok(api_token)) => Self::from_keys(vec![Key { key_id, api_token }]),
            _ => Err(Error::Invalid),
        }
    }
    pub fn unavailable() -> Self {
        Self { pool: None }
    }
    pub fn is_configured(&self) -> bool {
        self.pool.is_some()
    }
    pub async fn issue(&self, ttl: Duration) -> Result<Vec<IceServer>> {
        if ttl != TURN_TTL {
            return Err(Error::Invalid);
        }
        let pool = self.pool.as_ref().ok_or(Error::TurnUnavailable)?;
        let issued = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Internal)?
            .as_secs();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let count = pool.providers.len();
        let start = pool.next.fetch_add(1, Ordering::Relaxed) % count;
        for offset in 0..count {
            let now = tokio::time::Instant::now();
            let remaining = deadline.saturating_duration_since(now);
            if remaining.is_zero() {
                break;
            }
            // Reserve a fair share of the single deadline for each remaining key.
            let attempt_deadline = now + remaining / (count - offset) as u32;
            let provider = &pool.providers[(start + offset) % count];
            if let Ok(Ok(servers)) = tokio::time::timeout_at(
                attempt_deadline,
                Self::issue_one(&pool.http, provider, ttl, issued),
            )
            .await
            {
                return Ok(servers);
            }
        }
        Err(Error::TurnUnavailable)
    }
    async fn issue_one(
        http: &reqwest::Client,
        provider: &Provider,
        ttl: Duration,
        issued: u64,
    ) -> Result<Vec<IceServer>> {
        let mut response = http
            .post(provider.endpoint.clone())
            .bearer_auth(&provider.token)
            .json(&serde_json::json!({"ttl":ttl.as_secs()}))
            .send()
            .await
            .map_err(|_| Error::TurnUnavailable)?;
        if response.status() != reqwest::StatusCode::CREATED {
            return Err(Error::TurnUnavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::TurnUnavailable)? {
            if bytes.len() + chunk.len() > 32 * 1024 {
                return Err(Error::TurnUnavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Response {
            #[serde(rename = "iceServers")]
            servers: Vec<Server>,
        }
        #[derive(Deserialize)]
        struct Server {
            urls: Vec<String>,
            username: Option<String>,
            credential: Option<String>,
        }
        let response: Response =
            serde_json::from_slice(&bytes).map_err(|_| Error::TurnUnavailable)?;
        if response.servers.is_empty() || response.servers.len() > 8 {
            return Err(Error::TurnUnavailable);
        }
        let mut stun_count = 0;
        let mut turn_count = 0;
        let mut servers = Vec::new();
        for server in response.servers {
            if server.urls.is_empty() || server.urls.len() > 16 {
                return Err(Error::TurnUnavailable);
            }
            for uri in &server.urls {
                if uri.len() > 2048
                    || uri.chars().any(char::is_whitespace)
                    || uri.chars().any(char::is_control)
                {
                    return Err(Error::TurnUnavailable);
                }
                let (scheme, address) = uri.split_once(':').ok_or(Error::TurnUnavailable)?;
                if !matches!(scheme, "stun" | "turn" | "turns") || address.starts_with("//") {
                    return Err(Error::TurnUnavailable);
                }
                let parsed = Url::parse(&format!("{scheme}://{address}"))
                    .map_err(|_| Error::TurnUnavailable)?;
                if parsed.host().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.fragment().is_some()
                    || !matches!(parsed.path(), "" | "/")
                    || parsed
                        .query()
                        .is_some_and(|q| q != "transport=udp" && q != "transport=tcp")
                {
                    return Err(Error::TurnUnavailable);
                }
                if scheme == "stun" {
                    stun_count += 1;
                } else {
                    turn_count += 1;
                    for secret in [&server.username, &server.credential] {
                        if !secret.as_ref().is_some_and(|s| {
                            !s.is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control)
                        }) {
                            return Err(Error::TurnUnavailable);
                        }
                    }
                }
            }
            servers.push(IceServer {
                urls: server.urls,
                username: server.username,
                credential: server.credential,
                expires_at_unix_secs: Some(issued + ttl.as_secs()),
            });
        }
        if stun_count > 1 || turn_count == 0 {
            return Err(Error::TurnUnavailable);
        }
        Ok(servers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::Path as RoutePath,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    use serde_json::json;
    use std::sync::Mutex;

    type Requests = Arc<Mutex<Vec<(String, String, serde_json::Value)>>>;
    struct Reply {
        status: StatusCode,
        body: String,
        delay: Duration,
    }
    fn good(index: usize) -> Reply {
        Reply {
            status: StatusCode::CREATED,
            body: json!({"iceServers":[{"urls":["stun:stun.cloudflare.com:3478"]},{"urls":[format!("turn:key{index}.test:3478")],"username":format!("user-{index}"),"credential":format!("credential-{index}")}]}).to_string(),
            delay: Duration::ZERO,
        }
    }
    async fn fixture(replies: Vec<Reply>) -> (TurnClient, Requests, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let providers = (0..replies.len())
            .map(|index| Provider {
                endpoint: Url::parse(&format!("http://{address}/keys/{index}")).unwrap(),
                token: format!("fixture-token-{index}"),
            })
            .collect();
        let requests: Requests = Arc::default();
        let recorded = requests.clone();
        let replies = Arc::new(replies);
        let router = Router::new().route(
            "/keys/{key}",
            post(
                move |RoutePath(key): RoutePath<usize>,
                      headers: HeaderMap,
                      axum::Json(body): axum::Json<serde_json::Value>| {
                    let replies = replies.clone();
                    let recorded = recorded.clone();
                    async move {
                        recorded.lock().unwrap().push((
                            key.to_string(),
                            headers["authorization"].to_str().unwrap().to_owned(),
                            body,
                        ));
                        let reply = &replies[key];
                        tokio::time::sleep(reply.delay).await;
                        (reply.status, reply.body.clone())
                    }
                },
            ),
        );
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (TurnClient::configured(providers).unwrap(), requests, task)
    }
    fn assert_requests(requests: &Requests, keys: &[usize]) {
        let actual = requests.lock().unwrap();
        assert_eq!(actual.len(), keys.len());
        for ((key, auth, body), index) in actual.iter().zip(keys) {
            assert_eq!(key, &index.to_string());
            assert_eq!(auth, &format!("Bearer fixture-token-{index}"));
            assert_eq!(body, &json!({"ttl":3600}));
        }
    }
    #[tokio::test]
    async fn round_robin_is_shared_by_clones_and_returns_one_complete_response() {
        let (client, requests, task) = fixture(vec![good(0), good(1), good(2)]).await;
        let clone = client.clone();
        for (client, index) in [(&client, 0), (&clone, 1), (&client, 2), (&clone, 0)] {
            let servers = client.issue(TURN_TTL).await.unwrap();
            assert_eq!(servers.len(), 2);
            assert_eq!(
                servers[1].username.as_deref(),
                Some(format!("user-{index}").as_str())
            );
            assert_eq!(servers[1].urls, [format!("turn:key{index}.test:3478")]);
        }
        assert_requests(&requests, &[0, 1, 2, 0]);
        task.abort();
    }
    #[tokio::test]
    async fn failed_status_malformed_and_oversized_responses_fail_over() {
        for first in [
            Reply {
                status: StatusCode::UNAUTHORIZED,
                ..good(0)
            },
            Reply {
                status: StatusCode::FOUND,
                ..good(0)
            },
            Reply {
                body: "{malformed".into(),
                ..good(0)
            },
            Reply {
                body: json!({"iceServers":[]}).to_string(),
                ..good(0)
            },
            Reply {
                body: "x".repeat(32769),
                ..good(0)
            },
        ] {
            let (client, requests, task) = fixture(vec![first, good(1)]).await;
            let servers = client.issue(TURN_TTL).await.unwrap();
            assert_eq!(servers.len(), 2);
            assert_eq!(servers[1].username.as_deref(), Some("user-1"));
            assert_requests(&requests, &[0, 1]);
            task.abort();
        }
    }
    #[tokio::test]
    async fn network_failure_fails_over_and_all_failed_keys_are_unavailable() {
        let (client, requests, task) = fixture(vec![good(0), good(1)]).await;
        let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused =
            Url::parse(&format!("http://{}/closed", unused.local_addr().unwrap())).unwrap();
        drop(unused);
        let pool = client.pool.as_ref().unwrap();
        let client = TurnClient::configured(vec![
            Provider {
                endpoint: refused,
                token: "refused-token".into(),
            },
            Provider {
                endpoint: pool.providers[1].endpoint.clone(),
                token: pool.providers[1].token.clone(),
            },
        ])
        .unwrap();
        assert_eq!(
            client.issue(TURN_TTL).await.unwrap()[1].username.as_deref(),
            Some("user-1")
        );
        assert_requests(&requests, &[1]);
        task.abort();
        let (client, requests, task) = fixture(vec![
            Reply {
                status: StatusCode::UNAUTHORIZED,
                ..good(0)
            },
            Reply {
                body: "invalid".into(),
                ..good(1)
            },
        ])
        .await;
        assert!(matches!(
            client.issue(TURN_TTL).await,
            Err(Error::TurnUnavailable)
        ));
        assert_requests(&requests, &[0, 1]);
        task.abort();
    }
    #[tokio::test]
    async fn slow_first_key_leaves_time_for_a_healthy_key() {
        let (client, requests, task) = fixture(vec![
            Reply {
                delay: Duration::from_secs(30),
                ..good(0)
            },
            good(1),
        ])
        .await;
        let start = std::time::Instant::now();
        assert_eq!(
            client.issue(TURN_TTL).await.unwrap()[1].username.as_deref(),
            Some("user-1")
        );
        assert!(start.elapsed() < Duration::from_millis(4500));
        assert_requests(&requests, &[0, 1]);
        task.abort();
    }
    #[tokio::test]
    async fn sixteen_slow_keys_share_one_five_second_budget() {
        let (client, requests, task) = fixture(
            (0..16)
                .map(|index| Reply {
                    delay: Duration::from_secs(30),
                    ..good(index)
                })
                .collect(),
        )
        .await;
        let start = std::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_millis(6500), client.issue(TURN_TTL)).await;
        assert!(matches!(result, Ok(Err(Error::TurnUnavailable))));
        assert!(start.elapsed() < Duration::from_millis(6500));
        assert_requests(&requests, &(0..16).collect::<Vec<_>>());
        task.abort();
    }
    #[tokio::test]
    async fn invalid_ttl_does_not_call_provider_or_advance_rotation() {
        let (client, requests, task) = fixture(vec![good(0), good(1)]).await;
        assert!(matches!(
            client.issue(Duration::from_secs(60)).await,
            Err(Error::Invalid)
        ));
        assert_requests(&requests, &[]);
        assert_eq!(
            client.issue(TURN_TTL).await.unwrap()[1].username.as_deref(),
            Some("user-0")
        );
        assert_requests(&requests, &[0]);
        task.abort();
    }
}
