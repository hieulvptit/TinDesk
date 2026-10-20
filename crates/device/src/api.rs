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
        if response.status() != reqwest::StatusCode::NO_CONTENT {
            return Err(Error::Request);
        }
        Ok(())
    }
}
pub(crate) async fn decode<T: DeserializeOwned>(mut response: reqwest::Response) -> Result<T> {
    if response.status() == reqwest::StatusCode::CONFLICT {
        return Err(Error::Limit);
    }
    if !response.status().is_success() {
        return Err(Error::Request);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::Request)? {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(Error::Request);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::Request)
}
