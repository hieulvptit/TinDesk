use crate::{Error, Result};
use url::Url;

/// Validated public routing and Google credentials. Deliberately does not implement Debug.
#[derive(Clone)]
pub struct Config {
    pub public_origin: Url,
    pub redirect_uri: Url,
    pub google_client_id: String,
    pub google_client_secret: String,
}
impl Config {
    pub fn new(
        origin: &str,
        redirect: Option<&str>,
        client_id: String,
        secret: String,
    ) -> Result<Self> {
        let public_origin = Url::parse(origin).map_err(|_| Error::Invalid)?;
        if public_origin.scheme() != "https"
            || public_origin.host_str().is_none()
            || public_origin.path() != "/"
            || public_origin.query().is_some()
            || public_origin.fragment().is_some()
            || !public_origin.username().is_empty()
            || public_origin.password().is_some()
            || client_id.trim().is_empty()
            || secret.trim().is_empty()
        {
            return Err(Error::Invalid);
        }
        let redirect_uri = match redirect {
            Some(value) => Url::parse(value).map_err(|_| Error::Invalid)?,
            None => public_origin
                .join("/auth/google/callback")
                .map_err(|_| Error::Invalid)?,
        };
        if redirect_uri.origin() != public_origin.origin()
            || redirect_uri.query().is_some()
            || redirect_uri.fragment().is_some()
            || !redirect_uri.username().is_empty()
            || redirect_uri.password().is_some()
            || redirect_uri.path() == "/"
            || redirect_uri.path().starts_with("/v1")
            || ["/auth/google/start", "/auth/google/confirm"].contains(&redirect_uri.path())
            || redirect_uri.path().contains(['{', '}', '*', '%'])
        {
            return Err(Error::Invalid);
        }
        Ok(Self {
            public_origin,
            redirect_uri,
            google_client_id: client_id,
            google_client_secret: secret,
        })
    }
    pub fn from_env() -> Result<Self> {
        let required = |name| std::env::var(name).map_err(|_| Error::Invalid);
        Self::new(
            &required("REMOTEVNC_PUBLIC_ORIGIN")?,
            match std::env::var("REMOTEVNC_GOOGLE_REDIRECT_URI") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(_) => return Err(Error::Invalid),
            }
            .as_deref(),
            required("REMOTEVNC_GOOGLE_CLIENT_ID")?,
            required("REMOTEVNC_GOOGLE_CLIENT_SECRET")?,
        )
    }
}
