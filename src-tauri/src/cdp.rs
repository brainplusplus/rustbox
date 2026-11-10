//! Chrome DevTools Protocol bridge to the embedded tab webviews.
//!
//! Every tab rustbox opens is a real WebView2 instance with its own profile, so
//! it already speaks CDP. WebView2 exposes that through
//! `ICoreWebView2::CallDevToolsProtocolMethod` plus a matching event receiver
//! per event name — no `--remote-debugging-port` needed, and therefore no
//! unauthenticated debugging port on the machine.
//!
//! # Why this is not a straight `await`
//!
//! `with_webview` posts a message to the window's event loop and returns
//! immediately; the closure runs on the main thread later. So a call from the
//! bridge thread has to hand a oneshot sender to the main thread and block on
//! the receiver. Doing it the other way round (calling `with_webview` from
//! inside an IPC command handler) is the wry#583 reentrancy deadlock documented
//! on `tabs::open_embedded_tab`, which is why this module never builds or
//! manipulates a webview from a command handler.
//!
//! # What lives where
//!
//! - [`evaluate`], [`call`] — request/response CDP calls.
//! - [`ConsoleBuffer`], [`NetworkBuffer`] — event streams, capped and read by
//!   cursor so an agent can ask "what happened since N".
//! - [`logs`] — the ring buffers, keyed by app id.
//!
//! Everything here is Windows-only; the WebView2 API is the reason CDP is
//! reachable at all. Other platforms have no equivalent binding in this app, so
//! the bridge reports that instead of pretending.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How long a CDP call may take before the bridge gives up.
///
/// The browser answers in milliseconds; this is a backstop so a wedged webview
/// cannot block an agent forever. It is deliberately generous: `Page.navigate`
/// on a slow site can legitimately take a while to return.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// One console entry as the agent sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsoleEntry {
    /// Monotonic sequence number, so a caller can ask for "since 42".
    pub seq: u64,
    /// `log`, `warning`, `error`, `info`, `debug`, or a CDP type we did not map.
    pub level: String,
    /// Flattened argument text, already truncated to keep responses small.
    pub text: String,
    /// Page URL the entry came from.
    pub url: String,
}

/// One network request as the agent sees it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NetworkEntry {
    pub seq: u64,
    pub method: String,
    pub url: String,
    /// CDP `Network.ResourceType`, e.g. `XHR`, `Fetch`, `Document`.
    pub kind: String,
    /// HTTP status, or 0 when the request failed or is still in flight.
    pub status: u16,
    /// `None` until the response (or failure) arrives.
    pub ok: Option<bool>,
    pub failed: Option<String>,
}

/// Cap on how much of a console argument or URL is kept.
///
/// A page can log a megabyte in one call; an agent that asks for the buffer
/// should not receive it. Truncation is visible via the trailing marker rather
/// than silent.
const MAX_TEXT: usize = 2000;

/// Entries kept per tab. A busy app can log thousands per minute, and the
/// oldest are the least useful once an agent is debugging a specific moment.
const BUFFER_CAP: usize = 500;

/// Truncate to [`MAX_TEXT`] on a character boundary.
fn clip(text: &str) -> String {
    if text.len() <= MAX_TEXT {
        return text.to_string();
    }
    let mut end = MAX_TEXT;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes truncated)", &text[..end], text.len() - end)
}

/// Anything the event buffers can hold: it must carry its own sequence number
/// so a reader can resume from a cursor it saw earlier.
pub trait Sequenced {
    fn seq(&self) -> u64;
}

impl Sequenced for ConsoleEntry {
    fn seq(&self) -> u64 {
        self.seq
    }
}

impl Sequenced for NetworkEntry {
    fn seq(&self) -> u64 {
        self.seq
    }
}

/// A bounded event log with a monotonic cursor.
///
/// Shared by the console and network buffers because both have the same shape:
/// events arrive on their own schedule, and a reader wants "everything after
/// the point I last read". Keeping the counter outside the buffer means a
/// caller's cursor stays meaningful even after old entries are dropped.
#[derive(Debug)]
pub struct EventBuffer<T> {
    entries: VecDeque<T>,
    next_seq: u64,
}

impl<T> Default for EventBuffer<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 0,
        }
    }
}

impl<T: Sequenced> EventBuffer<T> {
    /// Append an entry built from the next sequence number.
    ///
    /// The closure receives the sequence number so the entry can carry it
    /// without the buffer having to know the entry's shape.
    pub fn push(&mut self, make: impl FnOnce(u64) -> T) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(make(seq));
        while self.entries.len() > BUFFER_CAP {
            self.entries.pop_front();
        }
        seq
    }

    /// The sequence number the next entry will get, so a caller can start
    /// streaming from "now" instead of replaying history it does not want.
    pub fn cursor(&self) -> u64 {
        self.next_seq
    }

    /// Number of entries currently held, after the cap has been applied.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Mutable access to the stored entries, for in-place correlation such as
    /// attaching a response to the request it belongs to.
    pub fn entries_mut(&mut self) -> &mut VecDeque<T> {
        &mut self.entries
    }

    /// Entries with `seq >= since`, oldest first.
    pub fn since(&self, since: u64) -> Vec<&T> {
        self.entries.iter().filter(|e| e.seq() >= since).collect()
    }

    /// Most recent entries, oldest first, capped at `limit`.
    pub fn tail(&self, limit: usize) -> Vec<&T> {
        let skip = self.entries.len().saturating_sub(limit);
        self.entries.iter().skip(skip).collect()
    }
}

/// Per-tab buffers.
#[derive(Debug, Default)]
pub struct TabLogs {
    pub console: Mutex<EventBuffer<ConsoleEntry>>,
    pub network: Mutex<EventBuffer<NetworkEntry>>,
}

fn logs() -> &'static Mutex<std::collections::HashMap<String, &'static TabLogs>> {
    static LOGS: OnceLock<Mutex<std::collections::HashMap<String, &'static TabLogs>>> =
        OnceLock::new();
    LOGS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Logs for one app, created on first use.
///
/// Leaked on purpose: a tab's buffers must outlive any single command, and the
/// set of apps is bounded by the workspace. Freeing them on tab close would
/// race with an in-flight event callback that still holds a reference.
pub fn logs_for(app_id: &str) -> &'static TabLogs {
    let mut map = logs().lock().unwrap();
    map.entry(app_id.to_string())
        .or_insert_with(|| Box::leak(Box::new(TabLogs::default())))
}

/// Drop a tab's buffers when the tab is closed.
pub fn forget_logs(app_id: &str) {
    logs().lock().unwrap().remove(app_id);
}

/// Build a `ConsoleEntry` from a CDP `Runtime.consoleAPICalled` payload.
pub fn console_entry_from_cdp(seq: u64, params: &serde_json::Value) -> ConsoleEntry {
    let level = params
        .get("type")
        .and_then(|v| v.as_str())
        .map(|t| match t {
            "warning" => "warning",
            "error" | "assert" => "error",
            "debug" | "verbose" => "debug",
            "info" => "info",
            other => other,
        })
        .unwrap_or("log")
        .to_string();
    // CDP sends arguments as RemoteObjects: `value` for primitives, `description`
    // for objects and errors. Preferring `value` keeps strings readable instead
    // of quoted.
    let text = params
        .get("args")
        .and_then(|a| a.as_array())
        .map(|args| {
            args.iter()
                .map(|arg| {
                    arg.get("value")
                        .map(|v| match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .or_else(|| {
                            arg.get("description")
                                .and_then(|d| d.as_str())
                                .map(str::to_string)
                        })
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    ConsoleEntry {
        seq,
        level,
        text: clip(&text),
        url: params
            .get("url")
            .and_then(|u| u.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}

/// Build a `NetworkEntry` from a CDP `Network.requestWillBeSent` payload.
pub fn network_entry_from_cdp(seq: u64, params: &serde_json::Value) -> NetworkEntry {
    let request = params.get("request");
    NetworkEntry {
        seq,
        method: request
            .and_then(|r| r.get("method"))
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string(),
        url: clip(
            request
                .and_then(|r| r.get("url"))
                .and_then(|u| u.as_str())
                .unwrap_or_default(),
        ),
        kind: params
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string(),
        status: 0,
        ok: None,
        failed: None,
    }
}

/// Record a response against the entry whose URL matches, newest first.
///
/// CDP correlates responses by `requestId`, which the buffers do not keep (an
/// agent never sees it). Matching on the URL is the honest fallback: it can
/// pick the wrong entry when a page fires the same URL twice concurrently, and
/// that ambiguity is preferable to storing an id the agent cannot use.
pub fn attach_response(entries: &mut VecDeque<NetworkEntry>, url: &str, status: u16, ok: bool) {
    for entry in entries.iter_mut().rev() {
        if entry.url == url && entry.status == 0 {
            entry.status = status;
            entry.ok = Some(ok);
            return;
        }
    }
}

/// Record a failure against the entry whose URL matches.
pub fn attach_failure(entries: &mut VecDeque<NetworkEntry>, url: &str, reason: &str) {
    for entry in entries.iter_mut().rev() {
        if entry.url == url && entry.status == 0 {
            entry.failed = Some(reason.to_string());
            entry.ok = Some(false);
            return;
        }
    }
}

/// Parse a CDP `Runtime.evaluate` result into plain JSON for the agent.
///
/// CDP wraps the value in a `RemoteObject`; the agent wants the value, not the
/// wrapper. Objects come back as `description` only (the protocol does not
/// serialize arbitrary structures unless `returnByValue` was set), which is
/// reported as a string rather than silently becoming `null`.
pub fn unwrap_evaluate_result(result: &serde_json::Value) -> serde_json::Value {
    if let Some(exception) = result.get("exceptionDetails") {
        let text = exception
            .get("exception")
            .and_then(|e| e.get("description"))
            .and_then(|d| d.as_str())
            .or_else(|| exception.get("text").and_then(|t| t.as_str()))
            .unwrap_or("evaluation threw");
        return serde_json::json!({ "error": clip(text) });
    }
    let Some(remote) = result.get("result") else {
        return serde_json::Value::Null;
    };
    if let Some(value) = remote.get("value") {
        return value.clone();
    }
    if let Some(desc) = remote.get("description") {
        return desc.clone();
    }
    // `undefined`, or a subtype with no description: report the CDP type so the
    // agent can tell "no value" from "null".
    remote
        .get("type")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Parse a CDP call payload, mapping an in-band protocol error to a message.
///
/// WebView2 hands the completion handler the CDP **result object** — for
/// `Runtime.evaluate` that is `{"result": <RemoteObject>, "exceptionDetails": …}`,
/// for `Page.captureScreenshot` it is `{"data": "<base64>"}`. There is no
/// `{"id":…, "result":…}` envelope to unwrap, so this returns the payload as
/// given; callers read the fields their own command defines.
pub fn cdp_result(raw: &str) -> Result<serde_json::Value, String> {
    let parsed: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("browser returned invalid JSON: {e}"))?;
    // Protocol errors arrive in-band, not as a failed HRESULT.
    if let Some(err) = parsed.get("error") {
        let message = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("protocol error");
        return Err(message.to_string());
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// Send one CDP command to a tab and return its `result` object.
///
/// The call hops to the main thread through `with_webview` (which posts and
/// returns) and blocks here on a channel the completion handler feeds. Blocking
/// the caller, never the main thread, is the whole point: the completion
/// handler runs on the main thread's message pump, so waiting for it from
/// inside the pump would deadlock.
#[cfg(windows)]
pub fn call(
    win: &tauri::WebviewWindow,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    use windows_core::HSTRING;

    let (tx, rx) = std::sync::mpsc::channel::<(Result<(), String>, String)>();
    let label = method.to_string();
    let method = method.to_string();
    let params = params.to_string();
    let sync_tx = tx.clone();

    let _ = win.with_webview(move |platform| {
        let core = match unsafe { platform.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(e) => {
                let _ = sync_tx.send((Err(format!("tab has no WebView2 core: {e}")), String::new()));
                return;
            }
        };
        let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |res, json| {
            let _ = tx.send((res.map_err(|e| e.to_string()), json));
            Ok(())
        }));
        let hr = unsafe {
            core.CallDevToolsProtocolMethod(
                &HSTRING::from(method.as_str()),
                &HSTRING::from(params.as_str()),
                &handler,
            )
        };
        if let Err(e) = hr {
            // The command never reached the browser, so the handler will not
            // fire. Report it now instead of waiting out the timeout.
            let _ = sync_tx.send((Err(format!("CDP call rejected: {e}")), String::new()));
        }
    });

    match rx.recv_timeout(CALL_TIMEOUT) {
        Ok((Err(e), _)) => Err(e),
        Ok((Ok(()), raw)) => cdp_result(&raw),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "{label} got no reply within {}s; the tab may be unresponsive",
            CALL_TIMEOUT.as_secs()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(format!("{label} was dropped: the tab went away"))
        }
    }
}

/// Non-Windows: there is no CDP binding for WebKit through Tauri.
#[cfg(not(windows))]
pub fn call(
    _win: &tauri::WebviewWindow,
    method: &str,
    _params: serde_json::Value,
) -> Result<serde_json::Value, String> {
    Err(format!(
        "{method} is unavailable: only the Windows WebView2 backend exposes CDP"
    ))
}

/// Evaluate a JS expression in the tab and return its value.
///
/// `awaitPromise` is on so an agent can `await fetch(...)` without wrapping the
/// expression in an async IIFE itself.
pub fn evaluate(win: &tauri::WebviewWindow, expression: &str) -> Result<serde_json::Value, String> {
    let result = call(
        win,
        "Runtime.evaluate",
        serde_json::json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true,
        }),
    )?;
    Ok(unwrap_evaluate_result(&result))
}

/// Tabs whose console/network receivers are already installed.
///
/// Without this, every call to [`subscribe`] would add another receiver and
/// every event would land in the buffer twice.
static SUBSCRIBED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

/// Claim the right to subscribe `app_id`, returning false if it is already done.
///
/// The claim is recorded before the receivers are installed: a duplicate
/// subscription doubles every log line, which is worse to debug than a
/// subscription that silently failed and can be retried by reopening the tab.
pub fn mark_subscribed(app_id: &str) -> bool {
    let mut guard = SUBSCRIBED.lock().unwrap();
    guard
        .get_or_insert_with(std::collections::HashSet::new)
        .insert(app_id.to_string())
}

/// Allow a later subscribe after the tab is reopened with a fresh webview.
pub fn mark_unsubscribed(app_id: &str) {
    if let Some(set) = SUBSCRIBED.lock().unwrap().as_mut() {
        set.remove(app_id);
    }
}

// ---------------------------------------------------------------------------
// Event receivers
// ---------------------------------------------------------------------------

// Live `ICoreWebView2DevToolsProtocolEventReceiver` objects, per tab.
//
// WebView2 only delivers an event while the receiver that registered for it is
// alive, so these must be kept. They are COM objects bound to the UI thread,
// which is why they live in a thread-local instead of a static: every access
// happens inside a `with_webview` closure, and those run on the main thread.
#[cfg(windows)]
thread_local! {
    static RECEIVERS: std::cell::RefCell<
        std::collections::HashMap<
            String,
            Vec<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DevToolsProtocolEventReceiver>,
        >,
    > = std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Install console + network receivers for a tab.
///
/// The domains are *not* enabled here. Receivers are cheap and idle until
/// `Runtime.enable` / `Network.enable` is sent, so registering them at tab
/// creation means an agent that attaches later still sees the very next event,
/// while a tab nobody inspects pays nothing.
#[cfg(windows)]
pub fn subscribe(win: &tauri::WebviewWindow, app_id: String) {
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2DevToolsProtocolEventReceivedEventArgs;
    use webview2_com::take_pwstr;
    use webview2_com::DevToolsProtocolEventReceivedEventHandler;
    use windows_core::{HSTRING, PWSTR};

    let _ = win.with_webview(move |platform| {
        let core = match unsafe { platform.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(_) => return,
        };

        // Each event name needs its own receiver object.
        let mut wired = Vec::new();
        for event in [
            "Runtime.consoleAPICalled",
            "Network.requestWillBeSent",
            "Network.responseReceived",
            "Network.loadingFailed",
            "Runtime.exceptionThrown",
        ] {
            let receiver = match unsafe {
                core.GetDevToolsProtocolEventReceiver(&HSTRING::from(event))
            } {
                Ok(r) => r,
                Err(_) => continue,
            };

            let id = app_id.clone();
            let name = event.to_string();
            let handler =
                DevToolsProtocolEventReceivedEventHandler::create(Box::new(move |_core, args| {
                    let Some(args) = args else { return Ok(()) };
                    let args: ICoreWebView2DevToolsProtocolEventReceivedEventArgs = args;
                    let mut json = PWSTR::null();
                    if unsafe { args.ParameterObjectAsJson(&mut json) }.is_err() {
                        return Ok(());
                    }
                    let raw = take_pwstr(json);
                    if let Ok(params) = serde_json::from_str::<serde_json::Value>(&raw) {
                        record(&id, &name, &params);
                    }
                    Ok(())
                }));

            let mut token = 0i64;
            if unsafe { receiver.add_DevToolsProtocolEventReceived(&handler, &mut token) }.is_ok() {
                wired.push(receiver);
            }
        }

        RECEIVERS.with(|map| {
            // Replacing drops the previous set, which unregisters those
            // handlers: two live sets would double every entry.
            map.borrow_mut().insert(app_id.clone(), wired);
        });
    });
}

#[cfg(not(windows))]
pub fn subscribe(_win: &tauri::WebviewWindow, _app_id: String) {}

/// Drop a tab's receivers. Must run on the main thread, before the window dies.
#[cfg(windows)]
pub fn detach(app_id: &str) {
    RECEIVERS.with(|map| {
        map.borrow_mut().remove(app_id);
    });
}

#[cfg(not(windows))]
pub fn detach(_app_id: &str) {}

/// Whether console/network capture is switched on.
///
/// Off by default: enabling the CDP domains makes the browser emit an event for
/// every console line and every request on every tab, which is real overhead for
/// a feature most sessions never use. The MCP bridge turns it on while it is
/// running.
static CAPTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Switch capture on or off.
pub fn set_capture(on: bool) {
    CAPTURE.store(on, std::sync::atomic::Ordering::SeqCst);
}

/// Is capture currently on?
pub fn capture_enabled() -> bool {
    CAPTURE.load(std::sync::atomic::Ordering::SeqCst)
}

/// Ask the browser to start emitting console and network events for one tab.
///
/// Separate from [`subscribe`] because the receivers are always installed but the
/// domains are only enabled when capture is on: enabling a domain twice is
/// harmless, enabling it when nobody reads the buffers is not.
#[cfg(windows)]
pub fn enable_domains(win: &tauri::WebviewWindow) {
    use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
    use windows_core::HSTRING;

    let _ = win.with_webview(move |platform| {
        let core = match unsafe { platform.controller().CoreWebView2() } {
            Ok(core) => core,
            Err(_) => return,
        };
        for domain in ["Runtime.enable", "Network.enable"] {
            let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
            unsafe {
                let _ = core.CallDevToolsProtocolMethod(
                    &HSTRING::from(domain),
                    &HSTRING::from("{}"),
                    &handler,
                );
            }
        }
    });
}

#[cfg(not(windows))]
pub fn enable_domains(_win: &tauri::WebviewWindow) {}

/// Route one CDP event into the right buffer.
pub fn record(app_id: &str, event: &str, params: &serde_json::Value) {
    let logs = logs_for(app_id);
    match event {
        "Runtime.consoleAPICalled" => {
            let mut buffer = logs.console.lock().unwrap();
            buffer.push(|seq| console_entry_from_cdp(seq, params));
        }
        "Runtime.exceptionThrown" => {
            // An uncaught error is the single most useful console line, and CDP
            // reports it on its own event rather than through consoleAPICalled.
            let mut buffer = logs.console.lock().unwrap();
            buffer.push(|seq| ConsoleEntry {
                seq,
                level: "error".to_string(),
                text: clip(&exception_text(params)),
                url: params
                    .get("exceptionDetails")
                    .and_then(|d| d.get("url"))
                    .and_then(|u| u.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
        "Network.requestWillBeSent" => {
            let mut buffer = logs.network.lock().unwrap();
            buffer.push(|seq| network_entry_from_cdp(seq, params));
        }
        "Network.responseReceived" => {
            let url = params
                .get("response")
                .and_then(|r| r.get("url"))
                .and_then(|u| u.as_str());
            let status = params
                .get("response")
                .and_then(|r| r.get("status"))
                .and_then(|s| s.as_u64())
                .unwrap_or(0) as u16;
            if let Some(url) = url {
                let mut buffer = logs.network.lock().unwrap();
                attach_response(buffer.entries_mut(), url, status, (200..400).contains(&status));
            }
        }
        "Network.loadingFailed" => {
            let url = params.get("requestUrl").and_then(|u| u.as_str());
            let reason = params
                .get("errorText")
                .and_then(|t| t.as_str())
                .unwrap_or("failed");
            if let Some(url) = url {
                let mut buffer = logs.network.lock().unwrap();
                attach_failure(buffer.entries_mut(), url, reason);
            }
        }
        _ => {}
    }
}

/// Flatten a `Runtime.exceptionThrown` payload into one line.
fn exception_text(params: &serde_json::Value) -> String {
    let details = params.get("exceptionDetails");
    let description = details
        .and_then(|d| d.get("exception"))
        .and_then(|e| e.get("description"))
        .and_then(|d| d.as_str());
    let fallback = details
        .and_then(|d| d.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("uncaught exception");
    let line = details
        .and_then(|d| d.get("lineNumber"))
        .and_then(|l| l.as_u64());
    match (description, line) {
        (Some(desc), Some(line)) => format!("{desc} (line {})", line + 1),
        (Some(desc), None) => desc.to_string(),
        (None, _) => fallback.to_string(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_text_prefers_value_over_description() {
        let params = serde_json::json!({
            "type": "log",
            "url": "https://x.test/",
            "args": [
                { "type": "string", "value": "hello" },
                { "type": "object", "description": "Object" }
            ]
        });
        let entry = console_entry_from_cdp(7, &params);
        assert_eq!(entry.level, "log");
        assert_eq!(entry.text, "hello Object");
        assert_eq!(entry.url, "https://x.test/");
        assert_eq!(entry.seq, 7);
    }

    #[test]
    fn console_levels_are_mapped_to_four_names() {
        for (cdp, want) in [
            ("warning", "warning"),
            ("error", "error"),
            ("assert", "error"),
            ("verbose", "debug"),
            ("info", "info"),
            ("dir", "dir"),
        ] {
            let params = serde_json::json!({ "type": cdp, "args": [] });
            assert_eq!(console_entry_from_cdp(0, &params).level, want, "cdp {cdp}");
        }
    }

    #[test]
    fn long_console_text_is_clipped_visibly() {
        let long = "x".repeat(MAX_TEXT + 50);
        let params = serde_json::json!({ "type": "log", "args": [{ "value": long }] });
        let entry = console_entry_from_cdp(0, &params);
        assert!(entry.text.len() < MAX_TEXT + 100);
        assert!(entry.text.contains("truncated"));
    }

    #[test]
    fn clip_never_splits_a_utf8_character() {
        // A multi-byte character straddling the cut must not produce invalid
        // UTF-8, which would panic on slicing.
        let text = "é".repeat(MAX_TEXT);
        let clipped = clip(&text);
        assert!(clipped.contains("truncated"));
        assert!(clipped.chars().all(|c| c == 'é' || c == '…' || c.is_ascii()));
    }

    #[test]
    fn network_entry_carries_method_url_and_type() {
        let params = serde_json::json!({
            "type": "XHR",
            "request": { "method": "POST", "url": "https://x.test/api" }
        });
        let entry = network_entry_from_cdp(3, &params);
        assert_eq!(entry.method, "POST");
        assert_eq!(entry.url, "https://x.test/api");
        assert_eq!(entry.kind, "XHR");
        assert_eq!(entry.status, 0);
        assert_eq!(entry.seq, 3);
    }

    #[test]
    fn response_attaches_to_the_matching_pending_request() {
        let mut entries: VecDeque<NetworkEntry> = VecDeque::new();
        entries.push_back(network_entry_from_cdp(0, &serde_json::json!({
            "type": "XHR", "request": { "method": "GET", "url": "https://x.test/a" }
        })));
        attach_response(&mut entries, "https://x.test/a", 200, true);
        assert_eq!(entries[0].status, 200);
        assert_eq!(entries[0].ok, Some(true));
    }

    #[test]
    fn failure_marks_the_request_as_not_ok() {
        let mut entries: VecDeque<NetworkEntry> = VecDeque::new();
        entries.push_back(network_entry_from_cdp(0, &serde_json::json!({
            "type": "Fetch", "request": { "method": "GET", "url": "https://x.test/b" }
        })));
        attach_failure(&mut entries, "https://x.test/b", "net::ERR_ABORTED");
        assert_eq!(entries[0].ok, Some(false));
        assert_eq!(entries[0].failed.as_deref(), Some("net::ERR_ABORTED"));
    }

    #[test]
    fn evaluate_returns_the_value_not_the_remote_object() {
        let result = serde_json::json!({
            "result": { "type": "string", "value": "hi" }
        });
        assert_eq!(unwrap_evaluate_result(&result), serde_json::json!("hi"));
    }

    #[test]
    fn evaluate_surfaces_a_thrown_exception_as_an_error() {
        let result = serde_json::json!({
            "result": { "type": "object" },
            "exceptionDetails": {
                "text": "Uncaught",
                "exception": { "description": "ReferenceError: nope is not defined" }
            }
        });
        let out = unwrap_evaluate_result(&result);
        assert!(out.get("error").is_some());
        assert!(out["error"].as_str().unwrap().contains("ReferenceError"));
    }

    #[test]
    fn evaluate_reports_undefined_rather_than_null() {
        let result = serde_json::json!({ "result": { "type": "undefined" } });
        assert_eq!(unwrap_evaluate_result(&result), serde_json::json!("undefined"));
    }

    #[test]
    fn cdp_error_payload_becomes_an_error_message() {
        let raw = r#"{"error":{"code":-32601,"message":"'Nope.enable' wasn't found"}}"#;
        let err = cdp_result(raw).unwrap_err();
        assert!(err.contains("wasn't found"));
    }

    #[test]
    fn cdp_result_keeps_the_envelope_so_exception_details_survive() {
        // `evaluate` needs both `result` and `exceptionDetails` from the same
        // payload, so unwrapping here would lose the error branch.
        let raw = r#"{"result":{"result":{"type":"number","value":2}},"exceptionDetails":null}"#;
        let out = cdp_result(raw).unwrap();
        assert_eq!(out["result"]["result"]["value"], 2);
        assert!(out.get("exceptionDetails").is_some());
    }

    #[test]
    fn cdp_result_keeps_a_bare_result_object_intact() {
        // WebView2 delivers the CDP *result object*, not an envelope: a
        // screenshot arrives as `{"data": "..."}` with nothing around it.
        let raw = r#"{"data":"iVBORw0KGgo"}"#;
        assert_eq!(cdp_result(raw).unwrap()["data"], "iVBORw0KGgo");
    }

    #[test]
    fn buffer_drops_oldest_entries_but_keeps_counting() {
        let mut buffer: EventBuffer<ConsoleEntry> = EventBuffer::default();
        for i in 0..(BUFFER_CAP + 10) {
            let params = serde_json::json!({ "type": "log", "args": [{ "value": format!("{i}") }] });
            buffer.push(|seq| console_entry_from_cdp(seq, &params));
        }
        // The oldest were dropped, but the cursor still reflects every entry, so
        // a reader that asks for "since 0" gets a coherent suffix rather than
        // a buffer that appears to have restarted.
        assert_eq!(buffer.len(), BUFFER_CAP);
        assert_eq!(buffer.cursor(), (BUFFER_CAP + 10) as u64);
    }

    #[test]
    fn buffer_since_returns_only_newer_entries() {
        let mut buffer: EventBuffer<ConsoleEntry> = EventBuffer::default();
        for i in 0..5 {
            let params = serde_json::json!({ "type": "log", "args": [{ "value": format!("{i}") }] });
            buffer.push(|seq| console_entry_from_cdp(seq, &params));
        }
        let since = buffer.since(3);
        assert_eq!(since.len(), 2);
        assert_eq!(since[0].seq, 3);
        assert_eq!(since[1].seq, 4);
    }
}
