#![cfg(target_os = "macos")]
use std::{path::Path, process::Command};

#[test]
fn bundled_media_child() {
    if std::env::var_os("REMOTEVNC_BUNDLE_TEST_CHILD").is_none() {
        return;
    }
    gst::init().unwrap();
    // The normal executable path, also used by LaunchAgent current_exe, initializes it.
    remotevnc_client::media_runtime::initialize_bundled().unwrap();
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
}

#[test]
fn app_bundle_finds_media_without_terminal_environment_or_registry_cache() {
    let root = tempfile::tempdir().unwrap();
    let contents = root.path().join("RemoteVNC.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources/gst-plugins")).unwrap();
    let executable = contents.join("MacOS/remotevnc-client");
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
        .env("REMOTEVNC_BUNDLE_TEST_CHILD", "1")
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
