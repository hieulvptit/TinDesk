//! Identity and enrollment service, independent of native media.
pub mod auth;
pub mod config;
pub mod db;
pub mod devices;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post},
};
use binachicken_protocol::*;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid request or credentials")]
    Invalid,
    #[error("service unavailable")]
    Internal,
    #[error("caller already has an active session")]
    CallerBusy,
    #[error("agent already has an active session")]
    AgentBusy,
    #[error("TURN unavailable")]
    TurnUnavailable,
    #[error("active device limit reached")]
    Limit,
}
pub type Result<T> = std::result::Result<T, Error>;
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Invalid => StatusCode::UNAUTHORIZED,
            Self::Internal | Self::TurnUnavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Limit | Self::CallerBusy | Self::AgentBusy => StatusCode::CONFLICT,
        };
        let code = match self {
            Self::CallerBusy => "caller_busy",
            Self::AgentBusy => "agent_busy",
            Self::TurnUnavailable => "turn_unavailable",
            Self::Limit => "limit",
            Self::Invalid => "unauthorized",
            Self::Internal => "unavailable",
        };
        let mut response = (status, "Request could not be completed").into_response();
        response
            .headers_mut()
            .insert("x-remotevnc-error", HeaderValue::from_static(code));
        response
    }
}
#[derive(Clone)]
pub struct BackendState {
    pub config: config::Config,
    pub auth: auth::AuthService,
    pub devices: devices::DeviceService,
    pub sessions: sessions::SessionService,
}
pub fn router(state: BackendState) -> Router {
    let callback = state.config.redirect_uri.path().to_owned();
    Router::new()
        .route("/v1/sessions", post(create_session))
        .route("/v1/sessions/{id}/renew", post(renew_session))
        .route("/v1/sessions/{id}/turn", post(refresh_turn))
        .route("/v1/sessions/{id}", delete(close_session))
        .route("/v1/events", get(signaling::events))
        .route("/v1/presence", get(presence))
        .route("/v1/enrollments", post(begin))
        .route("/v1/enrollments/{id}/poll", post(poll))
        .route("/auth/google/start", get(start))
        .route(&callback, get(finish))
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
async fn finish(
    State(s): State<BackendState>,
    Query(q): Query<Callback>,
) -> Result<Html<&'static str>> {
    let completion = s.auth.finish_login(&q.code, &q.state).await?;
    s.auth.auto_confirm(&completion.confirmation)?;
    Ok(Html(
        "<!doctype html><title>BinaChicken device registered</title><p>Google sign-in complete. This device is registered. Return to BinaChicken.</p>",
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
    s.devices.revoke(bearer(&headers)?, DeviceId(id)).await?;
    Ok(StatusCode::NO_CONTENT)
}
pub mod turn;

pub mod sessions;

pub mod signaling;

async fn create_session(
    State(s): State<BackendState>,
    headers: HeaderMap,
    Json(request): Json<CreateSession>,
) -> Result<Json<SessionGrant>> {
    s.sessions
        .create_authenticated(bearer(&headers)?, request.agent_id)
        .await
        .map(Json)
}
async fn renew_session(
    State(s): State<BackendState>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<SessionGrant>> {
    s.sessions
        .renew_authenticated(bearer(&headers)?, SessionId(id))
        .await
        .map(Json)
}
async fn close_session(
    State(s): State<BackendState>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode> {
    s.sessions
        .close_authenticated(bearer(&headers)?, SessionId(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn refresh_turn(
    State(s): State<BackendState>,
    headers: HeaderMap,
    Path(id): Path<uuid::Uuid>,
) -> Result<StatusCode> {
    s.sessions
        .refresh_turn_authenticated(bearer(&headers)?, SessionId(id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
async fn presence(
    State(s): State<BackendState>,
    headers: HeaderMap,
) -> Result<Json<Vec<DevicePresence>>> {
    s.sessions
        .presence_authenticated(bearer(&headers)?)
        .await
        .map(Json)
}
