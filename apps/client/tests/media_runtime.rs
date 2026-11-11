#![cfg(target_os = "macos")]
use std::{path::Path, process::Command};

#[test]
fn bundled_media_child() {
    if std::env::var_os("BINACHICKEN_BUNDLE_TEST_CHILD").is_none() {
        return;
    }
    gst::init().unwrap();
    // The normal executable path, also used by LaunchAgent current_exe, initializes it.
    binachicken_client::media_runtime::initialize_bundled().unwrap();
    for factory in [
        "webrtcbin",
        "nicesrc",
        "nicesink",
        "dtlsenc",
        "sctpenc",
        "h264parse",
        "avdec_h264",
        "x264enc",
    ] {
        assert!(
            gst::ElementFactory::find(factory).is_some(),
            "missing required packaged factory: {factory}"
        );
    }
    let nice = gst::Registry::get().find_plugin("nice").unwrap();
    assert!(
        nice.filename().unwrap().starts_with(
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("Resources")
        )
    );
    // Discovery alone is insufficient: cached factories may fail to instantiate.
    use binachicken_protocol::{DeviceId, DisplayInfo, SessionId};
    use binachicken_transport::{Peer, PeerConfig, PeerRole};
    let peer = Peer::new(PeerConfig {
        session_id: SessionId::from_uuid(uuid::Uuid::nil()),
        local_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(1)),
        remote_peer_id: DeviceId::from_uuid(uuid::Uuid::from_u128(2)),
        role: PeerRole::Controller,
        display: Some(DisplayInfo {
            generation: 1,
            width: 640,
            height: 480,
        }),
        ice_servers: vec![],
        relay_only: false,
    })
    .expect("packaged WebRTC engine must instantiate without shell environment");
    peer.enable_h264().unwrap();
    peer.close();
}

#[test]
fn app_bundle_finds_media_without_terminal_environment_or_registry_cache() {
    let root = tempfile::tempdir().unwrap();
    let contents = root.path().join("BinaChicken.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources/gst-plugins")).unwrap();
    let executable = contents.join("MacOS/binachicken-client");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.deps/media/gst-plugins/libgstnice.dylib");
    std::fs::copy(
        source,
        contents.join("Resources/gst-plugins/libgstnice.dylib"),
    )
    .unwrap();
    let mut command = Command::new(executable);
    command
        .args(["--exact", "bundled_media_child", "--nocapture"])
        .env("BINACHICKEN_BUNDLE_TEST_CHILD", "1")
        .env("GST_REGISTRY_1_0", root.path().join("fresh-registry.bin"));
    for variable in [
        "GST_PLUGIN_PATH",
        "GST_PLUGIN_PATH_1_0",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SYSTEM_PATH_1_0",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ] {
        command.env_remove(variable);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    // A broken bundle must fail instead of relying on another plugin/cache path.
    std::fs::remove_file(contents.join("Resources/gst-plugins/libgstnice.dylib")).unwrap();
    assert!(!command.output().unwrap().status.success());
}
