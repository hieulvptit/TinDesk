//! Initialize the development bundle's one local ICE plugin. Other native
//! libraries/plugins use their compiled Homebrew paths; no environment mutation.
use std::path::Path;

pub fn initialize_bundled() -> crate::Result<()> {
    let executable = std::env::current_exe()?;
    let Some(macos) = executable
        .parent()
        .filter(|p| p.file_name().is_some_and(|n| n == "MacOS"))
    else {
        // `cargo run` continues to use the documented development media setup.
        return Ok(());
    };
    let contents = macos.parent().ok_or("invalid macOS bundle")?;
    if contents.file_name() != Some(std::ffi::OsStr::new("Contents"))
        || contents.parent().and_then(Path::extension) != Some(std::ffi::OsStr::new("app"))
    {
        return Err("invalid macOS bundle layout".into());
    }
    let plugin = contents.join("Resources/gst-plugins/libgstnice.dylib");
    if !std::fs::symlink_metadata(&plugin)?.file_type().is_file() {
        return Err("bundled ICE plugin must be a regular file; rebuild the app".into());
    }
    gst::init()?;
    // Supported API registers factories in this process even with a fresh registry.
    // Fixed bundle-relative path only; no user-supplied plugin location/configuration.
    gst::Plugin::load_file(plugin)?;
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
        if gst::ElementFactory::find(factory).is_none() {
            return Err(format!("required native media component unavailable: {factory}; run native dependency setup and rebuild").into());
        }
    }
    Ok(())
}
