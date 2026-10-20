use remotevnc_backend::{
    BackendState, Error, Result, auth::AuthService, config::Config, db::Database,
    devices::DeviceService, router,
};
#[tokio::main]
async fn main() {
    if run().await.is_err() {
        eprintln!("RemoteVNC backend could not start or continue");
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let config = Config::from_env()?;
    let path = std::env::var("REMOTEVNC_DATABASE").map_err(|_| Error::Invalid)?;
    let db = Database::open(path)?;
    let auth = AuthService::discover_google(config.clone(), db.clone()).await?;
    let devices = DeviceService::new(config.clone(), db);
    let bind = std::env::var("REMOTEVNC_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|_| Error::Internal)?;
    axum::serve(
        listener,
        router(BackendState {
            config,
            auth,
            devices,
        }),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .map_err(|_| Error::Internal)
}
