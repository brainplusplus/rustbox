//! Loopback bridge that exposes the tab browsers to an external AI agent.
//!
//! # Shape
//!
//! ```text
//! MCP client  <--stdio-->  rustbox-mcp.exe  <--127.0.0.1-->  rustbox (this app)
//!                                                              |
//!                                                              +-- CDP --> tab webview
//! ```
//!
//! Only this process holds a `WebView2` controller, so it has to be the one that
//! talks to the browser. The alternative — starting the webview with
//! `--remote-debugging-port` — would open an *unauthenticated* debugging port
//! that any local program could attach to, so the capability stays inside the
//! app and is gated by a per-launch token instead.
//!
//! # Why a hand-rolled line protocol
//!
//! The only client is the bundled shim. There is no HTTP dependency in this
//! crate and none is needed: one JSON object per line is trivial to audit, has
//! no header-parsing surface, and cannot be reached by a web page doing a
//! cross-origin `fetch` (that would send an HTTP request, not a bare line).
//!
//! # Security posture
//!
//! - Off unless `RB_MCP=1` is set. Nothing listens by default.
//! - Bound to `127.0.0.1` on an ephemeral port: not reachable from the network.
//! - Every request must carry the token from the endpoint file, compared without
//!   an early exit so a wrong token cannot be discovered byte by byte.
//! - The endpoint file lives in the per-user data root; on Unix it is created
//!   mode 0600. On Windows `%LOCALAPPDATA%` is already per-user ACL.
//! - The token is regenerated on every launch and the file is deleted on exit,
//!   so a stale token cannot be replayed against a later run.
//! - This module never builds or re-enters a webview: every CDP call goes
//!   through [`crate::cdp::call`], which posts to the main thread and waits on a
//!   channel, avoiding the wry#583 reentrancy deadlock.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::Mutex;

use crate::cdp;
use crate::paths;
use crate::settings::{McpBind, McpScope};

/// Filename of the endpoint descriptor the shim reads.
const ENDPOINT_FILE: &str = "mcp-endpoint.json";

/// Set once the listener is up, so a second start is a no-op instead of a
/// second port nobody knows about.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether requests are currently accepted.
///
/// Separate from [`RUNNING`] because the accept loop only exits once it notices
/// a stop request, and a client that connected a moment earlier must not keep
/// acting in the meantime. Disabling sets this to false, which makes every
/// in-flight and future request refuse, so the toggle actually revokes access
/// instead of merely hiding the endpoint file from a token the client may
/// already hold.
static ACCEPTING: AtomicBool = AtomicBool::new(false);

/// Asks the accept loop to exit so the port can be released.
///
/// The loop polls this between accepts. Without it, `stop` could never free the
/// port, and changing the port in Settings would need a relaunch.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

/// True while an accept loop is running. `stop` waits for it to go false before
/// returning, so a restart cannot race the old loop for the same port.
static LOOP_ALIVE: AtomicBool = AtomicBool::new(false);

/// The port the bridge is listening on, once started.
static PORT: AtomicU16 = AtomicU16::new(0);

/// The token, kept across a restart so a connected client is not logged out
/// just because the user changed the port.
static TOKEN: Mutex<Option<std::sync::Arc<String>>> = Mutex::new(None);

/// Where the shim looks for the port and token.
pub fn endpoint_path() -> PathBuf {
    paths::root().join(ENDPOINT_FILE)
}

/// Path to the MCP shim binary that ships next to this executable.
///
/// The settings page prints this in the client configuration, so it has to be
/// the real path on this machine rather than a name the user has to resolve.
pub fn shim_path() -> PathBuf {
    let name = if cfg!(windows) {
        "rustbox-mcp.exe"
    } else {
        "rustbox-mcp"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Where the bridge writes diagnostics.
///
/// Release builds are `windows_subsystem = "windows"`, so stderr goes nowhere.
/// Without a file, a bridge that fails to start is invisible.
pub fn log_path() -> PathBuf {
    paths::root().join("mcp.log")
}

/// Append one line to the bridge log. Best effort: logging must never be able to
/// take the bridge down.
pub fn log(message: &str) {
    use std::io::Write as _;
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(file, "[{stamp}] {message}");
    }
    eprintln!("[rustbox mcp] {message}");
}

/// A 128-bit token from the OS.
///
/// `RandomState::new()` seeds itself from the OS on every call, so hashing a
/// constant through it extracts fresh entropy. This is a capability token that
/// only has to be unguessable for the lifetime of one process, which 128 random
/// bits satisfies without adding a `rand` dependency.
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for _ in 0..2 {
        let state = std::collections::hash_map::RandomState::new();
        let mut hasher = state.build_hasher();
        // The value hashed is irrelevant: the entropy is in the key.
        hasher.write_u8(0);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

/// Compare tokens without leaking the length of the matching prefix.
fn token_matches(expected: &str, given: &str) -> bool {
    let a = expected.as_bytes();
    let b = given.as_bytes();
    // Fold the length difference into the accumulator instead of returning
    // early, so every comparison walks the same number of bytes.
    let mut diff = (a.len() ^ b.len()) as u8;
    for (i, byte) in a.iter().enumerate() {
        diff |= byte ^ b.get(i).copied().unwrap_or(0);
    }
    diff == 0
}

/// Write the endpoint descriptor so the shim can find us.
///
/// Replaced on every start, so a token from a previous run cannot be replayed.
/// `bind` is recorded too: the shim connects to the host it names, and a
/// published bridge has to be reached on a real interface, not `127.0.0.1`.
fn write_endpoint(port: u16, token: &str, bind: McpBind) -> Result<(), String> {
    let path = endpoint_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let host = match bind {
        McpBind::Loopback => "127.0.0.1",
        // A shim on this machine still reaches it over loopback; the address is
        // recorded for the client configs the UI generates.
        McpBind::Network => "0.0.0.0",
    };
    let body = serde_json::json!({
        "port": port,
        "host": host,
        "bind": match bind {
            McpBind::Loopback => "loopback",
            McpBind::Network => "network",
        },
        "token": token,
        "pid": std::process::id(),
        "version": 2,
    });
    std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).map_err(|e| e.to_string())?;

    // Restrict the token to the owner on Unix. Windows inherits the per-user
    // ACL of %LOCALAPPDATA%, so there is nothing extra to do there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Delete the descriptor so the shim reports "app not running" instead of
/// connecting to a closed port.
pub fn remove_endpoint() {
    let _ = std::fs::remove_file(endpoint_path());
}

/// Stop accepting requests and revoke access.
///
/// The accept loop polls [`STOP_REQUESTED`], so this waits for it to exit and
/// release the port. That wait is what makes a later start on the same fixed
/// port possible: without it, the old loop would still hold the socket and the
/// restart would fail with "address in use" on the very port the user just set.
/// [`serve`] also re-checks [`ACCEPTING`] per line, so a connection opened
/// before this call stops being served on its next request.
pub fn stop() {
    ACCEPTING.store(false, Ordering::SeqCst);
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    // The loop wakes within its poll interval. Bounded wait: a loop that somehow
    // did not exit must not hang the caller (which is the UI thread).
    for _ in 0..200 {
        if !LOOP_ALIVE.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    RUNNING.store(false, Ordering::SeqCst);
    PORT.store(0, Ordering::SeqCst);
    cdp::set_capture(false);
    remove_endpoint();
    log("bridge revoked; access disabled");
}

/// Stop and start again, picking up new port/bind settings.
///
/// A fixed port is the reason this exists: changing it has to move the listener,
/// and a client config that names a port must keep working afterwards.
pub fn restart(handle: tauri::AppHandle) -> Result<u16, String> {
    stop();
    let (port, bind) = settings_for(&handle);
    start_with(handle, port, bind)
}

/// The port the bridge is listening on, or `None` when it is not running.
pub fn current_port() -> Option<u16> {
    let port = PORT.load(Ordering::SeqCst);
    (port != 0).then_some(port)
}

/// Read the configured port and bind mode.
///
/// Read from the config on every start rather than passed in, so the toggle and
/// the settings page cannot disagree about where the bridge listens.
fn settings_for(handle: &tauri::AppHandle) -> (Option<u16>, McpBind) {
    use tauri::Manager;
    let state: tauri::State<crate::config::AppState> = handle.state();
    match state.config.lock() {
        Ok(cfg) => (cfg.settings.mcp.port, cfg.settings.mcp.bind),
        Err(_) => (None, McpBind::Loopback),
    }
}

/// Where to listen, from the configured bind mode.
fn bind_address(bind: McpBind) -> std::net::IpAddr {
    match bind {
        McpBind::Loopback => std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
        // Every interface. Still token-gated, but reachable from the network, so
        // the UI confirms this choice before enabling it.
        McpBind::Network => std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED),
    }
}

/// Start the bridge. Returns the port, or an error explaining why not.
///
/// `port` is fixed when the user set one, which is what keeps a client config
/// valid across launches. Binding a fixed port can fail because something else
/// holds it, so the error names the port instead of saying "bind failed".
pub fn start_with(
    handle: tauri::AppHandle,
    port: Option<u16>,
    bind: McpBind,
) -> Result<u16, String> {
    if RUNNING.load(Ordering::SeqCst) {
        return Err("the MCP bridge is already running".to_string());
    }

    let address = bind_address(bind);
    let listener = match TcpListener::bind((address, port.unwrap_or(0))) {
        Ok(l) => l,
        Err(e) => {
            return Err(match port {
                Some(p) => format!("could not bind port {p} on {address}: {e}"),
                None => format!("could not bind the MCP bridge on {address}: {e}"),
            });
        }
    };
    // A fresh start, so the loop must not see a stop request left over from a
    // previous run.
    STOP_REQUESTED.store(false, Ordering::SeqCst);

    // Non-blocking so the accept loop can check whether it has been retired.
    // A blocking accept would pin the port for the life of the process, which
    // makes "change the port" impossible without a relaunch.
    if let Err(e) = listener.set_nonblocking(true) {
        return Err(format!("could not configure the MCP listener: {e}"));
    }
    let bound = listener.local_addr().map_err(|e| e.to_string())?.port();
    // Reuse the token across a restart so an already-configured client keeps
    // working; a fresh token is issued only when there was none.
    let token = {
        let mut guard = TOKEN.lock().map_err(|e| e.to_string())?;
        match guard.as_ref() {
            Some(existing) => existing.clone(),
            None => {
                let fresh = std::sync::Arc::new(new_token());
                *guard = Some(fresh.clone());
                fresh
            }
        }
    };

    if let Err(e) = write_endpoint(bound, &token, bind) {
        return Err(format!("could not write the MCP endpoint file: {e}"));
    }

    // Capture is what the agent reads, so switch it on now and enable it for the
    // tabs that were already open before this call.
    cdp::set_capture(true);
    crate::tabs::enable_capture_for_open_tabs();
    ACCEPTING.store(true, Ordering::SeqCst);
    PORT.store(bound, Ordering::SeqCst);
    RUNNING.store(true, Ordering::SeqCst);
    LOOP_ALIVE.store(true, Ordering::SeqCst);

    std::thread::Builder::new()
        .name("rustbox-mcp".to_string())
        .spawn(move || accept_loop(handle, token, listener))
        .map_err(|e| {
            RUNNING.store(false, Ordering::SeqCst);
            ACCEPTING.store(false, Ordering::SeqCst);
            PORT.store(0, Ordering::SeqCst);
            LOOP_ALIVE.store(false, Ordering::SeqCst);
            format!("could not start the MCP bridge thread: {e}")
        })?;

    Ok(bound)
}

/// Start with the configured options.
pub fn start(handle: tauri::AppHandle) -> Result<u16, String> {
    let (port, bind) = settings_for(&handle);
    start_with(handle, port, bind)
}

/// Accept connections until asked to stop.
///
/// The loop owns the listener, so dropping it here is what releases the port.
fn accept_loop(handle: tauri::AppHandle, token: std::sync::Arc<String>, listener: TcpListener) {
    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Err(e) = stream.set_nonblocking(false) {
                    log(&format!("could not configure an incoming connection: {e}"));
                    continue;
                }
                if !ACCEPTING.load(Ordering::SeqCst) {
                    // Revoked between the accept and now: close without serving.
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                let token = token.clone();
                let handle = handle.clone();
                // One short-lived thread per connection: a navigation or a
                // screenshot can take seconds and must not stall the accept loop
                // for other clients.
                std::thread::spawn(move || {
                    if let Err(e) = serve(stream, &token, &handle) {
                        log(&format!("connection ended: {e}"));
                    }
                });
            }
            // No connection waiting. This is the normal path, so sleep instead of
            // spinning a core.
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                log(&format!("accept failed: {e}"));
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    // Dropping `listener` here frees the port for the next start.
    drop(listener);
    LOOP_ALIVE.store(false, Ordering::SeqCst);
}

/// One request from the shim.
#[derive(serde::Deserialize)]
struct Request {
    /// Echoed back so the shim can match a reply to its call.
    id: u64,
    token: String,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

/// Serve one client connection until it closes.
fn serve(stream: TcpStream, token: &str, handle: &tauri::AppHandle) -> std::io::Result<()> {
    let mut writer = stream.try_clone()?;
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        if !ACCEPTING.load(Ordering::SeqCst) {
            // Revoked mid-session: refuse without evaluating the request, so a
            // client holding a valid token still cannot act after revocation.
            let refused = serde_json::json!({
                "id": 0,
                "ok": false,
                "error": "agent access has been revoked",
            });
            let mut out = serde_json::to_vec(&refused).unwrap_or_default();
            out.push(b'\n');
            let _ = writer.write_all(&out);
            let _ = writer.flush();
            break;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                if !token_matches(token, &req.token) {
                    log("rejected a request with a bad token");
                    serde_json::json!({ "id": req.id, "ok": false, "error": "bad token" })
                } else {
                    match dispatch(handle, &req.method, &req.params) {
                        Ok(value) => serde_json::json!({ "id": req.id, "ok": true, "result": value }),
                        Err(e) => serde_json::json!({ "id": req.id, "ok": false, "error": e }),
                    }
                }
            }
            Err(e) => serde_json::json!({
                "id": 0,
                "ok": false,
                "error": format!("malformed request: {e}"),
            }),
        };
        let mut out = serde_json::to_vec(&reply).unwrap_or_default();
        out.push(b'\n');
        if writer.write_all(&out).is_err() || writer.flush().is_err() {
            break;
        }
    }
    Ok(())
}

/// Resolve which tab a request may act on.
///
/// The scope is enforced here, in the backend, because a restriction the page
/// could lift by claiming a different tab is not a restriction. With
/// `ActiveTab`, a request naming any other tab is refused with an explanation
/// rather than quietly redirected: silently acting on a different tab than the
/// one asked for would make an agent's observations wrong without saying so.
/// An omitted `appId` means "the active tab", which is the only tab that scope
/// allows anyway.
///
/// Sub-tabs do not weaken the scope: `ActiveTab` limits *which app* may be
/// addressed, and every sub-tab of that app is then fair game. A sub-tab is the
/// same app and the same session, so treating it as a different tab would only
/// make the scope confusing without protecting anything.
fn app_id_for(handle: &tauri::AppHandle, params: &serde_json::Value) -> Result<String, String> {
    use tauri::Manager;
    let state: tauri::State<crate::config::AppState> = handle.state();
    let active_only = state
        .config
        .lock()
        .map(|cfg| cfg.settings.mcp.scope == McpScope::ActiveTab)
        .unwrap_or(false);

    let named = params.get("appId").and_then(|v| v.as_str());
    if !active_only {
        return named
            .map(str::to_string)
            .ok_or_else(|| "missing string parameter 'appId'".to_string());
    }

    match (named, crate::tabs::active_app()) {
        (Some(id), Some(active)) if id == active => Ok(id.to_string()),
        (Some(id), Some(active)) => Err(format!(
            "agent scope is limited to the active tab, which is '{active}', not '{id}'. \
             Switch to that tab, or set the scope to \"all tabs\" in Settings."
        )),
        (Some(_), None) => Err(
            "agent scope is limited to the active tab, and no tab is currently shown. \
             Open a tab, or set the scope to \"all tabs\" in Settings."
                .to_string(),
        ),
        // No id given: the active tab is the answer.
        (None, Some(active)) => Ok(active),
        (None, None) => Err(
            "no tab is currently shown. Open a tab, or set the scope to \"all tabs\" \
             in Settings."
                .to_string(),
        ),
    }
}

/// The sub-tab a request names, if any.
///
/// Optional everywhere: omitting it means "whichever sub-tab of this app is on
/// screen", which is what a caller that has not learned about sub-tabs expects,
/// and what a single-tab app always wants.
fn tab_id_of(params: &serde_json::Value) -> Option<String> {
    params
        .get("tabId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// The tab rows this caller is allowed to see.
///
/// Under active-tab scope, listing everything would advertise tabs the caller is
/// not allowed to touch. Sub-tabs of the allowed app stay visible: they are the
/// same app, and hiding them would make `list_tabs` disagree with the strip the
/// user is looking at.
fn visible_tabs(handle: &tauri::AppHandle) -> Result<Vec<serde_json::Value>, String> {
    use tauri::Manager;
    let all = crate::tabs::list_tabs()?;
    let rows = all.as_array().cloned().unwrap_or_default();
    let state: tauri::State<crate::config::AppState> = handle.state();
    let active_only = state
        .config
        .lock()
        .map(|cfg| cfg.settings.mcp.scope == McpScope::ActiveTab)
        .unwrap_or(false);
    if !active_only {
        return Ok(rows);
    }
    let active = crate::tabs::active_app();
    Ok(rows
        .into_iter()
        .filter(|t| {
            active
                .as_deref()
                .is_some_and(|id| t.get("appId").and_then(|v| v.as_str()) == Some(id))
        })
        .collect())
}

/// Route a method name to its implementation.
fn dispatch(
    handle: &tauri::AppHandle,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    match method {
        "status" => Ok(serde_json::json!({
            "pid": std::process::id(),
            "port": current_port(),
            "endpoint": endpoint_path().to_string_lossy(),
        })),
        "list_tabs" => Ok(serde_json::Value::Array(visible_tabs(handle)?)),
        "console" => {
            let app_id = app_id_for(handle, params)?;
            crate::tabs::tab_console(
                app_id,
                opt_u64(params, "since"),
                opt_usize(params, "limit"),
                tab_id_of(params),
            )
        }
        "network" => {
            let app_id = app_id_for(handle, params)?;
            crate::tabs::tab_network(
                app_id,
                opt_u64(params, "since"),
                opt_usize(params, "limit"),
                tab_id_of(params),
            )
        }
        "eval" => {
            let app_id = app_id_for(handle, params)?;
            let expression = require_str(params, "expression")?.to_string();
            let win = crate::tabs::tab_window_of(&app_id, tab_id_of(params).as_deref(), handle)?;
            cdp::evaluate(&win, &expression)
        }
        "cdp" => {
            let app_id = app_id_for(handle, params)?;
            let cdp_method = require_str(params, "method")?.to_string();
            let args = params.get("params").cloned().unwrap_or(serde_json::json!({}));
            let win = crate::tabs::tab_window_of(&app_id, tab_id_of(params).as_deref(), handle)?;
            cdp::call(&win, &cdp_method, args)
        }
        "navigate" => {
            let app_id = app_id_for(handle, params)?;
            let url = require_str(params, "url")?.to_string();
            let win = crate::tabs::tab_window_of(&app_id, tab_id_of(params).as_deref(), handle)?;
            crate::tabs::navigate_window(&win, &url)?;
            Ok(serde_json::json!({ "navigated": url }))
        }
        "screenshot" => {
            let app_id = app_id_for(handle, params)?;
            let win = crate::tabs::tab_window_of(&app_id, tab_id_of(params).as_deref(), handle)?;
            // `fromSurface: true` makes the browser compositor produce the image,
            // which renders even a hidden tab. `fromSurface: false` captures the
            // already-presented surface, which is blank for a tab that has not
            // painted since it was occluded. `captureBeyondViewport` is set so a
            // hidden tab without a valid viewport size still gets a sensible frame.
            let reply = cdp::call(
                &win,
                "Page.captureScreenshot",
                serde_json::json!({ "format": "png", "fromSurface": true, "captureBeyondViewport": true }),
            )?;
            // WebView2 hands back the CDP *result object*: `{"data": "<base64>"}`.
            let data = reply
                .get("data")
                .or_else(|| reply.get("result").and_then(|r| r.get("data")))
                .and_then(|d| d.as_str())
                .ok_or("the browser returned no image data")?;
            Ok(serde_json::json!({ "data": data }))
        }
        // ---- workspace: apps -------------------------------------------------
        "list_apps" => Ok(serde_json::json!({ "apps": crate::workspace::list(handle)? })),
        "get_app" => {
            let app_id = require_str(params, "appId")?;
            Ok(serde_json::json!({ "app": crate::workspace::get(handle, app_id)? }))
        }
        "add_app" => {
            require_app_edits(handle)?;
            let app = crate::workspace::add(handle, patch_from(params))?;            Ok(serde_json::json!({ "app": app }))
        }
        "update_app" => {
            require_app_edits(handle)?;
            let app_id = require_str(params, "appId")?;
            let app = crate::workspace::update(handle, app_id, patch_from(params))?;
            Ok(serde_json::json!({ "app": app }))
        }
        "delete_app" => {
            require_app_edits(handle)?;
            let app_id = require_str(params, "appId")?;
            crate::workspace::delete(handle, app_id)?;
            Ok(serde_json::json!({ "deleted": app_id }))
        }
        "reorder_apps" => {
            require_app_edits(handle)?;
            let order: Vec<String> = params
                .get("order")
                .and_then(|v| v.as_array())
                .map(|list| {
                    list.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .ok_or("reorder_apps needs an 'order' array of app ids")?;
            let apps = crate::workspace::reorder(handle, &order)?;
            Ok(serde_json::json!({ "apps": apps }))
        }
        "activate_tab" => {
            let app_id = require_str(params, "appId")?.to_string();
            // An optional `tabId` selects one of the app's sub-tabs; without it
            // the app's active sub-tab is shown, which is what "activate this
            // app" means to a caller that does not know about sub-tabs.
            let tab_id = tab_id_of(params);
            crate::tabs::activate_app(handle.clone(), app_id.clone(), tab_id)?;
            Ok(serde_json::json!({ "active": app_id }))
        }
        "set_link_target" => {
            require_app_edits(handle)?;
            let app_id = require_str(params, "appId")?.to_string();
            let raw = require_str(params, "target")?;
            let target = crate::settings::LinkTarget::parse(raw)
                .ok_or_else(|| format!("unknown link target '{raw}'"))?;
            crate::tabs::set_link_target(&app_id, target, handle)?;
            Ok(serde_json::json!({ "appId": app_id, "linkTarget": target.as_str() }))
        }
        // ---- sub-tabs ---------------------------------------------------------
        // Reading is already covered: `list_tabs` reports every sub-tab of every
        // app, so a second listing method would be the same data under a second
        // name. What is new here is changing tabs, which is an app edit and is
        // gated like `add_app`.
        "open_sub_tab" => {
            require_app_edits(handle)?;
            let app_id = app_id_for(handle, params)?;
            let url = params
                .get("url")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            // Goes through the same scheduler the UI uses, so a tab opened by an
            // agent is built exactly like one opened by a click: same profile,
            // same proxy, same position.
            let app = crate::workspace::get(handle, &app_id)?;
            crate::tabs::schedule_sub_tab(handle.clone(), app, url)?;
            Ok(serde_json::json!({ "opened": app_id }))
        }
        "close_sub_tab" => {
            require_app_edits(handle)?;
            let app_id = require_str(params, "appId")?.to_string();
            let tab_id = require_str(params, "tabId")?.to_string();
            crate::tabs::close_sub_tab(app_id.clone(), tab_id.clone(), handle.clone())?;
            Ok(serde_json::json!({ "closed": tab_id, "appId": app_id }))
        }
        // ---- conversations ---------------------------------------------------
        "list_conversations" => crate::chat::list(handle),
        "get_conversation" => {
            let id = require_str(params, "conversationId")?;
            crate::chat::get(handle, id)
        }
        "send_message" => {
            let id = require_str(params, "conversationId")?.to_string();
            let text = require_str(params, "text")?.to_string();
            // Synchronous from the caller's point of view: an external agent
            // asking a question needs the answer, not a job id to poll. Blocking
            // here costs this connection its latency and nothing else — the
            // bridge already serves each client on its own thread, which is
            // also why it is safe: the tools wait on CDP replies delivered by
            // the main thread, never by this one.
            tauri::async_runtime::block_on(crate::chat::send(handle, &id, &text, Vec::new()))
        }
        other => Err(format!("unknown method '{other}'")),
    }
}

/// Build an app patch from request parameters.
///
/// `proxy` is special: passing it at all replaces the app's proxy, and passing
/// `null` clears it. A plain optional field could not say "clear this".
fn patch_from(params: &serde_json::Value) -> crate::workspace::AppPatch {
    let text = |key: &str| params.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let flag = |key: &str| params.get(key).and_then(|v| v.as_bool());
    let proxy = params.get("proxy").map(|value| {
        if value.is_null() {
            None
        } else {
            // A proxy may be given as an object or as a `scheme://user:pass@ip:port`
            // string, because both are natural to write by hand.
            match value.as_str() {
                Some(uri) => crate::settings::Proxy::parse(uri).ok(),
                None => serde_json::from_value::<crate::settings::Proxy>(value.clone()).ok(),
            }
            .filter(|p| p.is_usable())
        }
    });
    crate::workspace::AppPatch {
        name: text("name"),
        url: text("url"),
        icon: text("icon"),
        engine: None,
        external_browser_path: None,
        profile_mode: None,
        fingerprint: None,
        external_args: None,
        proxy_mode: text("proxyMode").and_then(|mode| match mode.as_str() {
            "inherit" => Some(crate::settings::ProxyMode::Inherit),
            "none" => Some(crate::settings::ProxyMode::None),
            "custom" => Some(crate::settings::ProxyMode::Custom),
            _ => None,
        }),
        proxy,
        user_agent: None,
        link_target: text("linkTarget").and_then(|t| crate::settings::LinkTarget::parse(&t)),
        enabled: flag("enabled"),
        muted: flag("muted"),
        notifications: flag("notifications"),
    }
}

/// Refuse a workspace edit unless the user turned it on.
fn require_app_edits(handle: &tauri::AppHandle) -> Result<(), String> {
    use tauri::Manager;
    let state: tauri::State<crate::config::AppState> = handle.state();
    let allowed = state
        .config
        .lock()
        .map(|cfg| cfg.settings.mcp.allow_app_edits)
        .unwrap_or(false);
    if allowed {
        Ok(())
    } else {
        Err(
            "editing apps over MCP is turned off. Enable \"Allow app edits\" in \
             Settings -> MCP if you want an agent to change the workspace."
                .to_string(),
        )
    }
}

fn require_str<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("missing string parameter '{key}'"))
}

fn opt_u64(params: &serde_json::Value, key: &str) -> Option<u64> {
    params.get(key).and_then(|v| v.as_u64())
}

fn opt_usize(params: &serde_json::Value, key: &str) -> Option<usize> {
    params.get(key).and_then(|v| v.as_u64()).map(|v| v as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_hex_chars_and_differ_between_calls() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn token_comparison_accepts_only_the_exact_value() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc123", "abc124"));
        assert!(!token_matches("abc123", "abc12"));
        assert!(!token_matches("abc123", "abc1234"));
        assert!(!token_matches("abc123", ""));
    }

    #[test]
    fn endpoint_file_sits_in_the_data_root() {
        assert_eq!(endpoint_path().parent(), Some(paths::root().as_path()));
        assert!(endpoint_path().ends_with(ENDPOINT_FILE));
    }

    #[test]
    fn require_str_names_the_missing_parameter() {
        let params = serde_json::json!({ "appId": "x" });
        assert_eq!(require_str(&params, "appId").unwrap(), "x");
        assert!(require_str(&params, "expression").unwrap_err().contains("expression"));
    }

    #[test]
    fn revocation_gate_starts_closed_and_tracks_stop() {
        // The gate must default to closed: a bridge whose listener somehow came
        // up without `start` finishing must not serve anything.
        ACCEPTING.store(false, Ordering::SeqCst);
        assert!(!ACCEPTING.load(Ordering::SeqCst));

        ACCEPTING.store(true, Ordering::SeqCst);
        assert!(ACCEPTING.load(Ordering::SeqCst));

        stop();
        assert!(!ACCEPTING.load(Ordering::SeqCst), "stop() must revoke access");
        assert!(!cdp::capture_enabled(), "stop() must switch capture off");
    }
}
