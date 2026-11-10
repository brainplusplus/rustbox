//! The AI Router: one local endpoint that republishes every provider model.
//!
//! A model is addressed the way 9router addresses one: `{prefix}/{model-id}`,
//! where the prefix belongs to a provider and everything after the first `/` is
//! the model id exactly as configured on it. A provider named "Risun" with prefix
//! `risun` serving `bansos/deepseek-v4.1-flash-1037` is therefore called as
//! `risun/bansos/deepseek-v4.1-flash-1037`. A combo is `combo/{name}`.
//!
//! Three protocols are answered on the same port, because that is the whole point
//! of a router: a client should not have to care which vendor answers.
//!
//! ```text
//! GET  /v1/models              every published model, plus every combo
//! POST /v1/chat/completions    [OI] Chat Completions
//! POST /v1/responses           [OI] Responses API
//! POST /v1/messages            Anthropic Messages
//! ```
//!
//! # Shape of the implementation
//!
//! A request is parsed into one canonical form ([`Canon`]), the target provider's
//! protocol is rendered from it, and the reply is parsed into [`Answer`] and
//! rendered back in the protocol the client spoke. That is a little more work
//! than forwarding bytes, and it is what makes a Chat Completions client able to
//! use an Anthropic-only provider.
//!
//! When the client's protocol already matches the provider's, the body is
//! forwarded with only `model` rewritten, so every field the client sent —
//! `response_format`, reasoning effort, tool choice — survives untouched.
//!
//! Streaming follows the same rule: a same-protocol request streams the
//! provider's own bytes through, while a translated one is answered in one piece
//! and then emitted as the target protocol's event sequence. Re-framing a
//! provider's live events into another protocol's grammar is a second
//! implementation of every protocol, not a router.
//!
//! # Why a hand-written server
//!
//! The same reason [`crate::mcp`] is one: this is a loopback endpoint on a desktop
//! app, and an async web framework for four routes would cost more than it saves.
//! The listener is a non-blocking accept loop, each connection gets a thread, and
//! upstream calls use the blocking HTTP client.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use crate::settings::{AgentPreset, ApiKind, Combo, ComboStrategy, Provider};

static ACCEPTING: AtomicBool = AtomicBool::new(false);
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
static LOOP_ALIVE: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static PORT: AtomicU16 = AtomicU16::new(0);

// ---- lifecycle ------------------------------------------------------------

/// Where the router's own diagnostics go.
pub fn log_path() -> std::path::PathBuf {
    crate::ai_logs::dir().join("ai-router.log")
}

pub fn log(message: &str) {
    crate::ai_logs::note(message);
}

pub fn current_port() -> Option<u16> {
    let port = PORT.load(Ordering::SeqCst);
    (port != 0).then_some(port)
}

pub fn is_running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Start on the configured port and bind mode.
pub fn start(handle: tauri::AppHandle) -> Result<u16, String> {
    let (port, publish, tailscale) = {
        use tauri::Manager;
        let state: tauri::State<crate::config::AppState> = handle.state();
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        let r = &cfg.settings.router;
        (r.effective_port(), r.publish, r.tailscale)
    };
    start_with(handle, port, publish, tailscale)
}

pub fn restart(handle: tauri::AppHandle) -> Result<u16, String> {
    stop();
    start(handle)
}

/// Stop accepting and release the port.
pub fn stop() {
    ACCEPTING.store(false, Ordering::SeqCst);
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    for _ in 0..200 {
        if !LOOP_ALIVE.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    RUNNING.store(false, Ordering::SeqCst);
    PORT.store(0, Ordering::SeqCst);
    log("router stopped");
}

/// Where to listen.
///
/// Loopback unless published, because a router that republishes paid model access
/// is the last thing that should be reachable from a café Wi-Fi without being
/// asked for. `tailscale` widens it to every interface so the tailnet can reach
/// it — Tailscale's own access rules are then the control, which is why the page
/// says so rather than implying the port is private.
fn bind_address(publish: bool, tailscale: bool) -> std::net::IpAddr {
    if publish || tailscale {
        std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

pub fn start_with(
    handle: tauri::AppHandle,
    port: u16,
    publish: bool,
    tailscale: bool,
) -> Result<u16, String> {
    if RUNNING.load(Ordering::SeqCst) {
        return Err("the AI Router is already running".to_string());
    }
    let address = bind_address(publish, tailscale);
    let listener = TcpListener::bind((address, port))
        .map_err(|e| format!("could not bind port {port} on {address}: {e}"))?;
    if let Err(e) = listener.set_nonblocking(true) {
        return Err(format!("could not configure the router listener: {e}"));
    }
    let bound = listener.local_addr().map_err(|e| e.to_string())?.port();

    STOP_REQUESTED.store(false, Ordering::SeqCst);
    ACCEPTING.store(true, Ordering::SeqCst);
    PORT.store(bound, Ordering::SeqCst);
    RUNNING.store(true, Ordering::SeqCst);
    LOOP_ALIVE.store(true, Ordering::SeqCst);
    log(&format!("router listening on {address}:{bound}"));

    std::thread::Builder::new()
        .name("rustbox-ai-router".to_string())
        .spawn(move || accept_loop(handle, listener))
        .map_err(|e| {
            RUNNING.store(false, Ordering::SeqCst);
            ACCEPTING.store(false, Ordering::SeqCst);
            PORT.store(0, Ordering::SeqCst);
            LOOP_ALIVE.store(false, Ordering::SeqCst);
            format!("could not start the router thread: {e}")
        })?;
    Ok(bound)
}

fn accept_loop(handle: tauri::AppHandle, listener: TcpListener) {
    while !STOP_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if let Err(e) = stream.set_nonblocking(false) {
                    log(&format!("could not configure an incoming connection: {e}"));
                    continue;
                }
                if !ACCEPTING.load(Ordering::SeqCst) {
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }
                let handle = handle.clone();
                let loopback = peer.ip().is_loopback();
                // One thread per connection: a model call takes seconds and must
                // not stall the accept loop for other clients.
                std::thread::spawn(move || {
                    if let Err(e) = serve(stream, &handle, loopback) {
                        log(&format!("connection from {peer} ended: {e}"));
                    }
                });
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => {
                log(&format!("accept failed: {e}"));
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
    }
    drop(listener);
    LOOP_ALIVE.store(false, Ordering::SeqCst);
}

/// The live state of the router, as the settings page needs to show it.
///
/// Read from the process, not from the saved settings, because the two disagree
/// in exactly the cases that matter: a port that failed to bind, a tunnel that is
/// up on a URL nobody stored, a Tailscale address that changed since launch.
pub fn live_state(settings: &crate::settings::Settings) -> serde_json::Value {
    let router = &settings.router;
    let port = current_port().unwrap_or(router.effective_port());
    let tunnel = crate::ai_tunnel::state();
    let tailscale = crate::ai_tunnel::tailscale_state();
    // Asked once: each answer is a process launch, and `tunnel_available()` used
    // to repeat this lookup for the same state read.
    let cloudflared = crate::ai_tunnel::cloudflared_path();
    let catalogue = Catalogue::of(settings);
    serde_json::json!({
        "running": is_running(),
        "port": port,
        "configuredPort": router.effective_port(),
        "publish": router.publish,
        "tailscale": router.tailscale,
        "localEndpoint": format!("http://127.0.0.1:{port}/v1"),
        "networkEndpoint": if router.publish || router.tailscale {
            crate::settings::network_address().map(|ip| format!("http://{ip}:{port}/v1"))
        } else {
            None
        },
        "tailnetEndpoint": if !tailscale.address.is_empty() {
            Some(format!("http://{}:{port}/v1", tailscale.address))
        } else {
            None
        },
        "tunnel": tunnel,
        "tailscaleState": tailscale,
        "cloudflared": cloudflared.clone(),
        "cloudflaredAvailable": cloudflared.is_some(),
        "logFile": crate::ai_logs::file().to_string_lossy(),
        "routerLog": log_path().to_string_lossy(),
        "logBytes": crate::ai_logs::size(),
        "logEnabled": router.log.enabled,
        "logLimit": router.log.effective_limit(),
        "keyCount": router.keys.len(),
        "enabledKeys": router.keys.iter().filter(|k| k.enabled).count(),
        "models": catalogue.models.len(),
        "combos": catalogue.combos.len(),
    })
}

// ---- HTTP ----------------------------------------------------------------

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Read one HTTP/1.1 request.
///
/// Hand-parsed because the alternative is a framework. The parts that matter for
/// a JSON API are the request line, `content-length` (or chunked encoding, which
/// some SDKs use for streamed request bodies), and a case-insensitive
/// `authorization`.
fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let raw_path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || raw_path.is_empty() {
        return Ok(None);
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body: Vec<u8> = Vec::new();
    if chunked {
        loop {
            let mut size_line = String::new();
            if reader.read_line(&mut size_line)? == 0 {
                break;
            }
            let size = usize::from_str_radix(
                size_line.trim().split(';').next().unwrap_or("0").trim(),
                16,
            )
            .unwrap_or(0);
            if size == 0 {
                // Consume the trailer, if any.
                loop {
                    let mut trailer = String::new();
                    if reader.read_line(&mut trailer)? == 0 {
                        break;
                    }
                    if trailer.trim().is_empty() {
                        break;
                    }
                }
                break;
            }
            let mut chunk = vec![0u8; size];
            reader.read_exact(&mut chunk)?;
            body.extend_from_slice(&chunk);
            let mut crlf = [0u8; 2];
            let _ = reader.read_exact(&mut crlf);
        }
    } else if length > 0 {
        body = vec![0u8; length];
        reader.read_exact(&mut body)?;
    }
    // A body larger than this is not a chat request; refusing it keeps a
    // misconfigured client from making the router allocate without bound.
    const MAX_BODY: usize = 32 * 1024 * 1024;
    if body.len() > MAX_BODY {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "the request body is too large",
        ));
    }

    // Only the part after the authority is a route, and a query string is never
    // part of one.
    let path = match raw_path.split_once("://") {
        Some((_, rest)) => match rest.find('/') {
            Some(slash) => rest[slash..].to_string(),
            None => "/".to_string(),
        },
        None => raw_path,
    };
    let path = path.split('?').next().unwrap_or("/").to_string();
    Ok(Some(Request {
        method,
        path,
        headers,
        body,
    }))
}

/// Everything below writes to `dyn Write` rather than a `TcpStream` on purpose.
///
/// A combo that can fail over must not commit an answer until it knows the
/// member produced one: once the status line is on the socket the reply is
/// decided, and a later member's success can no longer be sent. Writing through
/// this trait lets the fallback path collect each attempt in a buffer and only
/// flush the one that worked.
fn write_response(
    sink: &mut dyn Write,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {}\r\n\
         content-type: {content_type}\r\n\
         content-length: {}\r\n\
         access-control-allow-origin: *\r\n\
         access-control-allow-headers: authorization, content-type, x-api-key, anthropic-version, anthropic-beta\r\n\
         access-control-allow-methods: GET, POST, OPTIONS\r\n\
         connection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    sink.write_all(head.as_bytes())?;
    sink.write_all(body)?;
    sink.flush()
}

fn write_json(sink: &mut dyn Write, status: u16, value: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    write_response(sink, status, "application/json", &body)
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// An error that carries the status the client should see.
#[derive(Debug)]
struct Failure {
    status: u16,
    message: String,
    kind: &'static str,
}

impl Failure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            kind: "invalid_request_error",
        }
    }

    fn upstream(message: impl Into<String>) -> Self {
        Self {
            status: 502,
            message: message.into(),
            kind: "upstream_error",
        }
    }

    /// [OI]-shaped error body. Anthropic clients read `error.message` too, so one
    /// shape serves both without a second renderer.
    fn body(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "error",
            "error": {
                "message": self.message,
                "type": self.kind,
                "code": self.status,
            }
        })
    }
}

// ---- routing --------------------------------------------------------------

fn serve(mut stream: TcpStream, handle: &tauri::AppHandle, loopback: bool) -> std::io::Result<()> {
    let request = match read_request(&mut stream)? {
        Some(r) => r,
        None => return Ok(()),
    };
    // A browser preflight must succeed before the real request is sent; without
    // it, a page-based client never gets to the call at all.
    if request.method == "OPTIONS" {
        return write_response(&mut stream, 204, "text/plain", b"");
    }

    let settings = match read_settings(handle) {
        Ok(s) => s,
        Err(e) => return write_json(&mut stream, 500, &Failure::new(500, e).body()),
    };
    let catalogue = Catalogue::of(&settings);

    let presented = bearer(&request);
    let key_name = match authenticate(&settings.router, presented.as_deref(), loopback) {
        Ok(name) => name,
        Err(failure) => return write_json(&mut stream, failure.status, &failure.body()),
    };

    let route = request
        .path
        .trim_end_matches('/')
        .strip_prefix("/v1")
        .unwrap_or(request.path.trim_end_matches('/'))
        .to_string();

    match (request.method.as_str(), route.as_str()) {
        ("GET", "/models") | ("GET", "") => {
            let list = models_body(&catalogue, &settings.router, key_name.as_deref());
            write_json(&mut stream, 200, &list)
        }
        ("GET", "/health") => write_json(
            &mut stream,
            200,
            &serde_json::json!({
                "ok": true,
                "port": current_port(),
                "models": catalogue.models.len(),
                "combos": catalogue.combos.len(),
            }),
        ),
        ("POST", "/chat/completions") => handle_call(
            &mut stream,
            &settings,
            &catalogue,
            &request,
            Protocol::Chat,
            key_name.as_deref(),
        ),
        ("POST", "/responses") => handle_call(
            &mut stream,
            &settings,
            &catalogue,
            &request,
            Protocol::Responses,
            key_name.as_deref(),
        ),
        ("POST", "/messages") => handle_call(
            &mut stream,
            &settings,
            &catalogue,
            &request,
            Protocol::Anthropic,
            key_name.as_deref(),
        ),
        (_, "/models" | "/chat/completions" | "/responses" | "/messages" | "/health") => {
            let failure = Failure::new(
                405,
                format!("{} is not allowed on {}", request.method, request.path),
            );
            write_json(&mut stream, 405, &failure.body())
        }
        _ => {
            let failure = Failure::new(
                404,
                format!(
                    "no route for {}. Use /v1/models, /v1/chat/completions, \
                     /v1/responses or /v1/messages.",
                    request.path
                ),
            );
            write_json(&mut stream, 404, &failure.body())
        }
    }
}

/// The bearer token from either header convention.
fn bearer(request: &Request) -> Option<String> {
    if let Some(value) = request.header("authorization")
        && let Some(rest) = value.strip_prefix("Bearer ")
    {
        return Some(rest.trim().to_string());
    }
    if let Some(value) = request.header("x-api-key") {
        return Some(value.trim().to_string());
    }
    None
}

/// Check the presented key and return its display name.
///
/// `Ok(None)` means "no key was needed", which is only ever true for a loopback
/// caller on a router with no keys: with a key configured, presenting none is a
/// refusal, and a published router with no keys at all is a refusal too, because
/// that would be an open proxy for paid model access.
fn authenticate(
    router: &crate::settings::RouterSettings,
    presented: Option<&str>,
    loopback: bool,
) -> Result<Option<String>, Failure> {
    let enabled = router
        .keys
        .iter()
        .filter(|k| k.enabled && !k.key.trim().is_empty())
        .count();
    if enabled == 0 {
        if loopback {
            return Ok(None);
        }
        return Err(Failure {
            status: 401,
            message: "this router is reachable from the network but has no enabled API key. \
                      Add one in Settings -> AI Router, or turn publishing off."
                .to_string(),
            kind: "authentication_error",
        });
    }
    let Some(presented) = presented.filter(|p| !p.is_empty()) else {
        return Err(Failure {
            status: 401,
            message: "missing API key. Send it as 'Authorization: Bearer <key>' or \
                      'x-api-key: <key>'."
                .to_string(),
            kind: "authentication_error",
        });
    };
    let found = router.key_for(presented).ok_or_else(|| Failure {
        status: 401,
        message: "that API key is not valid, or it is disabled.".to_string(),
        kind: "authentication_error",
    })?;
    Ok(Some(found.name.clone()))
}

/// Read the settings the router should serve from.
fn read_settings(handle: &tauri::AppHandle) -> Result<crate::settings::Settings, String> {
    use tauri::Manager;
    let state: tauri::State<crate::config::AppState> = handle.state();
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(cfg.settings.clone())
}

// ---- catalogue ------------------------------------------------------------

/// Every published model, and every enabled combo.
#[derive(Debug, Default, Clone)]
pub struct Catalogue {
    pub models: Vec<PublishedModel>,
    pub combos: Vec<Combo>,
}

#[derive(Debug, Clone)]
pub struct PublishedModel {
    /// `prefix/model`, the id a client passes.
    pub id: String,
    pub provider_id: String,
    pub provider_name: String,
    /// The model id as configured on the provider, i.e. what is sent upstream.
    pub upstream: String,
}

impl Catalogue {
    /// Build the catalogue from settings.
    ///
    /// A provider whose models are empty contributes nothing, because there is
    /// nothing to address; a disabled combo is left out entirely so a client that
    /// lists models cannot pick something the router will refuse.
    pub fn of(settings: &crate::settings::Settings) -> Catalogue {
        let mut models = Vec::new();
        for provider in &settings.providers {
            let prefix = provider.effective_prefix();
            for model in provider.model_list() {
                let model = model.trim().to_string();
                if model.is_empty() {
                    continue;
                }
                models.push(PublishedModel {
                    id: format!("{prefix}/{model}"),
                    provider_id: provider.id.clone(),
                    provider_name: provider.name.clone(),
                    upstream: model,
                });
            }
        }
        Catalogue {
            models,
            combos: settings
                .combos
                .iter()
                .filter(|c| c.enabled && !c.members.is_empty())
                .cloned()
                .collect(),
        }
    }

    /// Look up a model id.
    ///
    /// A bare combo name is accepted as well as `combo/name`, because a client
    /// that lists `/v1/models` may pass either, and refusing one of them makes the
    /// catalogue and the call disagree.
    pub fn resolve(&self, id: &str) -> Option<Target> {
        let trimmed = id.trim();
        if let Some(model) = self.models.iter().find(|m| m.id == trimmed) {
            return Some(Target::Single(model.clone()));
        }
        if let Some(rest) = trimmed.strip_prefix("combo/")
            && let Some(combo) = self.combos.iter().find(|c| c.name == rest)
        {
            return Some(Target::Group(combo.clone()));
        }
        self.combos
            .iter()
            .find(|c| c.name == trimmed)
            .cloned()
            .map(Target::Group)
    }
}

/// What a request resolved to.
pub enum Target {
    Single(PublishedModel),
    Group(Combo),
}

/// The `/v1/models` body.
///
/// [OI] shape (`object: list`, `data: [...]`) because that is what every client
/// parses; Anthropic clients read the same list and ignore the extra keys. The
/// list is filtered by the presented key's restrictions, so a restricted client is
/// not offered models it will be refused.
fn models_body(
    catalogue: &Catalogue,
    router: &crate::settings::RouterSettings,
    key_name: Option<&str>,
) -> serde_json::Value {
    let restriction = key_name.and_then(|name| router.keys.iter().find(|k| k.name == name));
    let allowed = |id: &str| match restriction {
        Some(key) => key.allows(id),
        None => true,
    };
    let mut data: Vec<serde_json::Value> = catalogue
        .models
        .iter()
        .filter(|m| allowed(&m.id))
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "object": "model",
                "created": 0,
                "owned_by": m.provider_name,
                // Not part of the [OI] schema, but harmless, and it lets a
                // client show where a model really comes from.
                "provider": m.provider_name,
            })
        })
        .collect();
    for combo in &catalogue.combos {
        let id = combo.model_id();
        if !allowed(&id) {
            continue;
        }
        data.push(serde_json::json!({
            "id": id,
            "object": "model",
            "created": 0,
            "owned_by": "combo",
            "provider": format!("combo · {}", combo.strategy.label()),
        }));
    }
    serde_json::json!({
        "object": "list",
        "data": data,
        // Anthropic clients look for this key instead of `data`.
        "models": data.clone(),
        "has_more": false,
    })
}

/// Which shape the client is speaking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Chat,
    Responses,
    Anthropic,
}

impl Protocol {
    fn kind(self) -> ApiKind {
        match self {
            Protocol::Chat => ApiKind::ChatCompletions,
            Protocol::Responses => ApiKind::Responses,
            Protocol::Anthropic => ApiKind::Anthropic,
        }
    }

    fn endpoint(self) -> &'static str {
        match self {
            Protocol::Chat => "chat/completions",
            Protocol::Responses => "responses",
            Protocol::Anthropic => "messages",
        }
    }
}

// ---- one call -------------------------------------------------------------

/// What a finished call produced, so the caller can log it.
struct Done {
    status: u16,
    usage: Usage,
    streamed: bool,
    /// The model id sent upstream.
    upstream: String,
    /// Which provider answered, so a combo logs the member that won.
    provider: String,
    /// The published model id whose price applies. A combo is called by its own
    /// name, which has no price; the member that answered does.
    billed: String,
    /// Time to first token, 0 when the reply did not stream.
    ttft_ms: u64,
    /// True when the output token count was counted from the event stream
    /// rather than reported by the provider.
    estimated: bool,
}

fn handle_call(
    stream: &mut TcpStream,
    settings: &crate::settings::Settings,
    catalogue: &Catalogue,
    request: &Request,
    protocol: Protocol,
    key_name: Option<&str>,
) -> std::io::Result<()> {
    let started = std::time::Instant::now();
    let body: serde_json::Value = match serde_json::from_slice(&request.body) {
        Ok(v) => v,
        Err(e) => {
            let failure = Failure::new(400, format!("the request body is not JSON: {e}"));
            return write_json(stream, 400, &failure.body());
        }
    };
    let model_id = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if model_id.is_empty() {
        let failure = Failure::new(400, "the request has no 'model'.");
        return write_json(stream, 400, &failure.body());
    }

    // From here the request is real, so it becomes visible in the request-log
    // pane while it runs — not only once it finishes. The guard removes it again
    // on every exit path, including the early returns below.
    let _live = LiveGuard::begin(
        protocol.endpoint(),
        &model_id,
        key_name,
        body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false),
    );

    let target = match catalogue.resolve(&model_id) {
        Some(t) => t,
        None => {
            let mut ids: Vec<&str> = catalogue
                .models
                .iter()
                .map(|m| m.id.as_str())
                .chain(catalogue.combos.iter().map(|c| c.name.as_str()))
                .collect();
            ids.sort_unstable();
            let hint = if ids.is_empty() {
                "No models are published yet: add a provider in Settings -> AI Provider."
                    .to_string()
            } else {
                let shown: Vec<&str> = ids.iter().take(8).copied().collect();
                format!("Known ids include: {}.", shown.join(", "))
            };
            let failure = Failure::new(404, format!("unknown model '{model_id}'. {hint}"));
            return write_json(stream, 404, &failure.body());
        }
    };

    // Restricted access is enforced here, per key, on the resolved id — not on
    // what `/v1/models` showed, because a client can ask for anything.
    if let Some(name) = key_name
        && let Some(key) = settings.router.keys.iter().find(|k| k.name == name)
        && !key.allows(&model_id)
    {
        let failure = Failure {
            status: 403,
            message: format!("this API key may not use '{model_id}'."),
            kind: "permission_error",
        };
        record(
            settings,
            LogLine {
                model: &model_id,
                billed: &model_id,
                upstream: "",
                provider: "",
                endpoint: protocol.endpoint(),
                status: 403,
                ms: started.elapsed().as_millis() as u64,
                usage: Usage::default(),
                streamed: false,
                error: &failure.message,
                key: key_name,
                ttft_ms: 0,
                estimated: false,
            },
        );
        return write_json(stream, 403, &failure.body());
    }

    let outcome = match target {
        Target::Single(model) => match provider_of(settings, &model) {
            // A single model owns the connection: its answer, including a
            // failure, is the reply.
            Ok(provider) => call_single(stream, settings, &provider, &model, &body, protocol, true),
            Err(failure) => {
                let _ = write_json(stream, failure.status, &failure.body());
                Err(failure)
            }
        },
        Target::Group(combo) => call_combo(stream, settings, catalogue, &combo, &body, protocol),
    };
    let elapsed = started.elapsed().as_millis() as u64;
    // An upstream refusal is a finished call, not a router failure, so it lands
    // in the `Ok` arm. Saying so is the difference between a log line a user can
    // act on and one that just shows a number.
    let relayed = outcome
        .as_ref()
        .ok()
        .filter(|done| done.status >= 400)
        .map(|done| format!("{} returned HTTP {}", done.provider, done.status));
    match &outcome {
        Ok(done) => record(
            settings,
            LogLine {
                model: &model_id,
                billed: &done.billed,
                upstream: &done.upstream,
                provider: &done.provider,
                endpoint: protocol.endpoint(),
                status: done.status,
                ms: elapsed,
                usage: done.usage,
                streamed: done.streamed,
                error: relayed.as_deref().unwrap_or(""),
                key: key_name,
                ttft_ms: done.ttft_ms,
                estimated: done.estimated,
            },
        ),
        Err(failure) => record(
            settings,
            LogLine {
                model: &model_id,
                billed: &model_id,
                upstream: "",
                provider: "",
                endpoint: protocol.endpoint(),
                status: failure.status,
                ms: elapsed,
                usage: Usage::default(),
                streamed: false,
                error: &failure.message,
                key: key_name,
                ttft_ms: 0,
                estimated: false,
            },
        ),
    }

    match outcome {
        Ok(_) => Ok(()),
        Err(failure) => write_json(stream, failure.status, &failure.body()),
    }
}

/// Keeps one request in the in-flight registry for exactly as long as it runs.
///
/// A guard rather than paired begin/end calls at each exit: `handle_call` has
/// several early returns, and a missed one would leave a row in the log pane
/// that never disappears. Dropping the guard is the one thing that always
/// happens.
struct LiveGuard(std::sync::Arc<crate::ai_logs::Live>);

impl LiveGuard {
    fn begin(endpoint: &str, model: &str, key: Option<&str>, stream: bool) -> Self {
        let live = crate::ai_logs::begin(endpoint, model, key, stream);
        crate::ai_logs::attach(live.clone());
        LiveGuard(live)
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.end();
        crate::ai_logs::detach();
    }
}

fn provider_of(
    settings: &crate::settings::Settings,
    model: &PublishedModel,
) -> Result<Provider, Failure> {    settings
        .providers
        .iter()
        .find(|p| p.id == model.provider_id)
        .cloned()
        .ok_or_else(|| {
            Failure::new(
                500,
                format!(
                    "provider '{}' is no longer in the config",
                    model.provider_name
                ),
            )
        })
}

/// One log line, in the shape the caller has available at that point.
struct LogLine<'a> {
    model: &'a str,
    /// The id to price the request by. Equal to `model` unless a combo answered.
    billed: &'a str,
    upstream: &'a str,
    provider: &'a str,
    endpoint: &'a str,
    status: u16,
    ms: u64,
    usage: Usage,
    streamed: bool,
    error: &'a str,
    key: Option<&'a str>,
    /// Time to first token, 0 when the reply did not stream.
    ttft_ms: u64,
    /// True when the output count is a count of stream events, not a usage
    /// report. Marked in the log so the page can label it as an estimate.
    estimated: bool,
}

fn record(settings: &crate::settings::Settings, line: LogLine<'_>) {
    if !settings.router.log.enabled {
        return;
    }
    let cost = estimate_cost(
        settings,
        line.billed,
        line.usage.input_tokens,
        line.usage.output_tokens,
    );
    // Generation speed excludes the wait for the first token: a model that
    // thinks for five seconds and then writes 100 tokens in one second is a
    // 100 t/s model, not a 20 t/s one. For a buffered reply there is no first
    // token, so `ttft_ms` is 0 and the whole duration is generation time.
    let generating_ms = line.ms.saturating_sub(line.ttft_ms);
    let tps = if line.usage.output_tokens > 0 && generating_ms > 0 {
        (line.usage.output_tokens as f64 * 1000.0 / generating_ms as f64 * 10.0).round() / 10.0
    } else {
        0.0
    };
    let entry = crate::ai_logs::Entry {
        at: crate::ai_logs::now_ms(),
        endpoint: line.endpoint.to_string(),
        model: line.model.to_string(),
        upstream: line.upstream.to_string(),
        provider: line.provider.to_string(),
        status: line.status,
        ms: line.ms,
        input_tokens: line.usage.input_tokens,
        output_tokens: line.usage.output_tokens,
        cost,
        key: line.key.unwrap_or("—").to_string(),
        stream: line.streamed,
        error: crate::settings::shorten(line.error, 240),
        ttft_ms: line.ttft_ms,
        tps,
        estimated: line.estimated,
    };
    crate::ai_logs::record(&entry, settings.router.log.effective_limit());
}

// ---- cost -----------------------------------------------------------------

/// Published per-million prices, keyed on a model-id fragment.
///
/// The longest matching fragment wins, because `gpt-4o-mini` must not be priced
/// by `gpt-4o`. A table rather than a price service: the figure is an estimate for
/// a dashboard, and a network call per request to price the request would cost
/// more than the answer is worth.
///
/// These are list prices at the time of writing. They are never presented as a
/// bill — the log labels every figure "estimated" — and a provider with its own
/// rates always wins over this table.
const PUBLISHED_PRICES: &[(&str, f64, f64)] = &[
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1-nano", 0.10, 0.40),
    ("gpt-4-turbo", 10.00, 30.00),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4.1", 2.00, 8.00),
    ("gpt-5-mini", 0.25, 2.00),
    ("gpt-5-nano", 0.05, 0.40),
    ("gpt-5", 1.25, 10.00),
    ("gpt-3.5", 0.50, 1.50),
    ("o4-mini", 1.10, 4.40),
    ("o3-mini", 1.10, 4.40),
    ("o3", 2.00, 8.00),
    ("o1-mini", 1.10, 4.40),
    ("o1", 15.00, 60.00),
    ("opus", 15.00, 75.00),
    ("sonnet", 3.00, 15.00),
    ("haiku", 0.80, 4.00),
    ("deepseek-reasoner", 0.55, 2.19),
    ("deepseek", 0.27, 1.10),
    ("gemini-2.5-flash", 0.30, 2.50),
    ("gemini-2.5-pro", 1.25, 10.00),
    ("gemini", 1.25, 10.00),
    ("claude", 3.00, 15.00),
    ("glm", 0.60, 2.20),
    ("qwen", 0.40, 1.20),
    ("kimi", 0.60, 2.50),
    ("moonshot", 0.60, 2.50),
    ("minimax", 0.30, 1.20),
    ("llama", 0.20, 0.60),
    ("mistral", 0.20, 0.60),
];

/// USD per million tokens for a model id, or `None` when nothing is known.
pub fn published_price(model_id: &str) -> Option<(f64, f64)> {
    let lower = model_id.to_ascii_lowercase();
    PUBLISHED_PRICES
        .iter()
        .filter(|(key, _, _)| lower.contains(key))
        .max_by_key(|(key, _, _)| key.len())
        .map(|(_, input, output)| (*input, *output))
}

/// Estimate what one request cost.
///
/// The provider's own rates win when it has any, because a user who typed their
/// real price knows better than a table baked into the binary. Otherwise the table
/// is consulted; a model in neither produces 0, which the page renders as "no
/// rate" rather than "$0.00".
pub fn estimate_cost(
    settings: &crate::settings::Settings,
    model_id: &str,
    input_tokens: u64,
    output_tokens: u64,
) -> f64 {
    let prefix = model_id.split('/').next().unwrap_or_default();
    let own = settings
        .providers
        .iter()
        .find(|p| p.effective_prefix() == prefix);
    let rates = match own {
        Some(p) if p.input_price > 0.0 || p.output_price > 0.0 => {
            Some((p.input_price, p.output_price))
        }
        _ => published_price(model_id),
    };
    match rates {
        Some((input, output)) => {
            (input_tokens as f64 * input + output_tokens as f64 * output) / 1_000_000.0
        }
        None => 0.0,
    }
}

// ---- upstream -------------------------------------------------------------

/// One HTTP client for upstream calls, shared across connections.
///
/// Built once: TLS setup is the expensive part and a router makes many calls.
fn client() -> Result<&'static reqwest::blocking::Client, String> {
    static CLIENT: OnceLock<Result<reqwest::blocking::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(20))
                // Generous because a long answer legitimately takes a while, but
                // bounded so a hung upstream cannot pin a thread forever.
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .map_err(|e| format!("could not start the HTTP client: {e}"))
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// Headers for an upstream call: the provider's own, with the router's agent
/// preset applied on top.
///
/// The router preset wins, because it describes the client the router is
/// impersonating for this request; a provider preset would otherwise quietly claim
/// to be a different tool than the one the user picked here.
fn upstream_headers(
    settings: &crate::settings::Settings,
    provider: &Provider,
) -> Vec<(String, String)> {
    let mut out = provider.request_headers();
    let agent: AgentPreset = settings.router.agent;
    for (name, value) in agent.headers() {
        let lower = name.to_ascii_lowercase();
        out.retain(|(k, _)| k.to_ascii_lowercase() != lower);
        out.push((name, value));
    }
    out
}

fn request_builder(
    settings: &crate::settings::Settings,
    provider: &Provider,
    body: &serde_json::Value,
) -> Result<reqwest::blocking::RequestBuilder, Failure> {
    let client = client().map_err(|message| Failure::new(500, message))?;
    let mut request = client
        .post(provider.endpoint())
        .header("content-type", "application/json")
        .json(body);
    for (name, value) in upstream_headers(settings, provider) {
        request = request.header(name, value);
    }
    Ok(request)
}

/// Send one request and read the whole reply.
fn send(
    settings: &crate::settings::Settings,
    provider: &Provider,
    body: &serde_json::Value,
) -> Result<(u16, String), Failure> {
    let response = request_builder(settings, provider, body)?
        .send()
        .map_err(|e| Failure::upstream(format!("could not reach {}: {e}", provider.name)))?;
    let status = response.status().as_u16();
    let text = response.text().unwrap_or_default();
    Ok((status, text))
}

/// Send one request to one provider and answer the client.
///
/// `commit` says whether this attempt owns the client connection. It is true for
/// a single model and for a round-robin pick, where the member that was chosen is
/// the answer. It is false for a fallback member: a candidate that fails must
/// leave the socket untouched so the next candidate can still answer. Writing an
/// upstream error straight through there used to consume the one response the
/// client would ever read, which made every fallback combo fail on its first
/// broken member instead of falling back.
fn call_single(
    stream: &mut TcpStream,
    settings: &crate::settings::Settings,
    provider: &Provider,
    model: &PublishedModel,
    body: &serde_json::Value,
    protocol: Protocol,
    commit: bool,
) -> Result<Done, Failure> {
    let wants_stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);
    let same_protocol = provider.kind == protocol.kind();
    // Name the provider on the live row as soon as it is picked: a fallback
    // combo can sit on one member for seconds before anything else is known.
    crate::ai_logs::provider(&provider.name);

    if same_protocol {
        // Only the model id changes, so every field the client sent survives —
        // `response_format`, reasoning effort, tool choice, and anything this
        // build has never heard of.
        let mut forwarded = body.clone();
        forwarded["model"] = serde_json::Value::String(model.upstream.clone());
        return forward(
            stream,
            settings,
            provider,
            &forwarded,
            wants_stream,
            commit,
            &model.id,
        );
    }

    // Different protocol: parse, render for the provider, render the reply back.
    let canon = Canon::parse(protocol, body)?;
    let outbound = canon.emit(provider.kind, &model.upstream);
    let (status, text) = send(settings, provider, &outbound)?;
    if status >= 400 {
        return Err(Failure::upstream(upstream_message(
            &provider.name,
            status,
            &text,
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Failure::upstream(format!("{} returned invalid JSON: {e}", provider.name)))?;
    let answer = Answer::parse(provider.kind, &parsed);
    let usage = answer.usage;
    // This path renders a buffered upstream reply into frames, so there is no
    // per-token arrival to time: the first token is available as soon as the
    // whole answer is. Reporting a first-token time here would be reporting the
    // provider's full latency twice.
    let ttft_ms = 0;

    if wants_stream {
        write_stream_head(stream).map_err(|e| Failure::new(500, e.to_string()))?;
        let frames = answer.sse(protocol, &model.id);
        let _ = stream.write_all(frames.as_bytes());
        let _ = stream.flush();
    } else {
        let rendered = answer.emit(protocol, &model.id);
        write_json(stream, 200, &rendered).map_err(|e| Failure::new(500, e.to_string()))?;
    }
    Ok(Done {
        status: 200,
        usage,
        streamed: wants_stream,
        upstream: model.upstream.clone(),
        provider: provider.name.clone(),
        billed: model.id.clone(),
        ttft_ms,
        estimated: false,
    })
}

/// Forward a same-protocol request, streaming when asked.
fn forward(
    stream: &mut TcpStream,
    settings: &crate::settings::Settings,
    provider: &Provider,
    body: &serde_json::Value,
    wants_stream: bool,
    commit: bool,
    billed: &str,
) -> Result<Done, Failure> {
    let mut response = request_builder(settings, provider, body)?
        .send()
        .map_err(|e| Failure::upstream(format!("could not reach {}: {e}", provider.name)))?;
    let status = response.status().as_u16();
    let upstream_model = body["model"].as_str().unwrap_or_default().to_string();

    if status >= 400 {
        let text = response.text().unwrap_or_default();
        let mut payload = serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|_| serde_json::json!({ "error": { "message": text } }));
        // A gateway can refuse with a status and no body at all. Passing that on
        // leaves the client with `{"error":{"message":""}}`, which says nothing;
        // name the provider and the status instead.
        let explained = payload
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .map(str::trim)
            .unwrap_or_default()
            .to_string();
        if explained.is_empty() {
            payload = serde_json::json!({
                "error": {
                    "message": format!("{} returned HTTP {status} with no explanation", provider.name),
                    "type": "upstream_error",
                    "code": status,
                }
            });
        }
        // Only the attempt that owns the connection may report a failure. A
        // fallback candidate leaves the socket clean so the next member can
        // still produce the one reply the client will read.
        if commit {
            write_json(stream, status, &payload).map_err(|e| Failure::new(500, e.to_string()))?;
        }
        return Ok(Done {
            status,
            usage: Usage::default(),
            streamed: false,
            upstream: upstream_model,
            provider: provider.name.clone(),
            billed: billed.to_string(),
            ttft_ms: 0,
            estimated: false,
        });
    }

    if wants_stream {
        write_stream_head(stream).map_err(|e| Failure::new(500, e.to_string()))?;
        let mut window = String::new();
        let mut counter = crate::ai_logs::TokenCounter::default();
        let mut buffer = [0u8; 8192];
        loop {
            match response.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    // The first byte out of the provider is the first token time
                    // — measured here, before the write, so a slow client cannot
                    // be mistaken for a slow model.
                    crate::ai_logs::first_token();
                    if stream.write_all(&buffer[..n]).is_err() || stream.flush().is_err() {
                        // The client hung up. The call succeeded; the reader left.
                        break;
                    }
                    let chunk = String::from_utf8_lossy(&buffer[..n]);
                    // Two readers of the same bytes: the token counter feeds the
                    // live display, and the rolling window feeds the usage sniff
                    // at the end. They are separate because the window is capped
                    // and the counter must not miss a token to that cap.
                    counter.feed(&chunk);
                    crate::ai_logs::tokens(counter.tokens());
                    // Token counts ride inside the event stream, so they are
                    // sniffed from the frames as they pass. A miss only means the
                    // log shows no usage for a streamed call.
                    window.push_str(&chunk);
                    if window.len() > 1 << 20 {
                        window.drain(..window.len() - (1 << 16));
                    }
                }
                Err(e) => {
                    log(&format!("upstream stream ended early: {e}"));
                    break;
                }
            }
        }
        let reported = sniff_usage(&window);
        // A streamed reply often carries no usage block at all, so the count of
        // content deltas is the fallback — labelled as an estimate downstream.
        let estimated = reported.output_tokens == 0 && counter.tokens() > 0;
        let usage = if estimated {
            Usage {
                input_tokens: reported.input_tokens,
                output_tokens: counter.tokens(),
            }
        } else {
            reported
        };
        return Ok(Done {
            status: 200,
            usage,
            streamed: true,
            upstream: upstream_model,
            provider: provider.name.clone(),
            billed: billed.to_string(),
            ttft_ms: crate::ai_logs::current()
                .and_then(|live| live.ttft())
                .unwrap_or(0),
            estimated,
        });
    }

    let text = response.text().unwrap_or_default();
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .unwrap_or_else(|_| serde_json::json!({ "error": { "message": text } }));
    let usage = usage_of(&parsed);
    let bytes = serde_json::to_vec(&parsed).unwrap_or_default();
    write_response(stream, 200, "application/json", &bytes)
        .map_err(|e| Failure::new(500, e.to_string()))?;
    Ok(Done {
        status: 200,
        usage,
        streamed: false,
        upstream: upstream_model,
        provider: provider.name.clone(),
        billed: billed.to_string(),
        ttft_ms: 0,
        estimated: false,
    })
}

fn write_stream_head(sink: &mut dyn Write) -> std::io::Result<()> {
    sink.write_all(
        b"HTTP/1.1 200 OK\r\n\
          content-type: text/event-stream\r\n\
          cache-control: no-cache\r\n\
          access-control-allow-origin: *\r\n\
          connection: close\r\n\r\n",
    )?;
    sink.flush()
}

// ---- combos ---------------------------------------------------------------

fn call_combo(
    stream: &mut TcpStream,
    settings: &crate::settings::Settings,
    catalogue: &Catalogue,
    combo: &Combo,
    body: &serde_json::Value,
    protocol: Protocol,
) -> Result<Done, Failure> {
    let members: Vec<PublishedModel> = combo
        .members
        .iter()
        .filter_map(|id| catalogue.models.iter().find(|m| &m.id == id).cloned())
        .collect();
    if members.is_empty() {
        return Err(Failure::new(
            503,
            format!(
                "combo '{}' names {} model(s) that are no longer published. \
                 Edit the combo, or re-add the provider models it points at.",
                combo.name,
                combo.members.len()
            ),
        ));
    }

    match combo.strategy {
        ComboStrategy::RoundRobin => {
            let pick = next_round_robin(&combo.name, members.len());
            let model = &members[pick];
            let provider = provider_of(settings, model)?;
            // The pick is the answer, so it owns the connection.
            call_single(stream, settings, &provider, model, body, protocol, true)
        }
        ComboStrategy::Fallback => {
            let mut last: Option<Failure> = None;
            for model in &members {
                let Ok(provider) = provider_of(settings, model) else {
                    continue;
                };
                // No member may commit a failure: the last one tried is not
                // necessarily the one that answers, and a candidate that wrote
                // its error would block every member after it.
                match call_single(stream, settings, &provider, model, body, protocol, false) {
                    Ok(done) if done.status < 400 => return Ok(done),
                    Ok(done) => {
                        last = Some(Failure::upstream(format!(
                            "{} answered HTTP {}",
                            model.id, done.status
                        )));
                    }
                    Err(failure) => {
                        log(&format!(
                            "combo {}: {} failed: {}",
                            combo.name, model.id, failure.message
                        ));
                        last = Some(failure);
                    }
                }
            }
            Err(last.unwrap_or_else(|| {
                Failure::new(503, format!("every model in combo '{}' failed", combo.name))
            }))
        }
        ComboStrategy::Fusion => {
            call_fusion(stream, settings, catalogue, combo, &members, body, protocol)
        }
    }
}

/// Round-robin slot for this call.
///
/// A per-combo counter rather than a hash of the request: rotation is the point,
/// and a hash would pin a client to one member for as long as it keeps asking.
fn next_round_robin(combo: &str, len: usize) -> usize {
    static COUNTERS: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    let map = COUNTERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = match map.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let slot = guard.entry(combo.to_string()).or_insert(0);
    let pick = *slot % len.max(1);
    *slot = slot.wrapping_add(1);
    pick
}

/// Fusion: ask every member at once, then let the judge write one answer.
fn call_fusion(
    stream: &mut TcpStream,
    settings: &crate::settings::Settings,
    catalogue: &Catalogue,
    combo: &Combo,
    members: &[PublishedModel],
    body: &serde_json::Value,
    protocol: Protocol,
) -> Result<Done, Failure> {
    let canon = Canon::parse(protocol, body)?;
    let mut answers: Vec<(String, String, Answer)> = Vec::new();

    // In parallel: the point of fusion is that nobody waits for the slowest
    // member to finish before the fastest one is used.
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for model in members {
            let Ok(provider) = provider_of(settings, model) else {
                continue;
            };
            let outbound = canon.emit(provider.kind, &model.upstream);
            handles.push((
                model.clone(),
                provider.name.clone(),
                scope.spawn(move || send(settings, &provider, &outbound)),
            ));
        }
        for (model, provider_name, handle) in handles {
            match handle.join() {
                Ok(Ok((status, text))) if status < 400 => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                        let answer = Answer::parse(kind_of(settings, &model), &value);
                        if !answer.text.trim().is_empty() {
                            answers.push((model.id.clone(), provider_name, answer));
                        }
                    }
                }
                Ok(Ok((status, text))) => log(&format!(
                    "fusion {}: {} answered HTTP {status}: {}",
                    combo.name,
                    model.id,
                    crate::settings::shorten(&text, 200)
                )),
                Ok(Err(failure)) => log(&format!(
                    "fusion {}: {} failed: {}",
                    combo.name, model.id, failure.message
                )),
                Err(_) => log(&format!("fusion {}: {} panicked", combo.name, model.id)),
            }
        }
    });

    if answers.is_empty() {
        return Err(Failure::new(
            503,
            format!("every model in combo '{}' failed", combo.name),
        ));
    }

    // One survivor needs no judge: synthesising an answer from a single source
    // only adds a round trip and a chance to lose detail.
    if answers.len() == 1 {
        let (model_id, provider_name, mut answer) = answers.remove(0);
        answer.finish_reason = "stop".to_string();
        return write_answer(stream, protocol, &answer, &model_id, &provider_name);
    }

    let judge_id = if combo.judge.trim().is_empty() {
        answers[0].0.clone()
    } else {
        combo.judge.clone()
    };
    let judge_target = match catalogue.resolve(&judge_id) {
        Some(Target::Single(m)) => m,
        _ => members[0].clone(),
    };
    let judge = provider_of(settings, &judge_target)?;

    // Sources are anonymised so the judge weighs answers on their merits rather
    // than on which vendor produced them.
    let mut prompt = String::from(
        "Several models were asked the same question. Merge their answers into one \
         reply that is accurate and complete. Prefer agreement; where they conflict, \
         keep the claim the majority supports and say what is uncertain. Answer in \
         the same language the question was asked in, and do not mention that \
         multiple answers exist.\n",
    );
    for (index, (_, _, answer)) in answers.iter().enumerate() {
        prompt.push_str(&format!("\n--- answer {} ---\n{}\n", index + 1, answer.text));
    }

    let synthesis = Canon {
        system: Vec::new(),
        messages: vec![CMsg {
            role: "user".to_string(),
            parts: vec![CPart::Text(prompt)],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }],
        tools: Vec::new(),
        max_tokens: canon.max_tokens,
        temperature: canon.temperature,
        stream: false,
    };
    let outbound = synthesis.emit(judge.kind, &judge_target.upstream);
    let (status, text) = send(settings, &judge, &outbound)?;
    if status >= 400 {
        return Err(Failure::upstream(upstream_message(
            &judge.name,
            status,
            &text,
        )));
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Failure::upstream(format!("{} returned invalid JSON: {e}", judge.name)))?;
    let mut answer = Answer::parse(judge.kind, &parsed);
    answer.finish_reason = "stop".to_string();
    write_answer(stream, protocol, &answer, &judge_target.id, &judge.name)
}

/// The provider kind behind a published model, for parsing its reply.
fn kind_of(settings: &crate::settings::Settings, model: &PublishedModel) -> ApiKind {
    settings
        .providers
        .iter()
        .find(|p| p.id == model.provider_id)
        .map(|p| p.kind)
        .unwrap_or(ApiKind::ChatCompletions)
}

/// Render and write a finished answer.
fn write_answer(
    stream: &mut TcpStream,
    protocol: Protocol,
    answer: &Answer,
    model_id: &str,
    provider: &str,
) -> Result<Done, Failure> {
    let rendered = answer.emit(protocol, model_id);
    write_json(stream, 200, &rendered).map_err(|e| Failure::new(500, e.to_string()))?;
    Ok(Done {
        status: 200,
        usage: answer.usage,
        streamed: false,
        upstream: model_id.to_string(),
        provider: provider.to_string(),
        billed: model_id.to_string(),
        // Fusion answers with one buffered reply: the judge read every member's
        // answer in full before writing a word, so a "first token" here would
        // measure the whole fan-out and call it latency.
        ttft_ms: 0,
        estimated: false,
    })
}

/// A provider's explanation, flattened into one sentence.
fn upstream_message(name: &str, status: u16, text: &str) -> String {
    let explained = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").or_else(|| e.get("type")))
                .and_then(|m| m.as_str())
                .map(str::to_string)
                .or_else(|| v.get("message").and_then(|m| m.as_str()).map(str::to_string))
        })
        .unwrap_or_else(|| text.trim().to_string());
    format!(
        "{name} answered HTTP {status}: {}",
        crate::settings::shorten(&explained, 400)
    )
}

// ---- token accounting -----------------------------------------------------

#[derive(Debug, Default, Clone, Copy)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

impl Usage {
    fn total(self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Token counts out of a parsed reply, whichever protocol produced it.
fn usage_of(body: &serde_json::Value) -> Usage {
    let Some(usage) = body.get("usage") else {
        return Usage::default();
    };
    let number = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(|v| v.as_u64()))
            .unwrap_or(0)
    };
    Usage {
        input_tokens: number(&["prompt_tokens", "input_tokens"]),
        output_tokens: number(&["completion_tokens", "output_tokens"]),
    }
}

/// Find token counts in a partially accumulated event stream.
///
/// Scans the buffer for the last `"usage":{…}` object that mentions tokens. Cheap
/// enough for a stream that is trimmed as it grows.
fn sniff_usage(buffer: &str) -> Usage {
    let mut best = Usage::default();
    let mut search = 0usize;
    while let Some(pos) = buffer[search..].find("\"usage\"") {
        let at = search + pos;
        if let Some((_, rest)) = buffer[at..].split_once(':') {
            let tail = rest.trim_start();
            if tail.starts_with('{')
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(take_object(tail))
            {
                let found = usage_of(&serde_json::json!({ "usage": value }));
                if found.total() > 0 {
                    best = found;
                }
            }
        }
        search = at + "\"usage\"".len();
        if search >= buffer.len() {
            break;
        }
    }
    best
}

/// The balanced `{...}` at the start of `text`, as far as it is complete.
fn take_object(text: &str) -> &str {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[..=i];
                }
            }
            _ => {}
        }
    }
    text
}

// ---- canonical request ----------------------------------------------------

#[derive(Debug, Clone)]
enum CPart {
    Text(String),
    Image { mime: String, data: String },
}

#[derive(Debug, Clone)]
struct CMsg {
    /// `user`, `assistant` or `tool`.
    role: String,
    parts: Vec<CPart>,
    /// [OI]-shaped tool calls on an assistant turn.
    tool_calls: Vec<serde_json::Value>,
    tool_call_id: Option<String>,
}

/// A request, independent of which protocol it arrived in.
#[derive(Debug, Clone, Default)]
struct Canon {
    system: Vec<String>,
    messages: Vec<CMsg>,
    /// Tool definitions in the [OI] function shape, which both other protocols
    /// can be derived from.
    tools: Vec<serde_json::Value>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    stream: bool,
}

impl Canon {
    fn parse(protocol: Protocol, body: &serde_json::Value) -> Result<Canon, Failure> {
        let mut canon = match protocol {
            Protocol::Chat => parse_chat(body),
            Protocol::Responses => parse_responses(body),
            Protocol::Anthropic => parse_anthropic(body),
        };
        canon.max_tokens = body
            .get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .or_else(|| body.get("max_output_tokens"))
            .and_then(|v| v.as_u64())
            .or(canon.max_tokens);
        canon.temperature = body
            .get("temperature")
            .and_then(|v| v.as_f64())
            .or(canon.temperature);
        canon.stream = body
            .get("stream")
            .and_then(|v| v.as_bool())
            .unwrap_or(canon.stream);
        if canon.messages.is_empty() {
            return Err(Failure::new(400, "the request has no messages."));
        }
        Ok(canon)
    }

    /// Render this request in the provider's protocol.
    fn emit(&self, kind: ApiKind, model: &str) -> serde_json::Value {
        match kind {
            ApiKind::ChatCompletions => {
                let mut messages: Vec<serde_json::Value> = Vec::new();
                let system = self.system.join("\n\n");
                if !system.trim().is_empty() {
                    messages.push(serde_json::json!({ "role": "system", "content": system }));
                }
                for msg in &self.messages {
                    messages.push(msg.emit_chat());
                }
                let mut body = serde_json::json!({ "model": model, "messages": messages });
                if let Some(max) = self.max_tokens {
                    body["max_tokens"] = serde_json::json!(max);
                }
                if let Some(temp) = self.temperature {
                    body["temperature"] = serde_json::json!(temp);
                }
                if !self.tools.is_empty() {
                    body["tools"] = serde_json::Value::Array(self.tools.clone());
                }
                body
            }
            ApiKind::Anthropic => {
                let messages: Vec<serde_json::Value> =
                    self.messages.iter().map(CMsg::emit_anthropic).collect();
                let mut body = serde_json::json!({
                    "model": model,
                    // Anthropic requires a budget; the chat panel's default is
                    // 4096, and a client that named one keeps it.
                    "max_tokens": self.max_tokens.unwrap_or(4096),
                    "messages": messages,
                });
                let system = self.system.join("\n\n");
                if !system.trim().is_empty() {
                    body["system"] = serde_json::json!(system);
                }
                if let Some(temp) = self.temperature {
                    body["temperature"] = serde_json::json!(temp);
                }
                if !self.tools.is_empty() {
                    body["tools"] = serde_json::Value::Array(
                        self.tools
                            .iter()
                            .filter_map(|tool| {
                                let f = tool.get("function")?;
                                Some(serde_json::json!({
                                    "name": f.get("name").cloned().unwrap_or_default(),
                                    "description": f.get("description").cloned().unwrap_or_default(),
                                    "input_schema": f
                                        .get("parameters")
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                                }))
                            })
                            .collect(),
                    );
                }
                body
            }
            ApiKind::Responses => {
                let items: Vec<serde_json::Value> = self
                    .messages
                    .iter()
                    .map(|msg| {
                        let role = match msg.role.as_str() {
                            "assistant" => "assistant",
                            // The Responses API has no tool role; a tool result
                            // rides as a user turn.
                            _ => "user",
                        };
                        serde_json::json!({
                            "type": "message",
                            "role": role,
                            "content": msg.text_only(),
                        })
                    })
                    .collect();
                let mut body = serde_json::json!({ "model": model, "input": items });
                let system = self.system.join("\n\n");
                if !system.trim().is_empty() {
                    body["instructions"] = serde_json::json!(system);
                }
                if let Some(max) = self.max_tokens {
                    body["max_output_tokens"] = serde_json::json!(max);
                }
                if let Some(temp) = self.temperature {
                    body["temperature"] = serde_json::json!(temp);
                }
                if !self.tools.is_empty() {
                    body["tools"] = serde_json::Value::Array(
                        self.tools
                            .iter()
                            .filter_map(|tool| {
                                let f = tool.get("function")?;
                                Some(serde_json::json!({
                                    "type": "function",
                                    "name": f.get("name").cloned().unwrap_or_default(),
                                    "description": f.get("description").cloned().unwrap_or_default(),
                                    "parameters": f
                                        .get("parameters")
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                                }))
                            })
                            .collect(),
                    );
                }
                body
            }
        }
    }
}

impl CMsg {
    fn text_only(&self) -> String {
        self.parts
            .iter()
            .filter_map(|p| match p {
                CPart::Text(t) => Some(t.as_str()),
                CPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn emit_chat(&self) -> serde_json::Value {
        if self.role == "tool" {
            return serde_json::json!({
                "role": "tool",
                "tool_call_id": self.tool_call_id.clone().unwrap_or_default(),
                "content": self.text_only(),
            });
        }
        let images: Vec<serde_json::Value> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                CPart::Image { mime, data } => Some(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": format!("data:{mime};base64,{data}") },
                })),
                CPart::Text(_) => None,
            })
            .collect();
        let content = if images.is_empty() {
            serde_json::json!(self.text_only())
        } else {
            let mut parts: Vec<serde_json::Value> =
                vec![serde_json::json!({ "type": "text", "text": self.text_only() })];
            parts.extend(images);
            serde_json::json!(parts)
        };
        let mut out = serde_json::json!({
            "role": if self.role == "assistant" { "assistant" } else { "user" },
            "content": content,
        });
        if !self.tool_calls.is_empty() {
            out["content"] = serde_json::Value::Null;
            out["tool_calls"] = serde_json::Value::Array(self.tool_calls.clone());
        }
        out
    }

    fn emit_anthropic(&self) -> serde_json::Value {
        if self.role == "tool" {
            return serde_json::json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": self.tool_call_id.clone().unwrap_or_default(),
                    "content": self.text_only(),
                }],
            });
        }
        let images: Vec<serde_json::Value> = self
            .parts
            .iter()
            .filter_map(|p| match p {
                CPart::Image { mime, data } => Some(serde_json::json!({
                    "type": "image",
                    "source": { "type": "base64", "media_type": mime, "data": data },
                })),
                CPart::Text(_) => None,
            })
            .collect();
        if self.tool_calls.is_empty() && images.is_empty() {
            return serde_json::json!({
                "role": if self.role == "assistant" { "assistant" } else { "user" },
                "content": self.text_only(),
            });
        }
        let mut parts: Vec<serde_json::Value> = Vec::new();
        if !self.text_only().is_empty() {
            parts.push(serde_json::json!({ "type": "text", "text": self.text_only() }));
        }
        parts.extend(images);
        for call in &self.tool_calls {
            parts.push(serde_json::json!({
                "type": "tool_use",
                "id": call.get("id").cloned().unwrap_or_default(),
                "name": call
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .cloned()
                    .unwrap_or_default(),
                "input": call
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .unwrap_or_else(|| serde_json::json!({})),
            }));
        }
        serde_json::json!({
            "role": if self.role == "assistant" { "assistant" } else { "user" },
            "content": parts,
        })
    }
}

/// Text out of a `content` value, whether it is a string or a parts array.
fn content_parts(content: Option<&serde_json::Value>) -> Vec<CPart> {
    match content {
        Some(serde_json::Value::String(s)) => vec![CPart::Text(s.clone())],
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| match p.get("type").and_then(|t| t.as_str()) {
                Some("text") | Some("input_text") | Some("output_text") => p
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(|t| CPart::Text(t.to_string())),
                Some("image_url") => {
                    let url = p
                        .get("image_url")
                        .and_then(|i| i.get("url"))
                        .and_then(|u| u.as_str())
                        .or_else(|| p.get("image_url").and_then(|u| u.as_str()))
                        .unwrap_or_default();
                    image_from_data_url(url)
                }
                Some("input_image") => {
                    let url = p.get("image_url").and_then(|u| u.as_str()).unwrap_or_default();
                    image_from_data_url(url)
                }
                Some("image") => {
                    let source = p.get("source")?;
                    let mime = source
                        .get("media_type")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png")
                        .to_string();
                    let data = source.get("data").and_then(|d| d.as_str())?.to_string();
                    Some(CPart::Image { mime, data })
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn image_from_data_url(url: &str) -> Option<CPart> {
    let rest = url.strip_prefix("data:")?;
    let (meta, data) = rest.split_once(',')?;
    let mime = meta.split(';').next().unwrap_or("image/png").to_string();
    Some(CPart::Image {
        mime,
        data: data.to_string(),
    })
}

fn parse_chat(body: &serde_json::Value) -> Canon {
    let mut canon = Canon::default();
    let Some(list) = body.get("messages").and_then(|m| m.as_array()) else {
        return canon;
    };
    for message in list {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        if role == "system" || role == "developer" {
            let text = content_parts(message.get("content"))
                .iter()
                .filter_map(|p| match p {
                    CPart::Text(t) => Some(t.clone()),
                    CPart::Image { .. } => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.trim().is_empty() {
                canon.system.push(text);
            }
            continue;
        }
        let tool_calls: Vec<serde_json::Value> = message
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        canon.messages.push(CMsg {
            role: match role {
                "assistant" => "assistant".to_string(),
                "tool" | "function" => "tool".to_string(),
                _ => "user".to_string(),
            },
            parts: content_parts(message.get("content")),
            tool_calls,
            tool_call_id: message
                .get("tool_call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }
    canon.tools = body
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.get("function").is_some())
        .collect();
    canon
}

fn parse_anthropic(body: &serde_json::Value) -> Canon {
    let mut canon = Canon::default();
    match body.get("system") {
        Some(serde_json::Value::String(s)) => canon.system.push(s.clone()),
        Some(serde_json::Value::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n");
            if !text.trim().is_empty() {
                canon.system.push(text);
            }
        }
        _ => {}
    }
    let Some(list) = body.get("messages").and_then(|m| m.as_array()) else {
        return canon;
    };
    for message in list {
        let role = message.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let mut parts: Vec<CPart> = Vec::new();
        let mut tool_calls: Vec<serde_json::Value> = Vec::new();
        let mut tool_result: Option<String> = None;
        let mut tool_use_id: Option<String> = None;
        for block in message
            .get("content")
            .map(|c| match c {
                serde_json::Value::Array(list) => list.clone(),
                other => vec![other.clone()],
            })
            .unwrap_or_default()
        {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => tool_calls.push(serde_json::json!({
                    "id": block.get("id").cloned().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": block.get("name").cloned().unwrap_or_default(),
                        "arguments": block
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string()),
                    },
                })),
                // A tool result arrives as a user turn in Anthropic's shape; it
                // becomes a `tool` message so the other protocols see the same
                // thing they would have sent themselves.
                Some("tool_result") => {
                    tool_use_id = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    tool_result = Some(
                        block
                            .get("content")
                            .map(|c| {
                                content_parts(Some(c))
                                    .iter()
                                    .filter_map(|p| match p {
                                        CPart::Text(t) => Some(t.clone()),
                                        CPart::Image { .. } => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default(),
                    );
                }
                _ => parts.extend(content_parts(Some(&block))),
            }
        }
        if let Some(text) = tool_result {
            canon.messages.push(CMsg {
                role: "tool".to_string(),
                parts: vec![CPart::Text(text)],
                tool_calls: Vec::new(),
                tool_call_id: tool_use_id,
            });
            continue;
        }
        canon.messages.push(CMsg {
            role: if role == "assistant" {
                "assistant".to_string()
            } else {
                "user".to_string()
            },
            parts,
            tool_calls,
            tool_call_id: None,
        });
    }
    canon.tools = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|list| {
            list.iter()
                .map(|tool| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": tool.get("name").cloned().unwrap_or_default(),
                            "description": tool.get("description").cloned().unwrap_or_default(),
                            "parameters": tool
                                .get("input_schema")
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    canon
}

fn parse_responses(body: &serde_json::Value) -> Canon {
    let mut canon = Canon::default();
    if let Some(instructions) = body.get("instructions").and_then(|i| i.as_str())
        && !instructions.trim().is_empty()
    {
        canon.system.push(instructions.to_string());
    }
    match body.get("input") {
        // A plain string input is the whole conversation.
        Some(serde_json::Value::String(text)) => canon.messages.push(CMsg {
            role: "user".to_string(),
            parts: vec![CPart::Text(text.clone())],
            tool_calls: Vec::new(),
            tool_call_id: None,
        }),
        Some(serde_json::Value::Array(items)) => {
            for item in items {
                let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("user");
                canon.messages.push(CMsg {
                    role: if role == "assistant" {
                        "assistant".to_string()
                    } else {
                        "user".to_string()
                    },
                    parts: content_parts(item.get("content")),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                });
            }
        }
        _ => {}
    }
    canon.tools = body
        .get("tools")
        .and_then(|t| t.as_array())
        .map(|list| {
            list.iter()
                .filter(|t| t.get("type").and_then(|v| v.as_str()) == Some("function"))
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.get("name").cloned().unwrap_or_default(),
                            "description": t.get("description").cloned().unwrap_or_default(),
                            "parameters": t
                                .get("parameters")
                                .cloned()
                                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                        }
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    canon
}

// ---- canonical reply ------------------------------------------------------

/// A reply, independent of which protocol produced it.
#[derive(Debug, Clone, Default)]
struct Answer {
    text: String,
    /// [OI]-shaped tool calls.
    tool_calls: Vec<serde_json::Value>,
    /// `stop`, `tool_calls` or `length`.
    finish_reason: String,
    usage: Usage,
}

impl Answer {
    fn parse(kind: ApiKind, body: &serde_json::Value) -> Answer {
        let mut answer = Answer {
            usage: usage_of(body),
            finish_reason: "stop".to_string(),
            ..Answer::default()
        };
        match kind {
            ApiKind::ChatCompletions => {
                if let Some(choice) = body.get("choices").and_then(|c| c.get(0)) {
                    let message = choice.get("message").unwrap_or(&serde_json::Value::Null);
                    answer.text = message
                        .get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_string();
                    answer.tool_calls = message
                        .get("tool_calls")
                        .and_then(|t| t.as_array())
                        .cloned()
                        .unwrap_or_default();
                    if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
                        answer.finish_reason = reason.to_string();
                    }
                }
            }
            ApiKind::Anthropic => {
                if let Some(blocks) = body.get("content").and_then(|c| c.as_array()) {
                    for block in blocks {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(chunk) = block.get("text").and_then(|t| t.as_str()) {
                                    answer.text.push_str(chunk);
                                }
                            }
                            Some("tool_use") => answer.tool_calls.push(serde_json::json!({
                                "id": block.get("id").cloned().unwrap_or_default(),
                                "type": "function",
                                "function": {
                                    "name": block.get("name").cloned().unwrap_or_default(),
                                    "arguments": block
                                        .get("input")
                                        .map(|i| i.to_string())
                                        .unwrap_or_else(|| "{}".to_string()),
                                },
                            })),
                            _ => {}
                        }
                    }
                }
                answer.finish_reason = match body.get("stop_reason").and_then(|r| r.as_str()) {
                    Some("tool_use") => "tool_calls".to_string(),
                    Some("max_tokens") => "length".to_string(),
                    _ => "stop".to_string(),
                };
            }
            ApiKind::Responses => {
                answer.text = body
                    .get("output_text")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        body.get("output")
                            .and_then(|o| o.as_array())
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(|item| {
                                        item.get("content").and_then(|c| c.as_array()).map(
                                            |parts| {
                                                parts
                                                    .iter()
                                                    .filter_map(|p| {
                                                        p.get("text").and_then(|t| t.as_str())
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join("")
                                            },
                                        )
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default()
                    });
                if let Some(items) = body.get("output").and_then(|o| o.as_array()) {
                    for item in items {
                        if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                            answer.tool_calls.push(serde_json::json!({
                                "id": item.get("call_id").cloned().unwrap_or_default(),
                                "type": "function",
                                "function": {
                                    "name": item.get("name").cloned().unwrap_or_default(),
                                    "arguments": item
                                        .get("arguments")
                                        .cloned()
                                        .unwrap_or_else(|| serde_json::json!("{}")),
                                },
                            }));
                        }
                    }
                }
                if !answer.tool_calls.is_empty() {
                    answer.finish_reason = "tool_calls".to_string();
                }
            }
        }
        answer
    }

    /// Render the reply in the client's protocol.
    fn emit(&self, protocol: Protocol, model: &str) -> serde_json::Value {
        let created = crate::ai_logs::now_ms() / 1000;
        match protocol {
            Protocol::Chat => {
                let mut message = serde_json::json!({ "role": "assistant", "content": self.text });
                if !self.tool_calls.is_empty() {
                    message["content"] = serde_json::Value::Null;
                    message["tool_calls"] = serde_json::Value::Array(self.tool_calls.clone());
                }
                serde_json::json!({
                    "id": format!("chatcmpl-{created}"),
                    "object": "chat.completion",
                    "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": message,
                        "finish_reason": self.finish_reason,
                    }],
                    "usage": {
                        "prompt_tokens": self.usage.input_tokens,
                        "completion_tokens": self.usage.output_tokens,
                        "total_tokens": self.usage.total(),
                    },
                })
            }
            Protocol::Anthropic => {
                let mut content: Vec<serde_json::Value> = Vec::new();
                if !self.text.is_empty() {
                    content.push(serde_json::json!({ "type": "text", "text": self.text }));
                }
                for call in &self.tool_calls {
                    content.push(serde_json::json!({
                        "type": "tool_use",
                        "id": call.get("id").cloned().unwrap_or_default(),
                        "name": call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .cloned()
                            .unwrap_or_default(),
                        "input": call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| a.as_str())
                            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                            .unwrap_or_else(|| serde_json::json!({})),
                    }));
                }
                if content.is_empty() {
                    content.push(serde_json::json!({ "type": "text", "text": "" }));
                }
                serde_json::json!({
                    "id": format!("msg_{created}"),
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": content,
                    "stop_reason": if self.finish_reason == "tool_calls" { "tool_use" } else { "end_turn" },
                    "usage": {
                        "input_tokens": self.usage.input_tokens,
                        "output_tokens": self.usage.output_tokens,
                    },
                })
            }
            Protocol::Responses => {
                let mut output: Vec<serde_json::Value> = vec![serde_json::json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": self.text }],
                })];
                for call in &self.tool_calls {
                    output.push(serde_json::json!({
                        "type": "function_call",
                        "call_id": call.get("id").cloned().unwrap_or_default(),
                        "name": call
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .cloned()
                            .unwrap_or_default(),
                        "arguments": call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!("{}")),
                    }));
                }
                serde_json::json!({
                    "id": format!("resp_{created}"),
                    "object": "response",
                    "created_at": created,
                    "model": model,
                    "status": "completed",
                    "output": output,
                    "output_text": self.text,
                    "usage": {
                        "input_tokens": self.usage.input_tokens,
                        "output_tokens": self.usage.output_tokens,
                        "total_tokens": self.usage.total(),
                    },
                })
            }
        }
    }

    /// A synthesised event stream in the client's protocol.
    ///
    /// Used when the request had to be translated: re-framing a provider's own
    /// events into another protocol's grammar as they arrive is a second
    /// implementation of every protocol, so a translated request is answered in
    /// one piece and emitted as a short, correct event sequence instead.
    fn sse(&self, protocol: Protocol, model: &str) -> String {
        let created = crate::ai_logs::now_ms() / 1000;
        let mut out = String::new();
        match protocol {
            Protocol::Chat => {
                let head = serde_json::json!({
                    "id": format!("chatcmpl-{created}"),
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": { "role": "assistant" }, "finish_reason": null }],
                });
                out.push_str(&format!("data: {head}\n\n"));
                if !self.text.is_empty() {
                    let chunk = serde_json::json!({
                        "id": format!("chatcmpl-{created}"),
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": model,
                        "choices": [{ "index": 0, "delta": { "content": self.text }, "finish_reason": null }],
                    });
                    out.push_str(&format!("data: {chunk}\n\n"));
                }
                let tail = serde_json::json!({
                    "id": format!("chatcmpl-{created}"),
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": self.finish_reason }],
                    "usage": {
                        "prompt_tokens": self.usage.input_tokens,
                        "completion_tokens": self.usage.output_tokens,
                        "total_tokens": self.usage.total(),
                    },
                });
                out.push_str(&format!("data: {tail}\n\n"));
                out.push_str("data: [DONE]\n\n");
            }
            Protocol::Anthropic => {
                let start = serde_json::json!({
                    "type": "message_start",
                    "message": {
                        "id": format!("msg_{created}"),
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": null,
                        "usage": { "input_tokens": self.usage.input_tokens, "output_tokens": 0 },
                    }
                });
                out.push_str(&format!("event: message_start\ndata: {start}\n\n"));
                out.push_str(
                    "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                );
                let delta = serde_json::json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": self.text },
                });
                out.push_str(&format!("event: content_block_delta\ndata: {delta}\n\n"));
                out.push_str(
                    "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                );
                let message_delta = serde_json::json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": if self.finish_reason == "tool_calls" { "tool_use" } else { "end_turn" },
                    },
                    "usage": { "output_tokens": self.usage.output_tokens },
                });
                out.push_str(&format!("event: message_delta\ndata: {message_delta}\n\n"));
                out.push_str("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
            }
            Protocol::Responses => {
                let added = serde_json::json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "" }],
                    }
                });
                out.push_str(&format!("event: response.output_item.added\ndata: {added}\n\n"));
                let delta = serde_json::json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "delta": self.text,
                });
                out.push_str(&format!("event: response.output_text.delta\ndata: {delta}\n\n"));
                let done = serde_json::json!({
                    "type": "response.completed",
                    "response": self.emit(Protocol::Responses, model),
                });
                out.push_str(&format!("event: response.completed\ndata: {done}\n\n"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Combo, Provider, RouterKey, RouterSettings};

    fn published(id: &str, upstream: &str) -> PublishedModel {
        PublishedModel {
            id: id.to_string(),
            provider_id: "p1".to_string(),
            provider_name: "Risun".to_string(),
            upstream: upstream.to_string(),
        }
    }

    fn catalogue() -> Catalogue {
        Catalogue {
            models: vec![published("risun/bansos/deepseek-v4.1", "bansos/deepseek-v4.1")],
            combos: vec![Combo {
                id: "c1".to_string(),
                name: "fast".to_string(),
                members: vec!["risun/bansos/deepseek-v4.1".to_string()],
                enabled: true,
                ..Combo::default()
            }],
        }
    }

    fn key(models: Vec<&str>) -> RouterKey {
        RouterKey {
            id: "k1".to_string(),
            name: "Laptop".to_string(),
            key: "sk-rbx-abc".to_string(),
            enabled: true,
            models: models.into_iter().map(str::to_string).collect(),
            created_at: 0,
        }
    }

    #[test]
    fn a_prefixed_id_resolves_to_the_model_behind_it() {
        match catalogue().resolve("risun/bansos/deepseek-v4.1") {
            Some(Target::Single(m)) => {
                assert_eq!(m.upstream, "bansos/deepseek-v4.1");
                assert_eq!(m.provider_name, "Risun");
            }
            _ => panic!("the prefixed id did not resolve"),
        }
    }

    #[test]
    fn a_combo_answers_to_both_spellings() {
        let cat = catalogue();
        assert!(matches!(cat.resolve("combo/fast"), Some(Target::Group(_))));
        assert!(matches!(cat.resolve("fast"), Some(Target::Group(_))));
    }

    #[test]
    fn an_unknown_id_does_not_resolve() {
        assert!(catalogue().resolve("nope/nothing").is_none());
    }

    #[test]
    fn a_key_with_no_patterns_may_use_everything() {
        assert!(key(vec![]).allows("risun/anything"));
    }

    #[test]
    fn a_negative_only_pattern_starts_from_everything() {
        let k = key(vec!["!combo/*"]);
        assert!(k.allows("risun/gpt-4o"));
        assert!(!k.allows("combo/fast"));
    }

    #[test]
    fn a_positive_pattern_limits_access() {
        let k = key(vec!["risun/*"]);
        assert!(k.allows("risun/gpt-4o"));
        assert!(!k.allows("other/gpt-4o"));
    }

    #[test]
    fn an_openai_request_becomes_an_anthropic_one() {
        let body = serde_json::json!({
            "model": "x",
            "messages": [
                { "role": "system", "content": "be brief" },
                { "role": "user", "content": "hi" }
            ],
            "max_tokens": 32,
        });
        let canon = Canon::parse(Protocol::Chat, &body).unwrap();
        assert_eq!(canon.system, vec!["be brief".to_string()]);
        let out = canon.emit(ApiKind::Anthropic, "claude-3");
        assert_eq!(out["model"], "claude-3");
        assert_eq!(out["system"], "be brief");
        assert_eq!(out["max_tokens"], 32);
        assert_eq!(out["messages"][0]["role"], "user");
        assert_eq!(out["messages"][0]["content"], "hi");
    }

    #[test]
    fn an_anthropic_reply_becomes_an_openai_one() {
        let reply = serde_json::json!({
            "content": [{ "type": "text", "text": "hello" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 7, "output_tokens": 3 },
        });
        let answer = Answer::parse(ApiKind::Anthropic, &reply);
        assert_eq!(answer.text, "hello");
        let out = answer.emit(Protocol::Chat, "risun/claude");
        assert_eq!(out["choices"][0]["message"]["content"], "hello");
        assert_eq!(out["usage"]["prompt_tokens"], 7);
        assert_eq!(out["usage"]["completion_tokens"], 3);
    }

    #[test]
    fn a_tool_call_survives_a_trip_through_anthropic() {
        let reply = serde_json::json!({
            "content": [
                { "type": "text", "text": "checking" },
                { "type": "tool_use", "id": "t1", "name": "read_file", "input": { "path": "a" } }
            ],
            "stop_reason": "tool_use",
        });
        let answer = Answer::parse(ApiKind::Anthropic, &reply);
        assert_eq!(answer.finish_reason, "tool_calls");
        let out = answer.emit(Protocol::Chat, "m");
        assert_eq!(out["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            out["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "read_file"
        );
        // The arguments come back as a JSON *string*, which is what [OI] clients
        // parse.
        let args = out["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .expect("arguments must be a string");
        assert!(args.contains("\"path\""));
    }

    #[test]
    fn a_responses_reply_reads_output_text() {
        let reply = serde_json::json!({
            "output_text": "done",
            "usage": { "input_tokens": 2, "output_tokens": 1 },
        });
        let answer = Answer::parse(ApiKind::Responses, &reply);
        assert_eq!(answer.text, "done");
        assert_eq!(answer.usage.input_tokens, 2);
    }

    #[test]
    fn streaming_usage_is_sniffed_out_of_the_event_frames() {
        let buffer = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
                      data: {\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\n";
        let usage = sniff_usage(buffer);
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 4);
    }

    #[test]
    fn a_partial_usage_object_is_not_mistaken_for_a_complete_one() {
        let buffer = "data: {\"usage\":{\"prompt_tokens\":11";
        assert_eq!(sniff_usage(buffer).total(), 0);
    }

    #[test]
    fn a_network_caller_with_no_key_is_refused() {
        let router = RouterSettings::default();
        let failure = authenticate(&router, None, false).expect_err("must refuse");
        assert_eq!(failure.status, 401);
    }

    #[test]
    fn a_loopback_caller_with_no_key_is_served() {
        let router = RouterSettings::default();
        assert!(authenticate(&router, None, true).is_ok());
    }

    #[test]
    fn a_configured_key_must_match_exactly() {
        let router = RouterSettings {
            keys: vec![key(vec![])],
            ..RouterSettings::default()
        };
        assert!(authenticate(&router, Some("sk-rbx-abc"), false).is_ok());
        assert!(authenticate(&router, Some("sk-rbx-abd"), false).is_err());
        assert!(authenticate(&router, None, false).is_err());
        // A disabled key is not a key, so the router falls back to "loopback
        // only" instead of accepting it.
        let disabled = RouterSettings {
            keys: vec![RouterKey {
                enabled: false,
                ..key(vec![])
            }],
            ..RouterSettings::default()
        };
        assert!(authenticate(&disabled, Some("sk-rbx-abc"), true).is_ok());
        assert!(authenticate(&disabled, Some("sk-rbx-abc"), false).is_err());
    }

    #[test]
    fn the_models_list_hides_what_a_key_may_not_use() {
        let mut settings = crate::settings::Settings::default();
        settings.providers.push(Provider {
            id: "p1".to_string(),
            name: "Risun".to_string(),
            prefix: "risun".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            model: "a".to_string(),
            models: vec!["a".to_string(), "b".to_string()],
            ..Provider::default()
        });
        settings.router.keys.push(key(vec!["risun/a"]));
        let cat = Catalogue::of(&settings);
        let body = models_body(&cat, &settings.router, Some("Laptop"));
        let ids: Vec<&str> = body["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["risun/a"]);
    }

    #[test]
    fn the_catalogue_publishes_every_provider_model_under_its_prefix() {
        let mut settings = crate::settings::Settings::default();
        settings.providers.push(Provider {
            id: "p1".to_string(),
            name: "Risun".to_string(),
            prefix: "risun".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            model: "bansos/deepseek-v4.1".to_string(),
            models: vec![
                "bansos/deepseek-v4.1".to_string(),
                "bansos/glm-5".to_string(),
            ],
            ..Provider::default()
        });
        let cat = Catalogue::of(&settings);
        assert_eq!(cat.models.len(), 2);
        assert_eq!(cat.models[0].id, "risun/bansos/deepseek-v4.1");
        assert_eq!(cat.models[1].id, "risun/bansos/glm-5");
    }

    #[test]
    fn a_disabled_combo_is_not_published() {
        let mut settings = crate::settings::Settings::default();
        settings.combos.push(Combo {
            id: "c1".to_string(),
            name: "fast".to_string(),
            members: vec!["risun/a".to_string()],
            enabled: false,
            ..Combo::default()
        });
        assert!(Catalogue::of(&settings).combos.is_empty());
    }

    #[test]
    fn the_estimate_prefers_a_provider_rate_over_the_table() {
        let mut settings = crate::settings::Settings::default();
        settings.providers.push(Provider {
            id: "p1".to_string(),
            name: "Risun".to_string(),
            prefix: "risun".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            input_price: 1.0,
            output_price: 2.0,
            ..Provider::default()
        });
        // One million in, one million out: exactly the configured rates.
        let cost = estimate_cost(&settings, "risun/gpt-4o", 1_000_000, 1_000_000);
        assert!((cost - 3.0).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn the_estimate_falls_back_to_the_published_table() {
        let settings = crate::settings::Settings::default();
        // 4o-mini is 0.15 in / 0.60 out per million.
        let cost = estimate_cost(&settings, "any/gpt-4o-mini", 1_000_000, 0);
        assert!((cost - 0.15).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn an_unknown_model_has_no_estimate_rather_than_a_wrong_one() {
        let settings = crate::settings::Settings::default();
        assert_eq!(estimate_cost(&settings, "x/mystery-model", 1_000_000, 0), 0.0);
    }
}
