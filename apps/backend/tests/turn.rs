use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use remotevnc_backend::turn::TurnClient;
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

async fn fixture(status: StatusCode, body: Value) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/turn/keys/test-key/credentials/generate-ice-servers",
        listener.local_addr().unwrap()
    );
    let router = Router::new().route(
        "/v1/turn/keys/test-key/credentials/generate-ice-servers",
        post(
            move |headers: HeaderMap, Json(request): Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-token");
                assert_eq!(request, json!({"ttl":3600}));
                (status, Json(body))
            },
        ),
    );
    (
        url,
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() }),
    )
}
fn response() -> Value {
    json!({"iceServers":[{"urls":["stun:stun.cloudflare.com:3478"]},{"urls":["turn:turn.cloudflare.com:3478?transport=udp","turn:turn.cloudflare.com:443?transport=udp","turn:turn.cloudflare.com:3478?transport=tcp","turn:turn.cloudflare.com:80?transport=tcp","turns:turn.cloudflare.com:5349?transport=tcp","turns:turn.cloudflare.com:443?transport=tcp"],"username":"user:escaped","credential":"password+/="}]})
}
#[tokio::test]
async fn documented_contract_and_conservative_expiry() {
    let (url, task) = fixture(StatusCode::CREATED, response()).await;
    let before = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let client = TurnClient::new(&url, "fixture-token".into()).unwrap();
    let servers = client.issue(Duration::from_secs(3600)).await.unwrap();
    assert_eq!(servers.len(), 2);
    assert_eq!(servers[1].urls.len(), 6);
    assert_eq!(servers[1].username.as_deref(), Some("user:escaped"));
    assert_eq!(servers[1].credential.as_deref(), Some("password+/="));
    assert!(
        servers
            .iter()
            .all(|s| s.expires_at_unix_secs.unwrap() >= before + 3600)
    );
    task.abort();
}
#[tokio::test]
async fn rejects_provider_failure_and_invalid_responses() {
    for (status, body) in [
        (StatusCode::UNAUTHORIZED, response()),
        (StatusCode::OK, response()),
        (StatusCode::CREATED, json!({"iceServers":[]})),
        (
            StatusCode::CREATED,
            json!({"iceServers":[{"urls":["https://evil.test"]}]}),
        ),
        (
            StatusCode::CREATED,
            json!({"iceServers":[{"urls":["turn:turn.cloudflare.com:3478"]}]}),
        ),
        (
            StatusCode::CREATED,
            json!({"iceServers":[{"urls":["stun:a:3478","stun:b:3478"]}]}),
        ),
        (
            StatusCode::CREATED,
            json!({"iceServers":[{"urls":["turn:host:3478"],"username":"u","credential":"x".repeat(70000)}]}),
        ),
    ] {
        let (url, task) = fixture(status, body).await;
        assert!(
            TurnClient::new(&url, "fixture-token".into())
                .unwrap()
                .issue(Duration::from_secs(3600))
                .await
                .is_err()
        );
        task.abort();
    }
    assert!(
        TurnClient::unavailable()
            .issue(Duration::from_secs(3600))
            .await
            .is_err()
    );
    assert!(TurnClient::new("http://remote.test/path", "token".into()).is_err());
}

#[tokio::test]
async fn provider_timeout_is_bounded_and_does_not_return_credentials() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/slow", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/slow",
                post(|| async {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    (StatusCode::CREATED, Json(response()))
                }),
            ),
        )
        .await
        .unwrap()
    });
    let start = std::time::Instant::now();
    assert!(
        TurnClient::new(&endpoint, "fixture-token".into())
            .unwrap()
            .issue(Duration::from_secs(3600))
            .await
            .is_err()
    );
    assert!(start.elapsed() < Duration::from_secs(7));
    task.abort();
}
