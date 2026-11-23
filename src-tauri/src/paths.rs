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
//!   session.json            remembered open sub-tabs and their last URLs
//!   logs\shell.log          release-build log (there is no console to print to)
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
//!
//! That forced directory is why `main` rewrites the window config's
//! `data_directory` before the app is built: after the user moves the root, the
//! identifier-derived path would leave the shell (and every `Light` profile
//! inside its WebView2 environment) behind. With the rewrite, the shell uses
//! `<root>` too and the whole tree follows the locator.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// File in the default location naming the folder that actually holds the data.
///
/// The pointer has to live *outside* the data root, or it could not name its own
/// location: `config.json` cannot say "my folder is on D:" and still be found on
/// D: when the app starts. So the default location keeps one small text file and
/// everything else can move.
pub const LOCATOR: &str = "data-root.txt";

/// Where the data lives when the user has not moved it.
pub fn default_root() -> PathBuf {
    dirs_next::data_local_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join("rustbox")
}

/// Debug-only override, see [`root`].
pub fn debug_root() -> Option<PathBuf> {
    #[cfg(debug_assertions)]
    {
        std::env::var_os("RB_DATA_ROOT").map(PathBuf::from)
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static FALLBACK: OnceLock<Option<String>> = OnceLock::new();

/// Root data directory.
///
/// Debug builds honour `RB_DATA_ROOT`, which is what lets the hand-off and the
/// engine install be exercised end to end without touching the real workspace
/// (two instances fighting over one `config.json` is not a test, it is a
/// corruption). Release builds ignore it: a redirected data root in a shipped
/// binary would be a way to make the app write anywhere.
///
/// Otherwise this is [`default_root`], unless the locator file names another
/// folder (see [`crate::data_root`]). The answer is resolved once per process:
/// a root that changed halfway through a run would split one workspace across
/// two folders.
pub fn root() -> PathBuf {
    // A root frozen by `init_root` wins, including over `RB_DATA_ROOT`: after a
    // data-folder move the process has to run against the new folder, and the
    // debug seam would otherwise pin it to the one the move just deleted.
    if let Some(dir) = ROOT.get() {
        return dir.clone();
    }
    if let Some(dir) = debug_root() {
        return dir;
    }
    ROOT.get_or_init(|| {
        let (root, why) = resolve();
        let _ = FALLBACK.set(why);
        root
    })
    .clone()
}

/// Why the locator was ignored, if it was. Read by the settings page.
pub fn fallback_note() -> Option<String> {
    let _ = root();
    FALLBACK.get().cloned().flatten()
}

/// Resolve the root from the locator without committing to it.
///
/// Split out so a pending move can be applied between the decision and the
/// first path lookup: the move happens before anything opens a profile, which
/// is the only moment the tree is not locked.
pub fn resolve_root() -> (PathBuf, Option<String>) {
    if let Some(dir) = debug_root() {
        return (dir, None);
    }
    resolve()
}

/// Freeze the root for this process. First call wins.
///
/// `note` is kept for the settings page: a locator that named a folder which is
/// gone is not an error the app can fix, but the user has to be told, or their
/// data looks lost.
pub fn init_root(dir: PathBuf, note: Option<String>) {
    let _ = ROOT.set(dir);
    let _ = FALLBACK.set(note);
}

/// Resolve the locator into a usable root, explaining any refusal.
fn resolve() -> (PathBuf, Option<String>) {
    resolve_from(read_locator())
}

/// The decision itself, split from reading the file so it can be tested without
/// touching the real `%LOCALAPPDATA%`.
fn resolve_from(chosen: Option<PathBuf>) -> (PathBuf, Option<String>) {
    let fallback = default_root();
    let Some(chosen) = chosen else {
        return (fallback, None);
    };
    if chosen == fallback {
        return (fallback, None);
    }
    if !chosen.is_absolute() {
        return (
            fallback.clone(),
            Some(format!(
                "the saved data folder \"{}\" is not a full path, so RustBox started in {}",
                chosen.display(),
                fallback.display()
            )),
        );
    }
    if !chosen.is_dir() {
        return (
            fallback.clone(),
            Some(format!(
                "the saved data folder {} is not there any more, so RustBox started in {}",
                chosen.display(),
                fallback.display()
            )),
        );
    }
    (chosen, None)
}

/// The folder named by the locator file, if any.
pub fn read_locator() -> Option<PathBuf> {
    read_locator_in(&locator_dir())
}

/// The locator file itself. Always in the default location, never in the root
/// it names.
///
/// Debug builds honour `RB_DATA_ROOT_LOCATOR`, so the move can be exercised end
/// to end without writing the developer's real locator - the same reason
/// `RB_DATA_ROOT` exists. Release builds ignore both.
pub fn locator_file() -> PathBuf {
    #[cfg(debug_assertions)]
    if let Some(path) = std::env::var_os("RB_DATA_ROOT_LOCATOR") {
        return PathBuf::from(path);
    }
    default_root().join(LOCATOR)
}

/// The directory holding the locator, which is what [`read_locator`] and
/// [`write_locator`] work in. Also where the move's error note lives, so a
/// redirected locator keeps the note beside it instead of in the real profile.
pub fn locator_dir() -> PathBuf {
    locator_file()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(default_root)
}

/// Seam for the tests: the locator is read from `dir` instead of the real
/// `%LOCALAPPDATA%`, because a test that wrote the real one would move the
/// developer's own workspace.
pub(crate) fn read_locator_in(dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(dir.join(LOCATOR)).ok()?;
    let line = raw.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

pub(crate) fn write_locator_in(dir: &Path, target: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    let file = dir.join(LOCATOR);
    let tmp = file.with_extension("txt.tmp");
    std::fs::write(&tmp, format!("{}\n", target.display()))
        .map_err(|e| format!("could not write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &file).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not update {}: {e}", file.display())
    })
}

pub(crate) fn clear_locator_in(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(dir.join(LOCATOR)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "could not delete {}: {e}",
            dir.join(LOCATOR).display()
        )),
    }
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

/// Root of the Chromium engine's on-disk state.
///
/// CEF requires every request context's cache to live at or under one global
/// cache root, so the whole tree hangs off a single directory that can be moved
/// or deleted as a unit.
pub fn cef_root() -> PathBuf {
    root().join("cef")
}

/// Per-app request context for the Chromium engine.
///
/// This is the CEF equivalent of [`embedded_profile`]: one Chromium profile per
/// app, so cookies, localStorage and IndexedDB stay separate.
///
/// It has to be a *direct* child of [`cef_root`], not a nested one. Chrome's
/// profile manager only accepts a profile directory whose parent is the user
/// data directory; a path one level deeper is refused at browser-context
/// creation (`cef.log`: `Cannot create profile at path ...\cef\profiles\<app>`)
/// and the tab silently falls back to the shared `Default` profile, i.e. every
/// app ends up sharing one cookie jar. Measured: with `cef\profiles\<app>` the
/// cookie landed in `cef\Default\Network\Cookies`; with `cef\<app>` it lands in
/// `cef\<app>\Network\Cookies`.
pub fn cef_profile(app_id: &str) -> PathBuf {
    let mut name = sanitize(app_id);
    // Chrome owns these names in a user data directory. An app that sanitised
    // to one of them would be handed the shell's own jar instead of its own.
    if name.eq_ignore_ascii_case("Default")
        || name.to_ascii_lowercase().starts_with("profile-")
    {
        name.insert(0, '_');
    }
    cef_root().join(name)
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
        cef_profile(app_id),
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
    /// Where the data would live if it had not been moved.
    pub default_root: String,
    /// The locator file, which is what makes a moved root stick.
    pub locator: String,
    /// True when the data is not in [`default_root`].
    pub moved: bool,
    /// Why a saved location was ignored, so the UI can explain a reset.
    pub note: Option<String>,
    /// `std::env::consts::OS`, so the UI can explain platform limits.
    pub platform: String,
}

pub fn report() -> DataPaths {
    let root = root();
    let fallback = default_root();
    DataPaths {
        config: config_file().display().to_string(),
        profiles: root.join("profiles").display().to_string(),
        external: root.join("external").display().to_string(),
        moved: root != fallback,
        default_root: fallback.display().to_string(),
        locator: locator_file().display().to_string(),
        note: fallback_note(),
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

    /// The decision on a candidate locator, without reading the real file.
    fn resolve_for(chosen: &Path) -> (PathBuf, Option<String>) {
        resolve_from(Some(chosen.to_path_buf()))
    }

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
        // CEF only accepts a profile directory directly under its cache root.
        // Nesting it one level deeper makes Chrome refuse the profile and the
        // tab silently shares `Default`, so pin the shape here.
        assert_eq!(cef_profile("x"), cef_root().join("x"));
        assert_eq!(cef_profile("x").parent(), Some(cef_root().as_path()));
        // `Default` and `Profile-<hash>` are Chrome's own names inside a user data
        // directory; an app that mapped onto one would share the shell's jar.
        assert_eq!(cef_profile("Default"), cef_root().join("_Default"));
        assert_eq!(cef_profile("profile-abc"), cef_root().join("_profile-abc"));
        assert_eq!(profile_name("x"), "x");
        // The name reaches a filesystem path, so traversal must not survive.
        assert_eq!(profile_name("../../evil"), "______evil");
    }

    #[test]
    fn remove_profile_is_idempotent() {
        assert_eq!(remove_profile(root().join("does-not-exist-xyz")), Ok(false));
    }

    #[test]
    fn the_locator_round_trips_and_survives_a_trailing_newline() {
        let dir = std::env::temp_dir().join("rb-locator-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Nothing written yet reads as "use the default", not as an error.
        assert_eq!(read_locator_in(&dir), None);

        let target = Path::new("D:\\RustBox");
        write_locator_in(&dir, target).unwrap();
        assert_eq!(read_locator_in(&dir), Some(target.to_path_buf()));

        // The file ends with a newline; a reader that kept it would build a path
        // with a stray character on the end.
        let raw = std::fs::read_to_string(dir.join(LOCATOR)).unwrap();
        assert!(raw.ends_with('\n'));

        clear_locator_in(&dir).unwrap();
        assert_eq!(read_locator_in(&dir), None);
        // Clearing twice is not an error: the caller may retry after a crash.
        assert_eq!(clear_locator_in(&dir), Ok(()));
    }

    #[test]
    fn an_empty_locator_is_ignored_rather_than_resolved_to_nothing() {
        let dir = std::env::temp_dir().join("rb-locator-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LOCATOR), b"\n\n").unwrap();
        assert_eq!(read_locator_in(&dir), None);
    }

    #[test]
    fn a_missing_folder_falls_back_instead_of_failing_to_start() {
        // The drive may be unplugged, or the folder renamed by hand. The app has
        // to start on the default root and say why, not refuse to run.
        let (root, note) = resolve_for(Path::new("Q:\\definitely-not-there-rb"));
        assert_eq!(root, default_root());
        assert!(note.is_some_and(|n| n.contains("not there any more")));
    }

    #[test]
    fn a_relative_locator_falls_back_with_an_explanation() {
        let (root, note) = resolve_for(Path::new("rustbox-data"));
        assert_eq!(root, default_root());
        assert!(note.is_some_and(|n| n.contains("not a full path")));
    }

    #[test]
    fn a_locator_pointing_at_the_default_root_is_not_a_move() {
        let (root, note) = resolve_for(&default_root());
        assert_eq!(root, default_root());
        assert!(note.is_none());
    }

    #[test]
    fn data_store_identifier_is_stable_and_distinct() {        // Stable: the same app must map to the same store across launches, or
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
