//! Cloudflare's generate-ice-servers contract. Provider secrets stay in this client.
use crate::{Error, Result};
use remotevnc_protocol::IceServer;
use serde::Deserialize;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

pub const TURN_TTL: Duration = Duration::from_secs(3600);
pub const TURN_REFRESH_BEFORE: Duration = Duration::from_secs(300);
#[derive(Clone)]
pub struct TurnClient {
    provider: Option<(Url, String, reqwest::Client)>,
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
        if token.is_empty()
            || token.len() > 4096
            || token.chars().any(char::is_control)
            || (endpoint.scheme() != "https" && !(endpoint.scheme() == "http" && loopback))
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || endpoint.host().is_none()
        {
            return Err(Error::Invalid);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| Error::Internal)?;
        Ok(Self {
            provider: Some((endpoint, token, http)),
        })
    }
    pub fn from_env() -> Result<Self> {
        match (
            std::env::var("REMOTEVNC_TURN_KEY_ID"),
            std::env::var("REMOTEVNC_TURN_API_TOKEN"),
        ) {
            (Err(std::env::VarError::NotPresent), Err(std::env::VarError::NotPresent)) => {
                Ok(Self::unavailable())
            }
            (Ok(key), Ok(token))
                if !key.is_empty()
                    && key.len() <= 128
                    && key
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
            {
                Self::new(
                    &format!(
                        "https://rtc.live.cloudflare.com/v1/turn/keys/{key}/credentials/generate-ice-servers"
                    ),
                    token,
                )
            }
            _ => Err(Error::Invalid),
        }
    }
    pub fn unavailable() -> Self {
        Self { provider: None }
    }
    pub fn is_configured(&self) -> bool {
        self.provider.is_some()
    }
    pub async fn issue(&self, ttl: Duration) -> Result<Vec<IceServer>> {
        let (endpoint, token, http) = self.provider.as_ref().ok_or(Error::TurnUnavailable)?;
        if ttl != TURN_TTL {
            return Err(Error::Invalid);
        }
        let issued = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Internal)?
            .as_secs();
        let mut response = http
            .post(endpoint.clone())
            .bearer_auth(token)
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
