//! Chromium side of the CDP bridge: pending replies and the one observer.
//!
//! CEF delivers DevTools traffic differently from WebView2. There is no
//! per-call completion handler; instead the host sends JSON into the browser
//! and one observer receives everything that comes back for that browser —
//! results for calls made by this app, results for calls made by the runtime
//! itself, and unsolicited events.
//!
//! That shape forces two rules:
//!
//! * **Replies are matched by id.** [`pending`] is the table of calls in
//!   flight, keyed by the id this module allocated. A result whose id is not in
//!   the table is somebody else's and is dropped, not guessed at.
//! * **One observer per webview.** CEF has no way to unregister an observer
//!   short of destroying the browser, so installing one per call would leak one
//!   per call. The observer is installed once, at tab creation, and dispatches
//!   from then on.
//!
//! The observer runs on CEF's UI thread, which is the same thread the rest of
//! the runtime uses; it must return promptly and never wait for another
//! event-loop operation.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tauri_runtime_cef::DevToolsProtocol;

/// Calls waiting for a reply, keyed by the DevTools message id.
type Pending = HashMap<i32, std::sync::mpsc::Sender<Result<serde_json::Value, String>>>;

fn table() -> &'static Mutex<Pending> {
    static PENDING: OnceLock<Mutex<Pending>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The table of calls in flight.
///
/// Exposed to the module that sends the call so the registration and the
/// cleanup bracket the same map, rather than two copies of it.
pub(crate) fn pending() -> &'static Mutex<Pending> {
    table()
}

/// Webview labels that already have an observer.
fn installed() -> &'static Mutex<std::collections::HashSet<String>> {
    static INSTALLED: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    INSTALLED.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

/// Renderer-side logs CEF reports outside `consoleAPICalled`.
///
/// `Log.entryAdded` carries the messages Chromium itself emits (deprecations,
/// refused loads), which is the same class of line the WebView2 build receives
/// as a console entry. Mapping it keeps the console buffer comparable between
/// the two engines.
const EVENT_DOMAINS: &[&str] = &[
    "Runtime.consoleAPICalled",
    "Runtime.exceptionThrown",
    "Network.requestWillBeSent",
    "Network.responseReceived",
    "Network.loadingFailed",
    "Log.entryAdded",
];

/// Install the observer for `win`, once per label.
///
/// Re-registering is not an error: a tab may be reopened with the label it had
/// before, and the previous browser (and its observer) is gone by then.
pub(crate) fn install(win: &tauri::WebviewWindow, app_id: String) {
    use tauri_runtime_cef::WebviewCefExt;

    let fresh = installed().lock().unwrap().insert(app_id.clone());
    if !fresh {
        return;
    }

    let label = app_id.clone();
    let registered = win.on_dev_tools_protocol(move |protocol| {
        match protocol {
            DevToolsProtocol::MethodResult {
                message_id,
                success,
                result,
            } => {
                // Take the sender out before sending: the call itself removes a
                // stale entry on timeout, and a late reply must not be able to
                // wake a waiter that already gave up.
                let sender = pending().lock().unwrap().remove(&message_id);
                if let Some(sender) = sender {
                    let _ = sender.send(if success {
                        Ok(serde_json::from_slice(&result).unwrap_or(serde_json::Value::Null))
                    } else {
                        Err(format!(
                            "CDP call failed: {}",
                            String::from_utf8_lossy(&result)
                        ))
                    });
                }
            }
            DevToolsProtocol::Event { method, params } => {
                if !EVENT_DOMAINS.contains(&method.as_str()) {
                    return;
                }
                if let Ok(params) = serde_json::from_slice::<serde_json::Value>(&params) {
                    super::record(&label, &method, &params);
                }
            }
            // The raw message is already classified above; handling it too would
            // double every event.
            DevToolsProtocol::Message(_) => {}
        }
    });

    if registered.is_err() {
        // The observer could not be installed (the webview is not a CEF one, or
        // its browser is already gone). Let a later tab with this label try
        // again instead of remembering a registration that never happened.
        installed().lock().unwrap().remove(&app_id);
    }
}

/// Forget the observer bookkeeping for a tab that is closing.
///
/// Calls that tab was waiting on are left to their own timeout: they are keyed
/// by message id and removed by the caller, so clearing the table here would
/// cancel every other tab's in-flight call too.
pub(crate) fn forget(app_id: &str) {
    installed().lock().unwrap().remove(app_id);
}

/// Install the observer if this webview does not have one yet.
///
/// [`call`](super::call) needs the observer, and it must not depend on the tab
/// having gone through `subscribe` first: the MCP bridge can address a tab that
/// was opened before capture was switched on. The label is the tab's own, which
/// is also the key the console/network buffers use, so the two paths cannot
/// install two observers for one browser.
pub(crate) fn ensure_observer(win: &tauri::WebviewWindow) -> Result<(), String> {
    if installed().lock().unwrap().contains(win.label()) {
        return Ok(());
    }
    install(win, win.label().to_string());
    if installed().lock().unwrap().contains(win.label()) {
        Ok(())
    } else {
        Err(format!(
            "{}: the CDP observer could not be attached (is this a Chromium tab?)",
            win.label()
        ))
    }
}
