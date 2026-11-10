//! Single source of truth for every path rustbox writes to.
//!
//! Everything lives under one root so the user can find it, back it up, or
//! delete it in one move:
//!
//! ```text
//! Windows  %LOCALAPPDATA%\rustbox
//! macOS    ~/Library/Application Support/rustbox
//! Linux    $XDG_DATA_HOME/rustbox  (fallback: ~/.local/share/rustbox)
//!
//! rustbox\
//!   config.json             workspace config (apps, favorites, usage)
//!   EBWebView\              main-window webview data (Tauri default location)
//!     WV2Profile_<app-id>\  per-app profile for `ProfileMode::Light`
//!   profiles\<app-id>\      per-app profile for `ProfileMode::Isolated`
//!   external\<app-id>\      per-app profiles handed to external browsers
//!     chromium\             Chrome, Edge, Brave, CloakBrowser (shared session)
//!     gecko\                Firefox, Camoufox (separate format, own session)
//! ```
//!
//! `dirs_next::data_local_dir()` already returns the platform-correct base
//! (`%LOCALAPPDATA%`, `~/Library/Application Support`, `$XDG_DATA_HOME`), so
//! there is no per-OS branching beyond this documentation.
//!
//! The main window's webview data is created by Tauri at
//! `app_local_data_dir()`, which resolves to this same root because the bundle
//! identifier is plain `rustbox` (see `tauri.conf.json`). Tauri forces that
//! directory on Windows and Linux (`manager/webview.rs`), so the UI shell
//! profile lands in `<root>\EBWebView` next to everything else. On macOS it
//! uses the system default instead.

use std::path::PathBuf;

/// Root data directory.
pub fn root() -> PathBuf {
    dirs_next::data_local_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("rustbox")
}

/// Workspace config file.
pub fn config_file() -> PathBuf {
    root().join("config.json")
}

/// Per-app profile for an embedded tab.
pub fn embedded_profile(app_id: &str) -> PathBuf {
    root().join("profiles").join(sanitize(app_id))
}

/// Everything an app's external browsers write, one subdirectory per family.
///
/// The family level is not cosmetic: a Gecko profile (`prefs.js`, `places.sqlite`)
/// and a Chromium profile (`Local State`, `Default\`) are different formats and
/// cannot share a directory. Keeping them apart is also what makes Chrome, Edge
/// and CloakBrowser keep each other's session — they all read `chromium`.
pub fn external_root(app_id: &str) -> PathBuf {
    root().join("external").join(sanitize(app_id))
}

/// Per-app profile handed to one browser family via `--user-data-dir` or
/// `-profile`. Camoufox and CloakBrowser keep their fingerprint seed, cookies
/// and history here, so it must never live in the temp directory.
///
/// `family` is `"chromium"` or `"gecko"`; see `BrowserFamily::dir_name`.
pub fn external_profile(app_id: &str, family: &str) -> PathBuf {
    external_root(app_id).join(family)
}

/// Pre-family layout, where one folder was handed to Chrome and Camoufox alike.
///
/// Kept so an existing profile can be moved into its family folder instead of
/// being silently abandoned; see `external::adopt_legacy_profile`.
pub fn legacy_external_profile(app_id: &str) -> PathBuf {
    external_root(app_id)
}

/// Profile name handed to WebView2 for `ProfileMode::Light`. WebView2 prefixes
/// the on-disk folder with `WV2Profile_`, so the name itself must stay a plain
/// identifier (see [`light_profile`] for the resulting directory).
pub fn profile_name(app_id: &str) -> String {
    sanitize(app_id)
}

/// On-disk folder WebView2 creates for a `Light` profile. Only used to delete
/// it; WebView2 itself owns the layout.
pub fn light_profile(app_id: &str) -> PathBuf {
    root()
        .join("EBWebView")
        .join(format!("WV2Profile_{}", profile_name(app_id)))
}

/// `WKWebsiteDataStore` identifier for per-app isolation on macOS.
///
/// WKWebView ignores `data_directory`, so a per-app data directory isolates
/// nothing there. macOS 14 added named data stores instead, which wry exposes
/// as `data_store_identifier`. The identifier must be stable across launches
/// (a fresh one would drop the session every start), so it is derived from the
/// app id rather than generated randomly.
///
/// This is an identifier, not a security hash: two apps colliding would share
/// site data. 128 bits derived from a handful of ids makes that moot.
///
/// Defined on every platform so the derivation is unit-tested where we build,
/// and so a config authored on one OS can be reasoned about on another; only
/// the macOS call site uses it at runtime.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn data_store_identifier(app_id: &str) -> [u8; 16] {
    // Two FNV-1a passes with different offset bases, so the halves are not
    // correlated. Not cryptographic; it only needs to be stable and spread out.
    fn fnv1a(bytes: &[u8], offset: u64) -> u64 {
        let mut hash = offset;
        for byte in bytes {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
    let seed = sanitize(app_id);
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&fnv1a(seed.as_bytes(), 0xcbf2_9ce4_8422_2325).to_be_bytes());
    id[8..].copy_from_slice(&fnv1a(seed.as_bytes(), 0x9e37_79b9_7f4a_7c15).to_be_bytes());
    id
}

/// Create a directory and return its path. "Already exists" is fine.
pub fn ensure(dir: PathBuf) -> PathBuf {
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Delete an app's profile directory. Returns whether anything was removed.
pub fn remove_profile(dir: PathBuf) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir)
        .map(|_| true)
        .map_err(|e| format!("could not delete {}: {e}", dir.display()))
}

/// Every folder rustbox may have created for one app, whatever engine or mode
/// it currently uses.
///
/// An app can leave a folder behind each time the engine or storage mode is
/// switched, and nothing used to clean them up: 40–70 MB per abandoned profile
/// is normal, so a workspace edited a few times silently grows by hundreds of
/// megabytes. Everything that deletes or measures an app's data goes through
/// this list so no location is missed.
pub fn app_data_dirs(app_id: &str) -> Vec<PathBuf> {
    vec![
        embedded_profile(app_id),
        light_profile(app_id),
        external_root(app_id),
    ]
}

/// Total bytes held by an app across every profile location.
pub fn app_data_size(app_id: &str) -> u64 {
    app_data_dirs(app_id)
        .iter()
        .map(|dir| dir_size(dir))
        .sum()
}

/// Recursive size of a directory, 0 when it does not exist.
///
/// Symlinks are not followed: a profile folder that links elsewhere must not
/// make this walk outside the data root or loop forever.
fn dir_size(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            total += dir_size(&entry.path());
        } else if let Ok(meta) = entry.metadata() {
            total += meta.len();
        }
    }
    total
}

/// Delete everything rustbox stored for one app. Returns bytes reclaimed.
///
/// Used both by "Reset session" and when the app itself is removed: a deleted
/// app that leaves its cookies on disk is a privacy problem, not just clutter.
///
/// Every location is attempted even if one is locked, so a running browser
/// holding one folder open does not stop the others from being cleared. The
/// first failure is reported afterwards rather than swallowed.
pub fn purge_app_data(app_id: &str) -> Result<u64, String> {
    let mut freed = 0;
    let mut first_error: Option<String> = None;
    for dir in app_data_dirs(app_id) {
        freed += dir_size(&dir);
        if let Err(e) = remove_profile(dir) {
            first_error.get_or_insert(e);
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(freed),
    }
}

/// Keep app ids from escaping the profiles directory. Ids come from the
/// frontend, so treat them as untrusted input.
fn sanitize(id: &str) -> String {
    // Reject "." / ".." before mapping: afterwards they would become "_" and
    // "__", which are harmless but silently collide with real ids.
    if id.is_empty() || id.chars().all(|c| c == '.') {
        return "app".to_string();
    }
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "app".to_string()
    } else {
        cleaned
    }
}

/// Locations reported to the UI so users can see where their data lives.
#[derive(serde::Serialize)]
pub struct DataPaths {
    pub root: String,
    pub config: String,
    pub profiles: String,
    pub external: String,
    /// `std::env::consts::OS`, so the UI can explain platform limits.
    pub platform: String,
}

pub fn report() -> DataPaths {
    let root = root();
    DataPaths {
        config: config_file().display().to_string(),
        profiles: root.join("profiles").display().to_string(),
        external: root.join("external").display().to_string(),
        root: root.display().to_string(),
        platform: std::env::consts::OS.to_string(),
    }
}

/// Where a captured screenshot is written.
///
/// Screenshots come from the agent as base64 PNG. Writing them to disk gives the
/// user something they can open, and gives the chat log a path instead of a
/// megabyte of base64 per frame.
pub fn save_screenshot(base64_png: &str) -> Result<PathBuf, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_png.trim())
        .map_err(|e| format!("the screenshot was not valid base64: {e}"))?;
    let dir = root().join("screenshots");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("shot-{stamp}.png"));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal() {
        assert_eq!(sanitize("../../evil"), "______evil");
        assert_eq!(sanitize(".."), "app");
        assert_eq!(sanitize("."), "app");
        assert_eq!(sanitize(""), "app");
        assert_eq!(sanitize("app-mvh086f6"), "app-mvh086f6");
    }

    #[test]
    fn profiles_are_under_root() {
        let r = root();
        assert!(embedded_profile("x").starts_with(&r));
        assert!(external_profile("x", "chromium").starts_with(&r));
        assert!(light_profile("x").starts_with(&r));
        assert_eq!(config_file(), r.join("config.json"));
        assert_eq!(embedded_profile("x"), r.join("profiles").join("x"));
        assert_eq!(
            external_profile("x", "chromium"),
            r.join("external").join("x").join("chromium")
        );
        // The two families must never resolve to the same folder: a Gecko
        // profile and a Chromium profile in one directory corrupt each other.
        assert_ne!(
            external_profile("x", "chromium"),
            external_profile("x", "gecko")
        );
        assert_eq!(light_profile("x"), r.join("EBWebView").join("WV2Profile_x"));
        assert_eq!(profile_name("x"), "x");
        // The name reaches a filesystem path, so traversal must not survive.
        assert_eq!(profile_name("../../evil"), "______evil");
    }

    #[test]
    fn remove_profile_is_idempotent() {
        assert_eq!(remove_profile(root().join("does-not-exist-xyz")), Ok(false));
    }

    #[test]
    fn data_store_identifier_is_stable_and_distinct() {
        // Stable: the same app must map to the same store across launches, or
        // the user is logged out on every restart.
        assert_eq!(data_store_identifier("app-a"), data_store_identifier("app-a"));
        // Distinct: two apps sharing an identifier would share a session.
        assert_ne!(data_store_identifier("app-a"), data_store_identifier("app-b"));
        // The halves must not be correlated, or ids differing in one byte could
        // collide on the other half.
        let id = data_store_identifier("app-a");
        assert_ne!(id[..8], id[8..]);
        // Traversal in the id must not change the result shape.
        assert_eq!(
            data_store_identifier("../../evil"),
            data_store_identifier("______evil")
        );
    }
}
