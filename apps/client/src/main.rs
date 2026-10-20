use remotevnc_client::app::{ClientApp, Config};
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut backend =
        std::env::var("REMOTEVNC_BACKEND").unwrap_or_else(|_| "https://amio.vnoffice.io.vn".into());
    let mut relay = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--backend" => backend = args.next().ok_or("--backend requires an HTTPS origin")?,
            "--relay-only" => relay = true,
            "--help" => {
                println!(
                    "remotevnc-client [--backend HTTPS_ORIGIN] [--relay-only]\nREMOTEVNC_BACKEND supplies the default origin. Credentials use macOS Keychain."
                );
                return Ok(());
            }
            _ => return Err("unknown argument; use --help".into()),
        }
    }
    let mut config = Config::new(&backend)?;
    config.relay_only = relay;
    let options = eframe::NativeOptions {
        clipboard_shortcuts: false,
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("RemoteVNC")
            .with_inner_size([1100., 760.]),
        ..Default::default()
    };
    eframe::run_native(
        "RemoteVNC",
        options,
        Box::new(move |_| Ok(Box::new(ClientApp::new(config)))),
    )
    .map_err(|e| e.to_string().into())
}
