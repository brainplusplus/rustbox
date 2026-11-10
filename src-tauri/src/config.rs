use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// Rendering engine for an app tab.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// Embedded OS webview (WebView2 / WKWebView / WebKitGTK). Default.
    #[default]
    OsWebview,
    /// Launch in an external browser (Brave, Chrome, Firefox, OS default...).
    External,
}

/// How an embedded tab isolates its site data.
///
/// Both modes keep cookies, localStorage and IndexedDB private to the app; they
/// differ in how much machinery is duplicated to achieve it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileMode {
    /// A: a dedicated WebView2 environment per app (`data_directory`). Each app
    /// gets its own browser process tree, so crashes, GPU state and DRM are
    /// isolated too. Costs roughly 2x the memory of `light`.
    #[default]
    Isolated,
    /// B: a named profile inside the shared WebView2 environment
    /// (`profile_name`). Site data is still separate, but the browser runtime,
    /// GPU process and crash domain are shared. Windows only; other platforms
    /// fall back to `isolated`.
    Light,
}

impl ProfileMode {
    /// The mode a new app starts on.
    ///
    /// `Light` wherever the engine can give it a named profile, because sharing
    /// one browser process is what most sites want and the isolation it gives up
    /// (a crash or GPU state taking the tab down with the shell) is not something
    /// a chat window asks for. Elsewhere there is no Light to choose, so the
    /// answer is the mode that exists.
    ///
    /// Deliberately not `Default`: `#[serde(default)]` on `App::profile_mode`
    /// reads this value for a config written before modes existed, and those
    /// apps were isolated. Changing the default would silently re-open every one
    /// of them in the shared environment.
    pub fn platform_default() -> Self {
        if cfg!(windows) {
            Self::Light
        } else {
            Self::Isolated
        }
    }
}

/// Proxy lives in `settings` now that it is shared by the global setting, the
/// pools and per-app overrides. Re-exported here so existing `config::Proxy`
/// paths keep working.
pub use crate::settings::{LinkTarget, Proxy, ProxyMode, Settings};

/// An app entry: one tab in the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct App {
    pub id: String,
    pub name: String,
    pub url: String,
    pub icon: Option<String>,
    #[serde(default)]
    pub engine: Engine,
    /// Path to browser executable when engine == External. None => OS default browser.
    #[serde(default)]
    pub external_browser_path: Option<String>,
    /// Isolated storage partition id.
    #[serde(default)]
    pub session: Option<String>,
    /// Storage isolation strategy for embedded tabs. Missing field = `isolated`
    /// (back-compat: configs written before profiles existed).
    #[serde(default)]
    pub profile_mode: ProfileMode,
    /// Fingerprint seed for anti-detect external browsers (Camoufox,
    /// CloakBrowser). Empty/None lets the browser pick its own.
    #[serde(default)]
    pub fingerprint: Option<String>,
    /// Extra command-line arguments appended when launching the external
    /// browser, one flag per string.
    #[serde(default)]
    pub external_args: Vec<String>,
    /// How this app decides its proxy. Missing field = `inherit` (back-compat:
    /// an app saved before global proxies existed had no proxy unless one was
    /// typed in, which the v4 migration turns into `custom`).
    #[serde(default)]
    pub proxy_mode: ProxyMode,
    #[serde(default)]
    pub proxy: Option<Proxy>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Where a link that asks for a new window goes. Missing field = `same`,
    /// which follows the link in the app's own tab.
    #[serde(default)]
    pub link_target: LinkTarget,
    /// Runtime toggles (persisted). Missing field = enabled (back-compat with old configs).
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub muted: bool,
    #[serde(default = "default_true")]
    pub notifications: bool,
}

fn default_true() -> bool {
    true
}

/// Latest config schema version.
///
/// v2 healed `enabled: false` written by the pre-2.0 default bug.
/// v3 introduced per-app proxy + fingerprint/args.
/// v4 moved the proxy decision to `proxy_mode` and added the settings block.
/// v5 changed what the default link target means: `same` used to be the default
///    and it silently replaced the page a link was clicked from. `ask` is the
///    default now, so a stored `same` is the old default rather than a choice
///    and is moved with it.
const CURRENT_VERSION: u32 = 5;

/// Whole workspace config persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub apps: Vec<App>,
    /// Catalog entries the user starred, keyed by their service URL. These are
    /// catalog presets (not open tabs), so they survive app deletion.
    #[serde(default)]
    pub favorites: Vec<String>,
    /// Catalog URL -> number of times a tab was created from it. Drives the
    /// "Frequently used" section so the catalog adapts to real usage.
    #[serde(default)]
    pub usage: std::collections::HashMap<String, u32>,
    /// Schema version for one-way migrations. v2: heal `enabled: false`
    /// written by the pre-2.0 default bug (missing field => false).
    #[serde(default)]
    pub version: u32,
    /// Expose the tab browsers to a local AI agent over the MCP bridge.
    ///
    /// Defaults to off. This is a capability grant, not a convenience setting:
    /// while it is on, any program that can read the endpoint file can drive
    /// the user's logged-in tabs. It is opt-in per install and can be revoked
    /// by turning it off, which stops the bridge and deletes the token.
    #[serde(default)]
    pub mcp_enabled: bool,
    /// Everything the Settings page edits: global proxy, pools, providers, MCP
    /// bridge options, skills, theme and chat preferences.
    #[serde(default)]
    pub settings: Settings,
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Config, String> {
        if !path.exists() {
            let cfg = Config {
                version: CURRENT_VERSION,
                ..Config::default()
            };
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        // Strip a UTF-8 BOM: editors and PowerShell's `Set-Content` add one, and
        // `serde_json` rejects it. Without this the whole app list silently
        // loads as empty.
        let raw = raw.trim_start_matches('\u{feff}');
        let mut cfg: Config = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if cfg.version < CURRENT_VERSION {
            if cfg.version < 2 {
                // Apps saved before the `enabled` field existed were loaded
                // with a false default and persisted back as disabled.
                // Heal: re-enable everything once.
                for app in cfg.apps.iter_mut() {
                    app.enabled = true;
                }
            }
            if cfg.version < 4 {
                // An app that carried its own proxy was, in effect, "custom":
                // nothing global existed to inherit from. Recording that keeps
                // it behaving identically once a global proxy can be set, and
                // stops it silently picking up a global tunnel the user never
                // intended for it.
                for app in cfg.apps.iter_mut() {
                    let has_proxy = app
                        .proxy
                        .as_ref()
                        .is_some_and(|p| p.is_usable());
                    app.proxy_mode = if has_proxy {
                        ProxyMode::Custom
                    } else {
                        ProxyMode::Inherit
                    };
                }
            }
            if cfg.version < 5 {
                // `same` was the default before the chooser existed, so every
                // stored `same` is either that default or a click on the only
                // other radio in a two-option form. Both are better served by
                // asking: the old behaviour replaced the page a link was
                // clicked from with no way to get it back. The chooser has a
                // "remember" box, so a user who really wants "in this tab"
                // re-teaches it on the first click.
                for app in cfg.apps.iter_mut() {
                    if app.link_target == LinkTarget::Same {
                        app.link_target = LinkTarget::Ask;
                    }
                }
            }
            cfg.version = CURRENT_VERSION;
            let _ = cfg.save(path);
        }
        // Invariants rather than schema: a stored value that cannot mean what it
        // says is repaired on every load, whether or not the version moved.
        // Persist the repair, otherwise the file keeps the bad value and every
        // later reader starts from it again.
        if cfg.settings.normalize() {
            let _ = cfg.save(path);
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        // Write via a temp file + rename so a crash mid-write cannot leave a
        // truncated config behind (which would silently reset the workspace).
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())
    }
}

/// Shared app state: persisted config + handle for emitting events.
pub struct AppState {
    pub config: Mutex<Config>,
    pub config_path: PathBuf,
    pub app_handle: Mutex<Option<tauri::AppHandle>>,
}

impl AppState {
    pub fn new(config_path: PathBuf) -> Self {
        // A load failure used to fall back to an empty config silently, which
        // presents as "all my apps vanished". Keep the fallback (the app must
        // still start) but make the reason loud and leave the bad file alone so
        // the user can recover it.
        let config = match Config::load(&config_path) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "[rustbox] could not read {}: {e} — starting with an empty workspace",
                    config_path.display()
                );
                Config::default()
            }
        };
        Self {
            config: Mutex::new(config),
            config_path,
            app_handle: Mutex::new(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(scheme: Option<&str>) -> Proxy {
        Proxy {
            scheme: scheme.map(str::to_string),
            host: "10.0.0.1".to_string(),
            port: 8080,
            username: None,
            password: None,
        }
    }

    #[test]
    fn proxy_scheme_is_normalized_for_the_engines() {
        // WebView2 and wry only parse http and socks5. Anything else has to be
        // mapped, or the webview is built with no proxy at all and the failure
        // looks like "the proxy setting is ignored".
        assert_eq!(proxy(None).normalized_scheme(), "http");
        assert_eq!(proxy(Some("http")).normalized_scheme(), "http");
        assert_eq!(proxy(Some("HTTP")).normalized_scheme(), "http");
        assert_eq!(proxy(Some("https")).normalized_scheme(), "http");
        assert_eq!(proxy(Some("socks5")).normalized_scheme(), "socks5");
        assert_eq!(proxy(Some("socks")).normalized_scheme(), "socks5");
        assert_eq!(proxy(Some("SOCKS5")).normalized_scheme(), "socks5");
        assert_eq!(proxy(Some("garbage")).normalized_scheme(), "http");
    }

    #[test]
    fn a_broken_chat_block_is_repaired_on_disk_not_only_in_memory() {
        // The live config really did carry history_limit 0 after the deploy,
        // which means the repair has to reach the file: healing a value in
        // memory leaves the bad one for the next launch to read back.
        let dir = std::env::temp_dir().join("rustbox-normalize-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let broken = r#"{
            "version": 4,
            "apps": [],
            "settings": {
                "chat": {
                    "provider_id": null,
                    "model": "",
                    "tools_enabled": false,
                    "use_skills": false,
                    "history_limit": 0
                }
            }
        }"#;
        std::fs::write(&path, broken).unwrap();

        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.settings.chat.history_limit, 24);

        let on_disk = std::fs::read_to_string(&path).unwrap();
        let reparsed: Config = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(
            reparsed.settings.chat.history_limit, 24,
            "the repair was not written back to disk"
        );
        assert!(reparsed.settings.chat.tools_enabled);

        // A config that is already healthy must not be rewritten on load.
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let again = Config::load(&path).unwrap();
        assert_eq!(again.settings.chat.history_limit, 24);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before,
            "a healthy config was rewritten for nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
