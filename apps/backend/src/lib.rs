//! Identity and enrollment service, independent of native media.
pub mod auth;
pub mod config;
pub mod db;
pub mod devices;
use axum::{
    Form, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post},
};
use remotevnc_protocol::*;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request or credentials")]
    Invalid,
    #[error("service unavailable")]
    Internal,
    #[error("active device limit reached")]
    Limit,
}
pub type Result<T> = std::result::Result<T, Error>;
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Invalid => StatusCode::UNAUTHORIZED,
            Self::Internal => StatusCode::SERVICE_UNAVAILABLE,
            Self::Limit => StatusCode::CONFLICT,
        };
        (status, "Request could not be completed").into_response()
    }
}
#[derive(Clone)]
pub struct BackendState {
    pub config: config::Config,
    pub auth: auth::AuthService,
    pub devices: devices::DeviceService,
}
pub fn router(state: BackendState) -> Router {
    let callback = state.config.redirect_uri.path().to_owned();
    Router::new()
        .route("/v1/enrollments", post(begin))
        .route("/v1/enrollments/{id}/poll", post(poll))
        .route("/auth/google/start", get(start))
        .route(&callback, get(finish))
        .route("/auth/google/confirm", post(confirm))
        .route("/v1/devices/{id}/challenge", post(challenge))
        .route("/v1/devices/authenticate", post(authenticate))
        .route("/v1/devices", get(list))
        .route("/v1/devices/{id}", delete(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn(privacy_headers))
        .with_state(state)
}
async fn privacy_headers(request: axum::extract::Request, next: Next) -> Response {
    let mut response =
        match tokio::time::timeout(std::time::Duration::from_secs(45), next.run(request)).await {
            Ok(response) => response,
            Err(_) => (
                StatusCode::REQUEST_TIMEOUT,
                "Request could not be completed",
            )
                .into_response(),
        };
    if response.status().is_client_error() || response.status().is_server_error() {
        // Extractor errors can contain submitted strings; return only a fixed public message.
        *response.body_mut() = axum::body::Body::from("Request could not be completed");
        response.headers_mut().remove("content-length");
    }
    let headers = response.headers_mut();
    headers.insert("cache-control", HeaderValue::from_static("no-store"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    response
}
async fn begin(
    State(s): State<BackendState>,
    Json(request): Json<EnrollmentRequest>,
) -> Result<Json<Enrollment>> {
    s.devices.begin_enrollment(request).map(Json)
}
async fn poll(
    State(s): State<BackendState>,
    Path(id): Path<String>,
    Json(request): Json<EnrollmentPoll>,
) -> Result<Json<EnrollmentStatus>> {
    s.devices.poll_enrollment(&id, request).map(Json)
}
#[derive(Deserialize)]
struct Start {
    enrollment: String,
}
async fn start(State(s): State<BackendState>, Query(q): Query<Start>) -> Result<Redirect> {
    Ok(Redirect::to(&s.auth.start_login(&q.enrollment)?))
}
#[derive(Deserialize)]
struct Callback {
    code: String,
    state: String,
}
async fn finish(State(s): State<BackendState>, Query(q): Query<Callback>) -> Result<Html<String>> {
    let completion = s.auth.finish_login(&q.code, &q.state).await?;
    Ok(Html(format!(
        "<!doctype html><html><head><title>Confirm RemoteVNC device</title></head><body><h1>Confirm device registration</h1><p>Google account ID: {}</p><p>Device: {}</p><p>Public key: {}</p><p>Enter the display code shown on this device. Confirm only a device you control.</p><form method=\"post\" action=\"/auth/google/confirm\"><input type=\"hidden\" name=\"confirmation\" value=\"{}\"><label>Device code <input name=\"display_code\" maxlength=\"16\" required autocomplete=\"off\"></label><button type=\"submit\">Register this device</button></form></body></html>",
        escape(&completion.owner_subject),
        escape(&completion.device_name),
        escape(&completion.public_key),
        escape(&completion.confirmation)
    )))
}
#[derive(Deserialize)]
struct Confirmation {
    confirmation: String,
    display_code: String,
}
async fn confirm(
    State(s): State<BackendState>,
    Form(q): Form<Confirmation>,
) -> Result<Html<&'static str>> {
    s.auth.confirm(&q.confirmation, &q.display_code)?;
    Ok(Html(
        "<!doctype html><title>Device confirmed</title><p>Device confirmed. Return to the application to finish registration.</p>",
    ))
}
async fn challenge(
    State(s): State<BackendState>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<AuthenticationChallenge>> {
    s.devices.challenge(DeviceId(id)).map(Json)
}
async fn authenticate(
    State(s): State<BackendState>,
    Json(proof): Json<DeviceProof>,
) -> Result<Json<AccessToken>> {
    s.devices.authenticate(proof).map(Json)
}
fn bearer(headers: &HeaderMap) -> Result<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error::Invalid)
}
async fn list(
    State(s): State<BackendState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DeviceSummary>>> {
    s.devices.list(bearer(&headers)?).map(Json)
}
async fn revoke(
    State(s): State<BackendState>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode> {
    s.devices.revoke(bearer(&headers)?, DeviceId(id))?;
    Ok(StatusCode::NO_CONTENT)
}
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
