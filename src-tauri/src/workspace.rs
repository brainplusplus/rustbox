//! Workspace edits shared by the UI and the MCP bridge.
//!
//! Both need to add, change and remove apps, and both must produce the same
//! result: same id scheme, same validation, same event so every open view
//! refreshes. Keeping the write path here is what stops the agent-facing copy
//! from drifting away from the one the UI uses.

use crate::config::{App, AppState, Engine, ProfileMode};
use crate::settings::{LinkTarget, Proxy, ProxyMode};
use tauri::{Emitter, Manager};

/// Read the whole app list.
pub fn list(handle: &tauri::AppHandle) -> Result<Vec<App>, String> {
    let state: tauri::State<AppState> = handle.state();
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(cfg.apps.clone())
}

/// One app by id.
pub fn get(handle: &tauri::AppHandle, app_id: &str) -> Result<App, String> {
    let state: tauri::State<AppState> = handle.state();
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    cfg
        .apps
        .iter()
        .find(|a| a.id == app_id)
        .cloned()
        .ok_or_else(|| format!("no app with id '{app_id}'"))
}

/// Persist and announce the current app list.
fn commit(handle: &tauri::AppHandle, apps: Vec<App>) -> Result<(), String> {
    let state: tauri::State<AppState> = handle.state();
    {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.apps = apps.clone();
        let path = state.config_path.clone();
        cfg.save(&path)?;
    }
    handle
        .emit("apps-changed", serde_json::to_value(&apps).unwrap_or_default())
        .map_err(|e| e.to_string())
}

/// A fresh id. Time-ordered so a list sorted by id is also ordered by creation.
pub fn new_id() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("app-{stamp:x}")
}

/// Trim a name and reject an empty one.
fn clean_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("An app needs a name.".to_string());
    }
    if name.chars().count() > 60 {
        return Err("The name is too long (max 60 characters).".to_string());
    }
    Ok(name.to_string())
}

/// Accept `example.com` as well as a full URL, and reject anything that is not
/// http(s) so a `javascript:` or `file:` URL cannot be stored as an app.
fn clean_url(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("An app needs a URL.".to_string());
    }
    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let parsed: tauri::Url = candidate
        .parse()
        .map_err(|e| format!("'{raw}' is not a valid URL: {e}"))?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed.to_string()),
        other => Err(format!(
            "'{other}' URLs cannot be used as an app. Use http or https."
        )),
    }
}

/// Fields an agent or the UI may set on an app.
///
/// Every field is optional so a patch only changes what it names; `None` means
/// "leave this alone", which is what makes editing a single property safe.
#[derive(Debug, Default, serde::Deserialize)]
pub struct AppPatch {
    pub name: Option<String>,
    pub url: Option<String>,
    pub icon: Option<String>,
    pub engine: Option<Engine>,
    pub external_browser_path: Option<String>,
    pub profile_mode: Option<ProfileMode>,
    pub fingerprint: Option<String>,
    pub external_args: Option<Vec<String>>,
    pub proxy_mode: Option<ProxyMode>,
    /// `Some(None)` clears the proxy; `None` leaves it alone. A plain
    /// `Option<Proxy>` could not express that difference.
    pub proxy: Option<Option<Proxy>>,
    pub user_agent: Option<String>,
    /// Where a link that asks for a new window goes.
    pub link_target: Option<LinkTarget>,
    pub enabled: Option<bool>,
    pub muted: Option<bool>,
    pub notifications: Option<bool>,
}

impl AppPatch {
    /// Apply this patch to an app.
    pub fn apply(self, app: &mut App) -> Result<(), String> {
        if let Some(name) = self.name {
            app.name = clean_name(&name)?;
        }
        if let Some(url) = self.url {
            app.url = clean_url(&url)?;
        }
        if let Some(icon) = self.icon {
            app.icon = if icon.trim().is_empty() {
                None
            } else {
                Some(icon)
            };
        }
        if let Some(engine) = self.engine {
            app.engine = engine;
        }
        if let Some(path) = self.external_browser_path {
            app.external_browser_path = if path.trim().is_empty() {
                None
            } else {
                Some(path)
            };
        }
        if let Some(mode) = self.profile_mode {
            app.profile_mode = mode;
        }
        if let Some(seed) = self.fingerprint {
            app.fingerprint = if seed.trim().is_empty() {
                None
            } else {
                Some(seed)
            };
        }
        if let Some(args) = self.external_args {
            app.external_args = args;
        }
        if let Some(mode) = self.proxy_mode {
            app.proxy_mode = mode;
        }
        if let Some(proxy) = self.proxy {
            // A proxy that cannot be used is stored as "no proxy" rather than as
            // a half-filled record that silently does nothing later.
            app.proxy = proxy.filter(|p| p.is_usable());
            // Typing a proxy is the user saying "use this one"; leaving the mode
            // on `inherit` would silently ignore it.
            if app.proxy.is_some() && app.proxy_mode == ProxyMode::Inherit {
                app.proxy_mode = ProxyMode::Custom;
            }
        }
        if let Some(ua) = self.user_agent {
            app.user_agent = if ua.trim().is_empty() {
                None
            } else {
                Some(ua)
            };
        }
        if let Some(target) = self.link_target {
            app.link_target = target;
        }
        if let Some(enabled) = self.enabled {
            app.enabled = enabled;
        }
        if let Some(muted) = self.muted {
            app.muted = muted;
        }
        if let Some(notifications) = self.notifications {
            app.notifications = notifications;
        }
        Ok(())
    }
}

/// Create an app from a patch. Name and URL are required.
pub fn add(handle: &tauri::AppHandle, patch: AppPatch) -> Result<App, String> {
    let name = clean_name(patch.name.as_deref().unwrap_or_default())?;
    let url = clean_url(patch.url.as_deref().unwrap_or_default())?;
    let mut app = App {
        id: new_id(),
        name,
        url,
        icon: None,
        engine: Engine::OsWebview,
        external_browser_path: None,
        session: None,
        profile_mode: ProfileMode::platform_default(),
        fingerprint: None,
        external_args: Vec::new(),
        proxy_mode: ProxyMode::Inherit,
        proxy: None,
        user_agent: None,
        link_target: Default::default(),
        enabled: true,
        muted: false,
        notifications: true,
    };
    // The patch carries name and url too; applying it keeps every other field
    // validated by the same code path an edit uses.
    patch.apply(&mut app)?;
    let mut apps = list(handle)?;
    apps.push(app.clone());
    commit(handle, apps)?;
    Ok(app)
}

/// Change one app. An unknown id is an error, not a silent no-op.
pub fn update(handle: &tauri::AppHandle, app_id: &str, patch: AppPatch) -> Result<App, String> {
    let mut apps = list(handle)?;
    let index = apps
        .iter()
        .position(|a| a.id == app_id)
        .ok_or_else(|| format!("no app with id '{app_id}'"))?;
    // What the open tab was built with, captured before the patch lands. The
    // proxy and the storage mode are both fixed when a webview is created, so a
    // change to either leaves the tab on the old one while the config says
    // otherwise — the tab then looks broken for a reason nothing on screen
    // explains.
    let before_proxy = proxy_of(handle, &apps[index], index);
    let before_profile = apps[index].profile_mode;
    let before_engine = apps[index].engine.clone();
    patch.apply(&mut apps[index])?;
    let updated = apps[index].clone();
    let after_proxy = proxy_of(handle, &updated, index);
    commit(handle, apps)?;
    if before_proxy != after_proxy
        || before_profile != updated.profile_mode
        || before_engine != updated.engine
    {
        // The next open rebuilds with the new settings; leaving the old window
        // up would keep serving the previous proxy.
        let _ = crate::tabs::close_embedded_tab(app_id.to_string(), handle.clone());
    }
    Ok(updated)
}

/// The proxy an app resolves to right now, as a comparable value.
fn proxy_of(handle: &tauri::AppHandle, app: &App, index: usize) -> crate::settings::Resolved {
    use tauri::Manager;
    let state: tauri::State<crate::config::AppState> = handle.state();
    match state.config.lock() {
        Ok(cfg) => crate::settings::resolve(&cfg.settings, app, index),
        Err(_) => crate::settings::Resolved::Direct,
    }
}

/// Remove an app and close its tab.
pub fn delete(handle: &tauri::AppHandle, app_id: &str) -> Result<(), String> {
    let mut apps = list(handle)?;
    let before = apps.len();
    apps.retain(|a| a.id != app_id);
    if apps.len() == before {
        return Err(format!("no app with id '{app_id}'"));
    }
    commit(handle, apps)?;
    // The tab is a separate window; leaving it open would keep a deleted app
    // visible and usable until the next launch.
    let _ = crate::tabs::close_embedded_tab(app_id.to_string(), handle.clone());
    Ok(())
}

/// Reorder the app list, keeping every id exactly once.
///
/// Used by drag-and-drop. Ids in `order` that do not exist are ignored, and apps
/// missing from it keep their relative position at the end, so a stale order
/// from a client cannot delete an app.
pub fn reorder(handle: &tauri::AppHandle, order: &[String]) -> Result<Vec<App>, String> {
    let apps = list(handle)?;
    let mut reordered: Vec<App> = Vec::with_capacity(apps.len());
    for id in order {
        if let Some(pos) = apps.iter().position(|a| &a.id == id) {
            let app = apps[pos].clone();
            if !reordered.iter().any(|a| a.id == app.id) {
                reordered.push(app);
            }
        }
    }
    for app in apps {
        if !reordered.iter().any(|a| a.id == app.id) {
            reordered.push(app);
        }
    }
    commit(handle, reordered.clone())?;
    Ok(reordered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_becomes_https() {
        assert_eq!(clean_url("example.com").unwrap(), "https://example.com/");
        assert_eq!(
            clean_url("http://example.com/x").unwrap(),
            "http://example.com/x"
        );
    }

    #[test]
    fn a_dangerous_scheme_is_refused() {
        // Storing `javascript:` or `file:` as an app URL would give a tab a way
        // to run code in a context the user did not choose.
        assert!(clean_url("javascript:alert(1)").is_err());
        assert!(clean_url("file:///C:/Windows").is_err());
        assert!(clean_url("data:text/html,<b>x</b>").is_err());
    }

    #[test]
    fn a_name_cannot_be_blank_or_absurdly_long() {
        assert!(clean_name("   ").is_err());
        assert!(clean_name(&"x".repeat(61)).is_err());
        assert_eq!(clean_name("  Mail ").unwrap(), "Mail");
    }

    #[test]
    fn a_patch_only_touches_what_it_names() {
        let mut app = App {
            id: "a".to_string(),
            name: "Old".to_string(),
            url: "https://old.example".to_string(),
            icon: Some("keep".to_string()),
            engine: Engine::OsWebview,
            external_browser_path: None,
            session: None,
            profile_mode: ProfileMode::Isolated,
            fingerprint: None,
            external_args: Vec::new(),
            proxy_mode: ProxyMode::Inherit,
            proxy: None,
            user_agent: None,
            link_target: Default::default(),
            enabled: true,
            muted: false,
            notifications: true,
        };
        AppPatch {
            name: Some("New".to_string()),
            ..AppPatch::default()
        }
        .apply(&mut app)
        .unwrap();
        assert_eq!(app.name, "New");
        assert_eq!(app.url, "https://old.example");
        assert_eq!(app.icon.as_deref(), Some("keep"));
    }

    #[test]
    fn setting_a_proxy_switches_the_mode_to_custom() {
        // Otherwise the proxy would be stored and then ignored, because
        // `inherit` follows the global setting.
        let mut app = App {
            id: "a".to_string(),
            name: "A".to_string(),
            url: "https://a.example".to_string(),
            icon: None,
            engine: Engine::OsWebview,
            external_browser_path: None,
            session: None,
            profile_mode: ProfileMode::Isolated,
            fingerprint: None,
            external_args: Vec::new(),
            proxy_mode: ProxyMode::Inherit,
            proxy: None,
            user_agent: None,
            link_target: Default::default(),
            enabled: true,
            muted: false,
            notifications: true,
        };
        AppPatch {
            proxy: Some(Some(Proxy {
                scheme: Some("http".to_string()),
                host: "10.0.0.1".to_string(),
                port: 8080,
                username: None,
                password: None,
            })),
            ..AppPatch::default()
        }
        .apply(&mut app)
        .unwrap();
        assert_eq!(app.proxy_mode, ProxyMode::Custom);
        assert!(app.proxy.is_some());
    }

    #[test]
    fn an_unusable_proxy_is_stored_as_none() {
        let mut app = App {
            id: "a".to_string(),
            name: "A".to_string(),
            url: "https://a.example".to_string(),
            icon: None,
            engine: Engine::OsWebview,
            external_browser_path: None,
            session: None,
            profile_mode: ProfileMode::Isolated,
            fingerprint: None,
            external_args: Vec::new(),
            proxy_mode: ProxyMode::Inherit,
            proxy: None,
            user_agent: None,
            link_target: Default::default(),
            enabled: true,
            muted: false,
            notifications: true,
        };
        AppPatch {
            proxy: Some(Some(Proxy {
                scheme: Some("http".to_string()),
                host: "   ".to_string(),
                port: 8080,
                username: None,
                password: None,
            })),
            ..AppPatch::default()
        }
        .apply(&mut app)
        .unwrap();
        assert!(app.proxy.is_none());
    }

    /// Editing a proxy must resolve to something different, because that is
    /// what makes `update` close the tab. Without the comparison a user who
    /// switched proxies kept browsing through the old one: the config file said
    /// the new endpoint while the live webview still held the old
    /// `--proxy-server`, which reads as "the proxy is ignored".
    #[test]
    fn editing_a_proxy_changes_the_resolved_value() {
        let mut app = App {
            id: "a".to_string(),
            name: "A".to_string(),
            url: "https://a.example".to_string(),
            icon: None,
            engine: Engine::OsWebview,
            external_browser_path: None,
            session: None,
            profile_mode: ProfileMode::Isolated,
            fingerprint: None,
            external_args: Vec::new(),
            proxy_mode: ProxyMode::Inherit,
            proxy: None,
            user_agent: None,
            link_target: Default::default(),
            enabled: true,
            muted: false,
            notifications: true,
        };
        let settings = crate::settings::Settings::default();

        let before = crate::settings::resolve(&settings, &app, 0);
        AppPatch {
            proxy: Some(Some(Proxy {
                scheme: Some("http".to_string()),
                host: "1.1.1.1".to_string(),
                port: 8080,
                username: None,
                password: None,
            })),
            ..AppPatch::default()
        }
        .apply(&mut app)
        .unwrap();
        let after = crate::settings::resolve(&settings, &app, 0);
        assert_ne!(before, after, "a new proxy must resolve differently");

        // Same proxy, typed again: nothing to rebuild.
        let resaved = crate::settings::resolve(&settings, &app, 0);
        assert_eq!(after, resaved);

        // Clearing it resolves back to direct, which is also a change.
        AppPatch {
            proxy: Some(None),
            proxy_mode: Some(ProxyMode::None),
            ..AppPatch::default()
        }
        .apply(&mut app)
        .unwrap();
        assert_ne!(
            after,
            crate::settings::resolve(&settings, &app, 0),
            "clearing the proxy must resolve differently too"
        );
    }
}
