use crate::{DeviceIdentity, Error, Result};
use ed25519_dalek::SigningKey;
use remotevnc_protocol::*;
use serde::de::DeserializeOwned;
use std::time::Duration;
use url::Url;

pub struct DeviceApi {
    pub(crate) base: Url,
    pub(crate) http: reqwest::Client,
    pub(crate) key: SigningKey,
}
impl DeviceApi {
    /// HTTPS is mandatory except explicit loopback HTTP for local development/test servers.
    pub fn new(base: &str, key: SigningKey) -> Result<Self> {
        let base = Url::parse(base).map_err(|_| Error::Invalid)?;
        let loopback = match base.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            _ => false,
        };
        if (base.scheme() != "https" && !(base.scheme() == "http" && loopback))
            || !base.username().is_empty()
            || base.password().is_some()
            || base.path() != "/"
            || base.query().is_some()
            || base.fragment().is_some()
            || base.host_str().is_none()
        {
            return Err(Error::Invalid);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Error::Request)?;
        Ok(Self { base, http, key })
    }
    pub(crate) fn url(&self, path: &str) -> Result<Url> {
        self.base.join(path).map_err(|_| Error::Invalid)
    }
    pub async fn authenticate(&self, identity: &DeviceIdentity) -> Result<AccessToken> {
        let challenge: AuthenticationChallenge = decode(
            self.http
                .post(self.url(&format!("/v1/devices/{}/challenge", identity.device_id.0))?)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await?;
        if challenge.challenge.purpose != ChallengePurpose::AuthenticateDevice
            || challenge.challenge.device_id != identity.device_id
        {
            return Err(Error::Invalid);
        }
        let proof = DeviceProof {
            challenge_id: challenge.id,
            signature: sign_challenge(&identity.signing_key, &challenge.challenge)
                .map_err(|_| Error::Invalid)?,
        };
        decode(
            self.http
                .post(self.url("/v1/devices/authenticate")?)
                .json(&proof)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await
    }
    pub async fn list_devices(&self, token: &AccessToken) -> Result<Vec<DeviceSummary>> {
        decode(
            self.http
                .get(self.url("/v1/devices")?)
                .bearer_auth(&token.token)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await
    }
    pub async fn revoke_device(&self, token: &AccessToken, id: DeviceId) -> Result<()> {
        let response = self
            .http
            .delete(self.url(&format!("/v1/devices/{}", id.0))?)
            .bearer_auth(&token.token)
            .send()
            .await
            .map_err(|_| Error::Request)?;
        no_content(response)
    }
}
pub(crate) async fn decode<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    check_status(&response)?;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Request)? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(Error::Request);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Request)
}

impl DeviceApi {
    pub async fn create_session(
        &self,
        token: &AccessToken,
        agent_id: DeviceId,
    ) -> Result<SessionGrant> {
        decode(
            self.http
                .post(self.url("/v1/sessions")?)
                .bearer_auth(&token.token)
                .json(&CreateSession { agent_id })
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await
    }
    /// Agent renews every 15s; the returned absolute deadline remains authoritative.
    pub async fn renew_session(
        &self,
        token: &AccessToken,
        session: SessionId,
    ) -> Result<SessionGrant> {
        decode(
            self.http
                .post(self.url(&format!("/v1/sessions/{}/renew", session.0))?)
                .bearer_auth(&token.token)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await
    }
    pub async fn close_session(&self, token: &AccessToken, session: SessionId) -> Result<()> {
        no_content(
            self.http
                .delete(self.url(&format!("/v1/sessions/{}", session.0))?)
                .bearer_auth(&token.token)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
    }
    /// Agent only. Request at the earliest ICE expiry minus 300 seconds.
    /// Install the resulting IceConfiguration without restarting, ACK from both peers,
    /// then only the agent handles IceRestartReady. Treat its generation idempotently.
    pub async fn refresh_turn(&self, token: &AccessToken, session: SessionId) -> Result<()> {
        no_content(
            self.http
                .post(self.url(&format!("/v1/sessions/{}/turn", session.0))?)
                .bearer_auth(&token.token)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
    }
    pub async fn presence(&self, token: &AccessToken) -> Result<Vec<DevicePresence>> {
        decode(
            self.http
                .get(self.url("/v1/presence")?)
                .bearer_auth(&token.token)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await
    }
    /// Bearer credentials travel only in the upgrade Authorization header.
    /// Before token expiry, authenticate again, close/drop the old split connection,
    /// and reconnect with the new token within the session lease. A duplicate live
    /// socket returns Limit (409); retry with bounded backoff while cleanup finishes.
    /// Reconcile the initial SessionSnapshot, then replayed grants/config/readiness.
    /// Keep an existing Peer and last applied refresh generation during rotation.
    /// A fresh process without that Peer must close old grants and create a new session.
    pub async fn events(&self, token: &AccessToken, role: DeviceRole) -> Result<DeviceConnection> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut url = self.url("/v1/events")?;
        let scheme = if self.base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        };
        url.set_scheme(scheme).map_err(|_| Error::Invalid)?;
        url.query_pairs_mut().append_pair(
            "role",
            match role {
                DeviceRole::Agent => "agent",
                DeviceRole::Client => "client",
            },
        );
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|_| Error::Invalid)?;
        request.headers_mut().insert(
            "authorization",
            format!("Bearer {}", token.token)
                .parse()
                .map_err(|_| Error::Invalid)?,
        );
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(CONTROL_PAYLOAD_LIMIT))
            .max_frame_size(Some(CONTROL_PAYLOAD_LIMIT));
        let connection = tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(request, Some(config), false),
        )
        .await
        .map_err(|_| Error::Request)?;
        match connection {
            Ok((socket, _)) => Ok(DeviceConnection(socket)),
            Err(tokio_tungstenite::tungstenite::Error::Http(response))
                if response.status().as_u16() == 409 =>
            {
                Err(Error::Limit)
            }
            Err(tokio_tungstenite::tungstenite::Error::Http(response))
                if response.status().as_u16() == 401 =>
            {
                Err(Error::Unauthorized)
            }
            Err(_) => Err(Error::Request),
        }
    }
}
fn no_content(response: reqwest::Response) -> Result<()> {
    check_status(&response)?;
    if response.status() == reqwest::StatusCode::NO_CONTENT {
        Ok(())
    } else {
        Err(Error::Request)
    }
}
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
pub struct DeviceConnection(Socket);
pub struct DeviceSender(
    futures_util::stream::SplitSink<Socket, tokio_tungstenite::tungstenite::Message>,
);
pub struct DeviceEvents(futures_util::stream::SplitStream<Socket>);
impl DeviceConnection {
    pub fn split(self) -> (DeviceSender, DeviceEvents) {
        use futures_util::StreamExt;
        let (sender, events) = self.0.split();
        (DeviceSender(sender), DeviceEvents(events))
    }
}
impl DeviceSender {
    pub async fn send(&mut self, command: &DeviceCommand) -> Result<()> {
        use futures_util::SinkExt;
        let bytes = encode_control(command).map_err(|_| Error::Invalid)?;
        let text = String::from_utf8(bytes).map_err(|_| Error::Invalid)?;
        tokio::time::timeout(
            Duration::from_secs(5),
            self.0
                .send(tokio_tungstenite::tungstenite::Message::Text(text.into())),
        )
        .await
        .map_err(|_| Error::Request)?
        .map_err(|_| Error::Request)
    }
    /// Bounded close-frame write. Drop both halves afterwards to release the socket.
    pub async fn close(&mut self) -> Result<()> {
        use futures_util::SinkExt;
        tokio::time::timeout(Duration::from_secs(1), self.0.close())
            .await
            .map_err(|_| Error::Request)?
            .map_err(|_| Error::Request)
    }
}
impl DeviceEvents {
    pub async fn next(&mut self) -> Result<Option<DeviceEvent>> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        loop {
            match self.0.next().await {
                Some(Ok(Message::Text(text))) => {
                    return decode_control(text.as_bytes())
                        .map(Some)
                        .map_err(|_| Error::Request);
                }
                Some(Ok(Message::Binary(bytes))) => {
                    return decode_control(&bytes).map(Some).map_err(|_| Error::Request);
                }
                Some(Ok(Message::Close(Some(frame)))) if frame.code==tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Policy => return Err(Error::PolicyClosed),
                Some(Ok(Message::Close(_))) | None => return Ok(None),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return Err(Error::Request),
            }
        }
    }
}

fn check_status(response: &reqwest::Response) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(Error::Unauthorized);
    }
    Err(
        match response
            .headers()
            .get("x-remotevnc-error")
            .and_then(|v| v.to_str().ok())
        {
            Some("caller_busy") => Error::CallerBusy,
            Some("agent_busy") => Error::AgentBusy,
            Some("turn_unavailable") => Error::TurnUnavailable,
            _ if response.status() == reqwest::StatusCode::CONFLICT => Error::Limit,
            _ => Error::Request,
        },
    )
}
