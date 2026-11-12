// Release builds must not open a console window behind the app; debug builds
// keep it so panics and logs stay visible while developing.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ai_logs;
mod ai_router;
mod ai_stats;
mod ai_tunnel;
mod cdp;
mod chat;
mod clipboard;
mod config;
mod external;
mod favicon;
mod mcp;
mod paths;
mod proxy_auth;
mod session;
mod settings;
mod tabs;
mod webview2_skew;
mod webview_args;
mod workspace;

use config::{App, AppState};
use tauri::Emitter;

#[tauri::command]
fn list_apps(state: tauri::State<AppState>) -> Result<Vec<App>, String> {
    Ok(state.config.lock().map_err(|e| e.to_string())?.apps.clone())
}

/// Starred catalog entries (service URLs) and per-URL usage counters.
#[tauri::command]
fn get_catalog_state(
    state: tauri::State<AppState>,
) -> Result<(Vec<String>, std::collections::HashMap<String, u32>), String> {
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok((cfg.favorites.clone(), cfg.usage.clone()))
}

#[tauri::command]
fn set_favorites(favs: Vec<String>, state: tauri::State<AppState>) -> Result<(), String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    cfg.favorites = favs;
    let path = state.config_path.clone();
    cfg.save(&path)
}

/// Bump the usage counter for a catalog URL (called when a tab is created).
#[tauri::command]
fn bump_usage(url: String, state: tauri::State<AppState>) -> Result<(), String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    let entry = cfg.usage.entry(url).or_insert(0);
    *entry = entry.saturating_add(1);
    let path = state.config_path.clone();
    cfg.save(&path)
}

#[tauri::command]
fn save_apps(apps: Vec<App>, state: tauri::State<AppState>) -> Result<(), String> {
    let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
    cfg.apps = apps;
    let path = state.config_path.clone();
    let apps_now = cfg.apps.clone();
    cfg.save(&path)?;
    drop(cfg);
    let handle = state.app_handle.lock().map_err(|e| e.to_string())?;
    if let Some(h) = handle.as_ref() {
        h.emit("apps-changed", serde_json::to_value(apps_now).unwrap())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn open_devtools(main: tauri::WebviewWindow<tauri::Wry>) {
    main.open_devtools();
}
/// Put text on the clipboard.
///
/// The page tries `navigator.clipboard` first and calls this when WebView2
/// rejects it, which happens whenever the document is not focused. See
/// `clipboard.rs` for why that is a real state and not a broken click.
#[tauri::command]
fn copy_text(text: String) -> Result<(), String> {
    clipboard::write_text(&text)
}

/// Read the MCP toggle and, when on, the endpoint the agent should use.
///
/// The token is deliberately not returned: the UI only needs to know whether
/// the bridge is up and where the endpoint file is, and a token in a webview is
/// a token in reach of any script that page runs.
#[tauri::command]
fn mcp_state(state: tauri::State<AppState>) -> Result<serde_json::Value, String> {
    let (enabled, mcp) = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        (cfg.mcp_enabled, cfg.settings.mcp.clone())
    };
    let endpoint = mcp::endpoint_path();
    Ok(serde_json::json!({
        "enabled": enabled,
        "endpointFile": endpoint.to_string_lossy(),
        "logFile": mcp::log_path().to_string_lossy(),
        // The settings page prints this in the client configuration; a name the
        // user has to resolve themselves is not copy-pasteable.
        "shimPath": mcp::shim_path().to_string_lossy(),
        "running": endpoint.exists(),
        // The port actually in use, which differs from the configured one when
        // the bridge is on an ephemeral port.
        "port": mcp::current_port(),
        "configuredPort": mcp.port,
        "bind": match mcp.bind {
            settings::McpBind::Loopback => "loopback",
            settings::McpBind::Network => "network",
        },
        "scope": match mcp.scope {
            settings::McpScope::AllTabs => "all-tabs",
            settings::McpScope::ActiveTab => "active-tab",
        },
        "allowAppEdits": mcp.allow_app_edits,
        "guides": settings::mcp_guides(mcp.bind),
    }))
}

/// Turn the MCP bridge on or off and persist the choice.
///
/// Enabling starts the listener on the configured port and bind mode; disabling
/// stops accepting, drops the token file, and revokes every live connection by
/// asking the accept loop to exit. The loop is joined through [`mcp::stop`], so
/// turning it back on does not fail because the old socket still holds the port.
#[tauri::command]
fn set_mcp_enabled(
    enabled: bool,
    app_handle: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<serde_json::Value, String> {
    {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.mcp_enabled = enabled;
        let path = state.config_path.clone();
        cfg.save(&path)?;
    }

    if enabled {
        match mcp::start(app_handle) {
            Ok(port) => Ok(serde_json::json!({ "enabled": true, "port": port })),
            Err(e) => {
                // Already running is not a failure: the toggle just needs to
                // agree with reality.
                if e.contains("already running") {
                    Ok(serde_json::json!({ "enabled": true, "alreadyRunning": true }))
                } else {
                    Err(e)
                }
            }
        }
    } else {
        mcp::stop();
        Ok(serde_json::json!({ "enabled": false }))
    }
}

/// Where rustbox keeps its data, so the UI can show it to the user.
#[tauri::command]
fn data_paths() -> paths::DataPaths {
    paths::report()
}

/// The settings block, with every provider key masked.
///
/// The page has to render the provider list, so the whole structure is sent;
/// only the secret is substituted. `save_settings` recognises the mask and keeps
/// the stored key, so an untouched form cannot wipe a working credential.
#[tauri::command]
fn get_settings(state: tauri::State<AppState>) -> Result<SettingsView, String> {
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(SettingsView::of(&cfg.settings, cfg.mcp_enabled))
}

/// Persisted settings plus the derived facts the page needs to render.
#[derive(serde::Serialize)]
struct SettingsView {
    #[serde(flatten)]
    settings: settings::Settings,
    /// One row per provider with its key already masked. Named separately from
    /// `providers` because the flattened settings block already owns that key.
    provider_rows: Vec<settings::ProviderSummary>,
    /// The agent-header presets the picker offers, with the sentence explaining
    /// each one. Sent rather than hardcoded in the page so a preset the backend
    /// does not implement can never be selected.
    agent_presets: Vec<settings::AgentPresetRow>,
    env: settings::EnvFacts,
    mcp_enabled: bool,
    /// The combo strategies the page offers, with their explanations. Sent for
    /// the same reason the agent presets are: a strategy the page offers but the
    /// router does not implement would be a choice that silently does nothing.
    combo_strategies: Vec<settings::ComboStrategyRow>,
    /// Every model the router publishes right now, so the combo editor can offer
    /// them and the AI Router pane can show what is exposed.
    published: Vec<settings::PublishedRow>,
    /// The live listener: port, bind, tunnel and Tailscale, read from the
    /// process rather than from the saved settings, because the two can differ.
    ///
    /// Named `router_live` rather than `router` because the flattened settings
    /// block already owns that key, and two fields with one name would emit a
    /// duplicate JSON key — the page would read whichever survived.
    router_live: serde_json::Value,
}

impl SettingsView {
    fn of(settings: &settings::Settings, mcp_enabled: bool) -> SettingsView {
        let catalogue = ai_router::Catalogue::of(settings);
        let published = settings::published_rows(settings, &catalogue);
        SettingsView {
            settings: settings::redacted(settings),
            provider_rows: settings::provider_summaries(settings),
            agent_presets: settings::agent_presets(),
            env: settings::env_facts(),
            mcp_enabled,
            combo_strategies: settings::combo_strategies(),
            published,
            router_live: ai_router::live_state(settings),
        }    }
}

/// Persist the settings block.
///
/// Also applies the parts that are not just data: the MCP port/bind/scope take
/// effect immediately (the listener is restarted when they changed), because a
/// setting the user has to relaunch to see is a setting they will assume is
/// broken.
#[tauri::command]
fn save_settings(
    mut incoming: settings::Settings,
    app_handle: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<SettingsView, String> {
    let (previous, view) = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        // A masked or empty key means "unchanged": the page never saw the real
        // one, so writing the mask to disk would destroy the credential.
        settings::merge_secrets(&mut incoming, &cfg.settings);
        let previous = cfg.settings.clone();
        cfg.settings = incoming;
        let path = state.config_path.clone();
        cfg.save(&path)?;
        let view = SettingsView::of(&cfg.settings, cfg.mcp_enabled);
        (previous, view)
    };

    // Restart the bridge only when something it cares about changed; a restart
    // is a visible event for every connected client, so it must not happen on
    // an unrelated save (typing a new theme, adding a skill).
    let bridge_changed = previous.mcp.port != view.settings.mcp.port
        || previous.mcp.bind != view.settings.mcp.bind
        || previous.mcp.scope != view.settings.mcp.scope;
    if bridge_changed && view.mcp_enabled {
        match mcp::restart(app_handle.clone()) {
            Ok(port) => mcp::log(&format!("bridge restarted on the new settings, port {port}")),
            Err(e) => mcp::log(&format!("bridge restart failed: {e}")),
        }
    }

    // The router follows the same rule: only a change it cares about moves the
    // listener, and only while it is on. A published router whose bind or port
    // changed has to rebind, or the URL the page just printed is wrong.
    let router_changed = previous.router.port != view.settings.router.port
        || previous.router.publish != view.settings.router.publish
        || previous.router.tailscale != view.settings.router.tailscale;
    if router_changed && ai_router::is_running() {
        match ai_router::restart(app_handle.clone()) {
            Ok(port) => ai_router::log(&format!("router restarted on the new settings, port {port}")),
            Err(e) => ai_router::log(&format!("router restart failed: {e}")),
        }
    }

    // A tab's proxy is chosen when the tab is built, so a save that changes what
    // an app resolves to leaves the open tabs on the old endpoint while this page
    // reports the new one. The apps that actually changed are named here, because
    // only this side knows the precedence rules (per-app mode, then global, then
    // pool). Emitted even when the list is empty so the page can clear its state.
    {
        use tauri::Emitter;
        let changed = apps_with_changed_proxy(&previous, &view.settings, &state);
        let _ = app_handle.emit("proxy-changed", serde_json::json!({ "apps": changed }));
    }

    Ok(view)
}

/// Which open apps would now connect through a different proxy.
///
/// Compares the resolution before and after the save rather than the settings
/// diff, so an app on `custom` is not rebuilt because the global proxy moved
/// underneath it, and a re-save that changes nothing rebuilds nothing.
fn apps_with_changed_proxy(
    previous: &settings::Settings,
    current: &settings::Settings,
    state: &tauri::State<AppState>,
) -> Vec<String> {
    let Ok(cfg) = state.config.lock() else {
        return Vec::new();
    };
    changed_proxy_apps(previous, current, &cfg.apps)
}

/// The pure half of [`apps_with_changed_proxy`].
///
/// Split out so the precedence rules can be tested without an app handle: this
/// is the function that decides whether a user's Settings save reaches their
/// open tabs, and getting it wrong is invisible until someone notices the tab
/// is still on the old proxy.
fn changed_proxy_apps(
    previous: &settings::Settings,
    current: &settings::Settings,
    apps: &[crate::config::App],
) -> Vec<String> {
    let mut changed = Vec::new();
    for (index, app) in apps.iter().enumerate() {
        // External-browser apps resolve their proxy at launch time, so nothing
        // is holding the old value.
        if app.engine != crate::config::Engine::OsWebview {
            continue;
        }
        if settings::resolve(previous, app, index) != settings::resolve(current, app, index) {
            changed.push(app.id.clone());
        }
    }
    changed
}

/// Test one proxy by actually relaying a request through it.
#[tauri::command]
async fn test_proxy(uri: String) -> Result<settings::Probe, String> {
    let proxy = settings::Proxy::parse(&uri)?;
    Ok(settings::test_proxy(&proxy).await)
}

/// Test a provider by making one minimal request to it.
#[tauri::command]
async fn test_provider(
    provider: settings::Provider,
    state: tauri::State<'_, AppState>,
) -> Result<settings::Probe, String> {
    // The form may have sent a masked key back; use the stored one instead of
    // testing `****1234`, which would report a false 401.
    let mut provider = provider;
    if settings::is_masked_key(&provider.api_key) || provider.api_key.trim().is_empty() {
        let stored = state.config.lock().map_err(|e| e.to_string())?;
        if let Some(existing) = stored.settings.providers.iter().find(|p| p.id == provider.id) {
            provider.api_key = existing.api_key.clone();
        }
    }
    Ok(settings::test_provider(&provider).await)
}

/// The machine's own addresses, and its public one when the user asks for it.
///
/// Split from `get_settings` because the public lookup is an outbound request
/// that tells a third party this machine is online; it must be an explicit act,
/// not a side effect of opening a page.
#[tauri::command]
async fn network_facts(include_public: bool) -> Result<settings::EnvFacts, String> {
    let mut facts = settings::env_facts();
    if include_public {
        facts.public_address = settings::public_address().await.ok();
    }
    Ok(facts)
}

/// Turn the AI Router on or off and persist the choice.
///
/// Enabling starts the listener on the configured port; disabling stops it and
/// stops any tunnel, because a tunnel forwarding to a closed port is worse than
/// no tunnel at all — the public URL would answer 502 and look like a router bug.
#[tauri::command]
fn set_router_enabled(
    enabled: bool,
    app_handle: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<serde_json::Value, String> {
    {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.settings.router.enabled = enabled;
        let path = state.config_path.clone();
        cfg.save(&path)?;
    }
    if enabled {
        match ai_router::start(app_handle) {
            Ok(port) => Ok(serde_json::json!({ "enabled": true, "port": port })),
            Err(e) if e.contains("already running") => {
                Ok(serde_json::json!({ "enabled": true, "alreadyRunning": true }))
            }
            Err(e) => Err(e),
        }
    } else {
        ai_tunnel::stop();
        ai_router::stop();
        Ok(serde_json::json!({ "enabled": false }))
    }
}

/// Start or stop the Cloudflare tunnel in front of the router.
///
/// Refuses to start when the router is not listening: the tunnel would come up
/// and print a URL that answers nothing, which reads as a broken router rather
/// than a missing step.
#[tauri::command]
fn set_router_tunnel(enabled: bool) -> Result<ai_tunnel::TunnelState, String> {
    if !enabled {
        return Ok(ai_tunnel::stop());
    }
    let Some(port) = ai_router::current_port() else {
        return Err(
            "Start the router first: a tunnel needs a port to forward to.".to_string(),
        );
    };
    ai_tunnel::start(port)
}

/// Turn the tailnet route on or off.
#[tauri::command]
fn set_router_tailscale(
    enabled: bool,
    app_handle: tauri::AppHandle,
    state: tauri::State<AppState>,
) -> Result<ai_tunnel::TailscaleState, String> {
    let state_now = ai_tunnel::set_tailscale(enabled);
    if enabled && !state_now.enabled {
        return Ok(state_now);
    }
    {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.settings.router.tailscale = enabled;
        let path = state.config_path.clone();
        cfg.save(&path)?;
    }
    // Tailscale widens the bind, so the listener has to move for the change to
    // be real. Restarted only while the router is on: turning Tailscale on with
    // the router off just records the preference.
    if ai_router::is_running() {
        ai_router::restart(app_handle)?;
    }
    Ok(state_now)
}

/// The live router state: port, bind, tunnel, Tailscale, log size.
#[tauri::command]
fn router_state(state: tauri::State<AppState>) -> Result<serde_json::Value, String> {
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(ai_router::live_state(&cfg.settings))
}

/// The request log, newest first, with the dashboard numbers.
///
/// Two sources on purpose. `entries` is the file, bounded and clearable. `stats`
/// comes from the SQLite aggregates, so clearing the log resets the list and
/// leaves the chart alone — which is what makes "Clear log" a safe thing to
/// click. If the database cannot be read the dashboard falls back to aggregating
/// the file, which is what it did before the database existed.
#[tauri::command]
fn router_logs(state: tauri::State<AppState>) -> Result<serde_json::Value, String> {
    let (limit, enabled) = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        (
            cfg.settings.router.log.effective_limit(),
            cfg.settings.router.log.enabled,
        )
    };
    let entries = ai_logs::all();
    let stats = ai_stats::summary(ai_logs::now_ms())
        .unwrap_or_else(|| ai_logs::summarize(&entries, ai_logs::now_ms()));
    let mut newest = entries;
    newest.reverse();
    Ok(serde_json::json!({
        "entries": newest,
        // Requests being served right now. Empty most of the time, which is
        // exactly when the page stops polling.
        "live": ai_logs::in_flight(),
        "stats": stats,
        "limit": limit,
        "enabled": enabled,
        "bytes": ai_logs::size(),
        "file": ai_logs::file().to_string_lossy(),
        "stats_file": ai_stats::file().to_string_lossy(),
    }))
}

/// Just the requests being served right now.
///
/// A narrow command for the live poll: the page refreshes this every couple of
/// seconds while something is streaming, and re-reading the whole log — parse
/// every line, aggregate the dashboard — for a spinner would be work the user
/// never asked for.
#[tauri::command]
fn router_live() -> Vec<ai_logs::LiveRow> {
    ai_logs::in_flight()
}

/// Delete the request log.
///
/// The dashboard aggregates live in SQLite and are deliberately untouched: the
/// chart is a record of what the router did, not part of the list being cleared.
#[tauri::command]
fn clear_router_logs() -> Result<(), String> {
    ai_logs::clear()
}

/// A fresh API key for the router.
///
/// Generated in the backend so the secret never has to be invented in a page
/// that could be inspected; the page shows it once, then it is masked like every
/// other credential.
#[tauri::command]
fn new_router_key() -> String {
    settings::new_router_key()
}

/// Refuse a prefix that would collide or shadow the combo namespace.
///
/// Checked in the backend, not only in the page: a prefix is a routing key, and a
/// page that could be edited by hand is not where that rule belongs. The page
/// keeps a local copy of the rule for the live preview, but this answer is the
/// one that decides.
#[tauri::command]
fn check_prefix(
    provider_id: String,
    prefix: String,
    state: tauri::State<AppState>,
) -> Result<Option<String>, String> {
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(settings::prefix_problem(
        &cfg.settings,
        &provider_id,
        &prefix,
    ))
}

/// Whether this macOS build can give each app its own `WKWebsiteDataStore`.
///
/// The API needs macOS 14 (Sonoma). wry silently falls back to the shared
/// default store below that, which would let apps share a session, so the UI
/// needs to know before it claims isolation.
#[cfg(target_os = "macos")]
fn macos_has_named_data_stores() -> bool {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output();
    let Ok(out) = out else { return false };
    let version = String::from_utf8_lossy(&out.stdout);
    let major = version
        .trim()
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok());
    matches!(major, Some(v) if v >= 14)
}

#[cfg(not(target_os = "macos"))]
fn macos_has_named_data_stores() -> bool {
    // Not applicable: Windows and Linux isolate through `data_directory`.
    true
}

/// What this platform's webview engine can actually do, so the UI can explain
/// limits instead of offering options that would silently do nothing.
#[derive(serde::Serialize)]
struct PlatformInfo {
    /// Named WebView2 profiles (`ProfileMode::Light`). WKWebView and WebKitGTK
    /// have no equivalent, so the UI disables the option there rather than
    /// offering a mode that would behave like `Isolated`.
    light_profiles: bool,
    /// Per-app data stores on macOS. WKWebView ignores `data_directory` and
    /// isolates through `WKWebsiteDataStore`, which needs macOS 14+. Older
    /// systems fall back to one shared store, so apps would share a session;
    /// the UI says so rather than claiming isolation.
    named_data_stores: bool,
    /// Muting a tab uses a WebView2 API; other engines have no binding yet.
    tab_mute: bool,
}

#[tauri::command]
fn platform_info() -> PlatformInfo {
    PlatformInfo {
        light_profiles: cfg!(windows),
        named_data_stores: macos_has_named_data_stores(),
        tab_mute: cfg!(windows),
    }
}

/// Delete one app's stored session so the next open starts logged out.
///
/// Both storage modes are cleared, not just the active one: a leftover profile
/// from the other mode would silently come back if the user switched modes.
///
/// macOS stores site data in a named `WKWebsiteDataStore`, not in a folder, so
/// there is nothing here to delete. Clearing one requires WebKit's own removal
/// API, which neither Tauri nor wry exposes; rather than report a success that
/// did not happen, that case returns an explicit error.
#[tauri::command]
fn reset_app_profile(app_id: String, engine: String) -> Result<String, String> {
    if cfg!(target_os = "macos") && engine != "external" {
        return Err(
            "clearing a session is not supported on macOS yet: site data lives in a \
             WKWebsiteDataStore, not in a folder"
                .to_string(),
        );
    }
    // Every location, not just the active engine's: switching engine or storage
    // mode leaves the previous profile on disk, and clearing only the current
    // one would let a stale session come back the moment the user switches back.
    let freed = paths::purge_app_data(&app_id)?;
    Ok(if freed > 0 {
        format!("cleared {}", human_size(freed))
    } else {
        "profile was already empty".to_string()
    })
}

/// Delete everything stored for an app, including profiles left behind by an
/// earlier engine or storage mode. Returns bytes reclaimed.
#[tauri::command]
fn purge_app_data(app_id: String) -> Result<u64, String> {
    paths::purge_app_data(&app_id)
}

/// Bytes an app currently occupies, so the UI can warn before throwing a
/// session away instead of asking blind.
#[tauri::command]
fn app_data_size(app_id: String) -> u64 {
    paths::app_data_size(&app_id)
}

/// Format bytes for a user-facing sentence.
fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.0} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

fn main() {
    let config_path = paths::config_file();

    // Recorded before any webview exists: a later failure has to be compared
    // against the runtime this process actually started on, not against "now".
    webview2_skew::mark_started();

    // Freeze the runtime choice before the first webview exists. Without this,
    // every later `CreateCoreWebView2EnvironmentWithOptions` resolves the newest
    // folder on disk, so a runtime Windows installs mid-session makes the next
    // tab ask a live browser process for a version it is not running and fail
    // with RPC_E_DISCONNECTED. Pinned, tabs keep opening on the version this
    // process started with; the UI offers a restart to move to the new one.
    #[cfg(windows)]
    match webview2_skew::pin_runtime_to_startup_version() {
        Some(pin) => eprintln!(
            "[rustbox] webview2 runtime pinned to {}",
            pin.version
        ),
        None => eprintln!("[rustbox] webview2 runtime not pinned: letting the loader choose"),
    }

    tauri::Builder::default()
        .manage(AppState::new(config_path))
        .invoke_handler(tauri::generate_handler![
            list_apps,
            save_apps,
            get_catalog_state,
            set_favorites,
            bump_usage,
            data_paths,
            platform_info,
            reset_app_profile,
            purge_app_data,
            app_data_size,
            external::detect_browsers,
            external::system_default_browser,
            external::open_external,
            proxy_auth::start_proxy_auth_server,
            tabs::open_embedded_tab,
            tabs::warm_start_apps,
            tabs::close_embedded_tab,
            tabs::list_sub_tabs,
            tabs::add_sub_tab,
            tabs::close_sub_tab,
            tabs::activate_sub_tab,
            tabs::reorder_sub_tab,
            tabs::resolve_link_choice,
            tabs::dismiss_link_choice,
            tabs::tab_eval,
            tabs::tab_cdp,
            tabs::tab_console,
            tabs::tab_network,
            tabs::list_tabs,
            tabs::navigate,
            tabs::navigate_to_url,
            tabs::set_tab_muted,
            tabs::hide_all_tabs,
            tabs::ctx_menu_window,
            tabs::ctx_menu_fit,
            tabs::ctx_menu_close,
            tabs::emit_ctx_action,
            tabs::resize_tabs,
            tabs::restart_after_runtime_swap,
            favicon::fetch_favicon,
            mcp_state,
            set_mcp_enabled,
            router_state,
            set_router_enabled,
            set_router_tunnel,
            set_router_tailscale,
            router_logs,
            router_live,
            clear_router_logs,
            new_router_key,
            check_prefix,
            get_settings,
            save_settings,
            test_proxy,
            test_provider,
            network_facts,
            chat::chat_new,
            chat::chat_list,
            chat::chat_get,
            chat::chat_delete,
            chat::chat_rename,
            chat::chat_update,
            chat::chat_send,
            tabs::set_active_tab,
            tabs::activate_app,
            open_devtools,
            copy_text
        ])
        .setup(|app| {
            use tauri::Manager;
            let handle = app.handle().clone();
            let state: tauri::State<AppState> = handle.state();
            *state.app_handle.lock().unwrap() = Some(handle.clone());

            // Keep tab overlay windows glued to the main window's content area
            // whenever it is moved or resized.
            if let Some(main) = app.get_webview_window("main") {
                // `maximized: true` in tauri.conf.json is not honored at creation
                // time on Windows (the window comes up at its plain 1280x800),
                // and calling `maximize()` inside `setup` is also overridden by
                // the config size that Tauri applies right after setup returns.
                // So maximize from a fresh main-thread task once the window has
                // finished its initial show/resize.
                let target = main.clone();
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    let _ = h.run_on_main_thread(move || {
                        let _ = target.maximize();
                        let _ = target.set_focus();
                    });
                });
                let h = handle.clone();
                main.on_window_event(move |e| {
                    // Set before anything else: the quit that follows destroys the
                    // tab windows, and their teardown must not rewrite the
                    // remembered tabs with the shrinking list it leaves behind.
                    if matches!(e, tauri::WindowEvent::CloseRequested { .. }) {
                        tabs::begin_quit();
                    }
                    if tabs::trace_focus() {
                        if let tauri::WindowEvent::Focused(focused) = e {
                            tabs::trace_line(&format!("trace focus main focused={focused}"));
                        }
                    }
                    if matches!(e, tauri::WindowEvent::Moved(_))
                        || matches!(e, tauri::WindowEvent::Resized(_))
                    {
                        tabs::sync_all_tabs(&h);
                    }
                });

                // Dev hook: RB_PROBE_OPEN=1 opens every enabled app tab on
                // startup so profile isolation can be inspected from a script
                // without driving the UI with synthetic clicks. Inert unless
                // the variable is set.
                if std::env::var("RB_PROBE_OPEN").as_deref() == Ok("1") {
                    let h = handle.clone();
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(2500));
                        let h2 = h.clone();
                        let _ = h.run_on_main_thread(move || {
                            tabs::probe_open_all(&h2);
                        });
                    });
                }

                // MCP bridge: exposes the tab browsers to an external agent.
                // Off unless asked for, either by the persisted toggle or by
                // RB_MCP=1. Release builds have no console, so the outcome is
                // written to a log file as well as stderr.
                let mcp_on = std::env::var("RB_MCP").as_deref() == Ok("1") || {
                    let state: tauri::State<AppState> = handle.state();
                    state
                        .config
                        .lock()
                        .map(|c| c.mcp_enabled)
                        .unwrap_or(false)
                };
                if mcp_on {
                    let h = handle.clone();
                    std::thread::spawn(move || match mcp::start(h) {
                        Ok(port) => mcp::log(&format!("bridge listening on 127.0.0.1:{port}")),
                        Err(e) => mcp::log(&format!("bridge failed to start: {e}")),
                    });
                }

                // The AI Router: `enabled` is a persisted switch, so a router
                // the user turned on has to come back on the next launch.
                // Without this the setting was saved and then ignored, and the
                // endpoint a client was configured against was simply dead
                // after a restart with nothing on screen saying so.
                let router_on = {
                    let state: tauri::State<AppState> = handle.state();
                    state
                        .config
                        .lock()
                        .map(|c| c.settings.router.enabled)
                        .unwrap_or(false)
                };
                if router_on {
                    let h = handle.clone();
                    // Off the setup thread: binding can block briefly, and the
                    // window should paint first either way.
                    std::thread::spawn(move || match ai_router::start(h) {
                        Ok(port) => ai_router::log(&format!(
                            "router restored on launch, port {port}"
                        )),
                        Err(e) => ai_router::log(&format!("router failed to start: {e}")),
                    });
                }
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building rustbox")
        .run(|_, event| {
            // Delete the endpoint file on the way out, so the shim reports
            // "app not running" instead of dialling a closed port and waiting
            // out its read timeout.
            if matches!(event, tauri::RunEvent::Exit) {
                mcp::remove_endpoint();
                // Remembered tabs are written as they change, not on exit, so
                // this only catches a change made inside the debounce window.
                // It still matters: a clean quit is when a user expects their
                // last tab to be exactly where they left it.
                //
                // Nothing is remembered from here on: by the time `Exit` fires the
                // tab windows have been destroyed, so the registry no longer holds
                // the tabs the user closed with. `flush` writes only what was
                // already accumulated, never what teardown left behind.
                tabs::begin_quit();
                session::flush();
            }
        });
}
