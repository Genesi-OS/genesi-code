//! Tools for loading a [`ChannelConfig`] from the external config generator binary.
//!
//! For non-bundled builds, the generator is invoked at runtime. For bundled builds, the config
//! is embedded at compile time via the build script.
use warp_core::channel::{ChannelConfig, OzConfig, WarpServerConfig};
use warp_core::AppId;

/// The name of the config generator binary, expected to be on PATH.
const CONFIG_BIN_NAME: &str = "warp-channel-config";

#[macro_export]
#[cfg(windows)]
macro_rules! path_concat {
    ($path:expr, $file:expr) => {
        concat!($path, "\\", $file)
    };
}
#[macro_export]
#[cfg(not(windows))]
macro_rules! path_concat {
    ($path:expr, $file:expr) => {
        concat!($path, "/", $file)
    };
}

#[macro_export]
macro_rules! load_config {
    ($channel:expr) => {{
        #[cfg(feature = "release_bundle")]
        {
            channel_config::load_config_from_embedded(include_str!($crate::path_concat!(
                env!("OUT_DIR"),
                concat!($channel, "_config.json")
            )))
        }

        #[cfg(not(feature = "release_bundle"))]
        {
            channel_config::load_config_from_generator($channel)
        }
    }};
}
pub use load_config;

/// Invokes the config generator binary at runtime and deserializes its JSON output into a
/// [`ChannelConfig`].
#[cfg_attr(feature = "release_bundle", expect(dead_code))]
pub fn load_config_from_generator(channel: &str) -> ChannelConfig {
    let target_family = if cfg!(target_family = "wasm") {
        "wasm"
    } else {
        "native"
    };

    let target_os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };

    // Genesi Code fork: upstream's `warp-channel-config` generator lives in a
    // private Warp repo (`warpdotdev/warp-channel-config`) that wasn't open
    // sourced, so non-bundled builds can't shell out to it. Return the same
    // in-Rust default config that `ChannelState::init()` uses for the OSS
    // channel: Warp's production servers, with telemetry, autoupdate, and crash
    // reporting all disabled (Genesi doesn't phone home and ships its own update
    // pipeline). The channel/target args are kept for signature compatibility.
    let _ = (channel, target_family, target_os);
    ChannelConfig {
        app_id: AppId::new("dev", "genesi", "GenesiCode"),
        // MUST be non-empty: when stdout isn't a tty (launched from the KDE menu
        // / a plasmoid), the logger writes to a file at `log_directory / logfile_
        // name`. An empty name made that path the log directory itself, so opening
        // it as a file failed with "Is a directory (os error 21)" and the whole
        // app exited before its window appeared — i.e. "runs from a terminal but
        // loads-and-closes from the menu". A terminal (tty) skips the logfile path
        // entirely, which is why it only crashed off a tty.
        logfile_name: "genesicode.log".into(),
        server_config: WarpServerConfig::production(),
        oz_config: OzConfig::production(),
        telemetry_config: None,
        autoupdate_config: None,
        crash_reporting_config: None,
        mcp_static_config: None,
    }
}

/// Deserializes a [`ChannelConfig`] from a JSON string embedded at compile time.
///
/// This is used to load the channel configuration in release bundles, where configuration
/// is embedded at compile time instead of being generated at runtime.
#[cfg_attr(not(feature = "release_bundle"), expect(dead_code))]
pub fn load_config_from_embedded(json: &str) -> ChannelConfig {
    serde_json::from_str(json)
        .unwrap_or_else(|err| panic!("Failed to parse embedded channel config: {err}"))
}
