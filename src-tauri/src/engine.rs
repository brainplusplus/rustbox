//! Which engine this build runs, and the few decisions that follow from it.
//!
//! RustBox ships the same source tree twice:
//!
//! * the default build links [`tauri_runtime_wry`] and runs on the OS webview
//!   (WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux);
//! * the opt-in build links [`tauri_runtime_cef`] and runs on Chromium, which
//!   is what carries H.264/AAC and therefore what makes video sites work.
//!
//! The choice is made at compile time by a Cargo feature, not at run time, and
//! that is not a shortcut. `libcef.dll` is in the CEF build's PE import table,
//! so the Windows loader resolves it before `main()` runs: without the payload
//! the process cannot start, let alone decide to fall back. Two executables is
//! the only shape that can offer "use Chromium if it is installed" without
//! making the default build depend on a 360 MB download.
//!
//! Because the features are mutually exclusive, every `#[cfg]` below has
//! exactly one arm in any given build.

#[cfg(all(feature = "wry", feature = "cef"))]
compile_error!(
    "features `wry` and `cef` cannot be enabled together: `tauri-runtime-cef` \
     requires tauri/gtk4 and `tauri-runtime-wry` requires tauri/gtk3, and Cargo \
     features are additive. Build the Chromium binary with \
     `--no-default-features --features cef`."
);

#[cfg(not(any(feature = "wry", feature = "cef")))]
compile_error!("one engine feature is required: enable either `wry` (default) or `cef`");

/// Stable engine id, written to the config and read back by the launcher.
///
/// Kept separate from the crate names so a rename upstream cannot silently
/// change what a saved config means.
#[cfg(feature = "cef")]
pub const NAME: &str = "cef";

#[cfg(feature = "wry")]
pub const NAME: &str = if cfg!(windows) {
    "webview2"
} else if cfg!(target_os = "macos") {
    "wkwebview"
} else {
    "webkitgtk"
};

/// Whether this build runs Chromium.
pub const IS_CEF: bool = cfg!(feature = "cef");

/// Select the runtime this build links.
///
/// Tauri 3's `Builder::default()` is parameterised on the type-erased
/// `DynRuntime`, so the concrete runtime is supplied here and nowhere else.
/// `debug_port` is the moving session's opt-in debugging seam. WebView2 takes it
/// per window (`move_window` writes `--remote-debugging-port` into that window's
/// `additional_browser_args`), so this build ignores it here.
#[cfg(feature = "wry")]
pub fn apply(builder: tauri::Builder, _debug_port: Option<u16>) -> tauri::Builder {
    builder.runtime(tauri_runtime_wry::Wry::default())
}

/// Select the Chromium runtime and point its cache at RustBox's data root.
///
/// `root_cache_path` has to be set before the first browser exists: CEF refuses
/// to create a request context whose cache is outside the global root, and the
/// default root is a temp directory that a cleanup pass could delete from under
/// a running profile.
///
/// The shared switches travel here rather than on a webview builder: Chromium's
/// command line is per process, and the CEF runtime logs a warning when a
/// webview sets `additional_browser_args` (see `webview_args::switches`). A
/// per-app proxy still rides on the webview, because `proxy_url` is a request
/// context of its own in Chromium rather than a process switch.
#[cfg(feature = "cef")]
pub fn apply(builder: tauri::Builder, debug_port: Option<u16>) -> tauri::Builder {
    let mut cef = tauri_runtime_cef::Cef::default().root_cache_path(crate::paths::cef_root());
    // The moving session is the one window nobody can reach a console from, so
    // it may ask for a DevTools port. It has to be configured here rather than
    // per webview: Chromium's debugging server belongs to the browser process.
    if let Some(port) = debug_port {
        cef = cef.remote_debugging(tauri_runtime_cef::RemoteDebugging::Port {
            port,
            allowed_origins: Vec::new(),
        });
    }
    for (key, value) in crate::webview_args::switches() {
        // `--disable-features` must not go through the command line: Chromium stores a
        // switch by name and the last value *replaces* the previous one, so a raw
        // switch becomes the whole list and drops the entries CEF wrote to keep Chrome
        // from crashing at startup. Measured: it exits with `0xc0000005` before the
        // window appears. The typed call merges instead.
        if key == "--disable-features" {
            let features = value.unwrap_or_default().split(',').filter(|f| !f.is_empty());
            cef = cef.disable_features(features);
        } else {
            cef = cef.command_line_arg(key, value);
        }
    }
    for (key, value) in cef_only_switches() {
        cef = cef.command_line_arg(key, value);
    }
    builder.runtime(cef)
}

/// Per-profile ceiling for the Chromium HTTP disk cache, in bytes.
///
/// Kept as a named constant so the reasoning below has one number to point at.
const DISK_CACHE_MAX_BYTES: u32 = 128 * 1024 * 1024;

/// Switches that belong to the Chromium build only.
///
/// They stay out of `webview_args::BASE` for two reasons: that constant is
/// mirrored into `tauri.conf.json` and pinned by a test, and WebView2 already
/// caps its own cache budget, so the switch would be noise in the default build.
///
/// `--disk-cache-size`: Chromium has no upper bound on the HTTP disk cache by
/// default — it grows to roughly 320 MiB per cache backend and stays there.
/// RustBox gives every app its own backend (`root_cache_path` plus a per-app
/// request-context cache), so the ceiling multiplies by the number of apps and a
/// runaway cache in one profile only shows up as a very large `Cache` folder and
/// a steady stream of rewrites. Measured on this machine: ~14 MB typical, 98 MB
/// peak, so 128 MiB leaves a healthy profile untouched and only trims the ones
/// that would otherwise keep growing.
///
/// This does *not* address the IndexedDB churn that dominates writes: that is a
/// site database, not the HTTP cache.
///
/// Compiled in both builds so the default build's test run can pin its value;
/// only the CEF build applies it.
#[cfg_attr(not(feature = "cef"), allow(dead_code))]
fn cef_only_switches() -> Vec<(&'static str, Option<String>)> {
    vec![(
        "--disk-cache-size",
        Some(DISK_CACHE_MAX_BYTES.to_string()),
    )]
}

/// Register the profile a tab should be created in.
///
/// WebView2 needs this out of band (see the vendored `tauri-runtime-wry`); CEF
/// takes the profile as a `data_directory` on the builder instead, so there is
/// nothing to register and no CEF version of this function exists.
#[cfg(feature = "wry")]
pub fn set_profile_name(label: &str, app_id: &str) {
    #[cfg(windows)]
    tauri_runtime_wry::set_webview_profile_name(label, Some(crate::paths::profile_name(app_id)));
    #[cfg(not(windows))]
    let _ = (label, app_id);
}

/// Clear a registered profile name once the tab no longer exists.
#[cfg(feature = "wry")]
pub fn clear_profile_name(label: &str) {
    #[cfg(windows)]
    tauri_runtime_wry::set_webview_profile_name(label, None);
    #[cfg(not(windows))]
    let _ = label;
}

/// Clear a registered profile name once the tab no longer exists.
///
/// Chromium registers nothing per label, so there is nothing to clear; the
/// function still exists so the tab teardown path has one shape in both builds.
#[cfg(feature = "cef")]
pub fn clear_profile_name(label: &str) {
    let _ = label;
}

/// The per-app data directory this engine should open a tab with.
///
/// Both engines get one directory per app; the difference is what it means.
/// WebView2 treats it as a user data folder, CEF as a request context cache.
/// macOS is the exception: WKWebView ignores `data_directory`, which is why that
/// platform uses a named data store instead.
///
/// Only the Chromium build calls this: the WebView2 paths in `tabs.rs` build the
/// path themselves because `Light` and `Isolated` want different directories.
#[cfg(feature = "cef")]
pub fn app_data_directory(app_id: &str) -> Option<std::path::PathBuf> {
    Some(crate::paths::ensure(crate::paths::cef_profile(app_id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shared string is mirrored into `tauri.conf.json`, where an extra
    /// switch would be harmless on WebView2 but wrong to carry. Keeping the CEF
    /// switches in their own list is what makes that impossible.
    #[test]
    fn cef_only_switches_do_not_leak_into_the_shared_string() {
        for (key, _) in cef_only_switches() {
            assert!(
                !crate::webview_args::BASE.contains(key),
                "{key} must stay out of webview_args::BASE"
            );
        }
    }

    /// A ceiling at or above Chromium's own default would be ignored, and one
    /// far below the working set would evict on every request and cause the very
    /// write amplification this switch is meant to reduce.
    #[test]
    fn disk_cache_ceiling_is_below_the_default_and_above_the_working_set() {
        let switches = cef_only_switches();
        let (_, value) = switches
            .iter()
            .find(|(key, _)| *key == "--disk-cache-size")
            .expect("the HTTP cache ceiling is applied");
        let bytes: u64 = value
            .as_deref()
            .expect("--disk-cache-size takes a byte count")
            .parse()
            .expect("the ceiling is a plain number of bytes");
        assert_eq!(bytes, u64::from(DISK_CACHE_MAX_BYTES));
        assert!(
            bytes <= 320 * 1024 * 1024,
            "Chromium already caps at ~320 MiB; a larger value would never apply"
        );
        assert!(
            bytes >= 64 * 1024 * 1024,
            "under 64 MiB the cache evicts constantly and writes more, not less"
        );
    }
}
