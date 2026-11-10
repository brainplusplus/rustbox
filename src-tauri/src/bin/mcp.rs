//! `rustbox-mcp` — a Model Context Protocol server over stdio.
//!
//! MCP clients (Claude Desktop, Cursor, Droid, ...) launch this binary and speak
//! JSON-RPC on stdin/stdout. It forwards every tool call to the running rustbox
//! app over the loopback bridge the app opens when the feature is enabled, then
//! relays the answer back.
//!
//! # Why a separate process
//!
//! Two reasons, both structural:
//!
//! 1. MCP over stdio owns stdin and stdout. The GUI binary is built with
//!    `windows_subsystem = "windows"`, so it has no console handles to speak
//!    through, and mixing protocol output with the app's own logging would
//!    corrupt the stream.
//! 2. The capability being exposed (drive a logged-in browser) must not be
//!    ambient. The app decides whether the bridge exists at all; this shim is
//!    inert without it.
//!
//! # Protocol notes
//!
//! - One JSON object per line, per MCP's stdio framing.
//! - Only `initialize`, `tools/list`, `tools/call`, `ping` and the
//!   `notifications/initialized` notification are implemented. Anything else
//!   gets a `-32601` error, which is what the spec requires.
//! - stdout carries protocol frames only. Diagnostics go to stderr, which MCP
//!   clients surface as logs.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::path::PathBuf;

/// Protocol revision this server implements.
///
/// Clients negotiate: they send their preferred version and the server answers
/// with one it supports. Echoing the client's version back is also valid, but
/// answering with a fixed known-good version keeps behaviour identical across
/// clients.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Endpoint descriptor written by the app; must match `mcp::endpoint_path`.
fn endpoint_path() -> PathBuf {
    let base = dirs_next::data_local_dir()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    base.join("rustbox").join("mcp-endpoint.json")
}

/// Port + token read from the descriptor.
struct Endpoint {
    port: u16,
    token: String,
}

fn read_endpoint() -> Result<Endpoint, String> {
    let path = endpoint_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "rustbox does not appear to be running with MCP enabled \
             (could not read {}: {e})",
            path.display()
        )
    })?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("endpoint file at {} is corrupt: {e}", path.display()))?;
    let port = parsed
        .get("port")
        .and_then(|p| p.as_u64())
        .ok_or("endpoint file has no port")? as u16;
    let token = parsed
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or("endpoint file has no token")?
        .to_string();
    Ok(Endpoint { port, token })
}

/// Send one request to the app and return its result.
///
/// A fresh connection per call keeps the shim stateless: if the app restarts
/// (new port, new token) the next call simply re-reads the descriptor instead of
/// holding a dead socket.
fn forward(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let endpoint = read_endpoint()?;
    let stream = match TcpStream::connect(("127.0.0.1", endpoint.port)) {
        Ok(s) => s,
        Err(e) => {
            // A hard kill (task manager, crash) skips the app's exit hook, so the
            // descriptor can outlive the listener. Remove it so the next call
            // reports the truth — that rustbox is not running — instead of
            // dialling the same dead port forever.
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                let _ = std::fs::remove_file(endpoint_path());
                return Err(format!(
                    "rustbox is not running (stale endpoint on port {}); \
                     start it with the MCP bridge enabled",
                    endpoint.port
                ));
            }
            return Err(format!(
                "could not reach rustbox on 127.0.0.1:{} ({e})",
                endpoint.port
            ));
        }
    };
    // A screenshot or a navigation can legitimately take many seconds; this
    // bound only exists so a hung app cannot hang the agent's turn forever.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(60)));

    let request = serde_json::json!({
        "id": 1,
        "token": endpoint.token,
        "method": method,
        "params": params,
    });
    let mut line = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    line.push(b'\n');

    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;
    writer.write_all(&line).map_err(|e| e.to_string())?;
    writer.flush().map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut reply = String::new();
    reader
        .read_line(&mut reply)
        .map_err(|e| format!("no reply from rustbox: {e}"))?;
    if reply.trim().is_empty() {
        return Err("rustbox closed the connection without replying".to_string());
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&reply).map_err(|e| format!("bad reply from rustbox: {e}"))?;
    if parsed.get("ok").and_then(|o| o.as_bool()) == Some(true) {
        Ok(parsed.get("result").cloned().unwrap_or(serde_json::Value::Null))
    } else {
        Err(parsed
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("unknown error from rustbox")
            .to_string())
    }
}

/// Tool definitions advertised through `tools/list`.
fn tools() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "list_tabs",
            "description": "List every open rustbox app with its sub-tabs. Each row has the appId, \
                            the active tabId, the URL, and a `subTabs` array of every tab that app \
                            has open (with its own tabId, url, title, pinned and popup flags). \
                            Call this first: it is where appId and tabId come from. Omitting `tabId` \
                            anywhere else means \"the app's active tab\".",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "open_sub_tab",
            "description": "Open another tab in an app, sharing that app's cookies, storage and proxy. \
                            Use it to keep the page you are on while looking at something else. \
                            Requires \"Allow app edits\" in Settings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string", "description": "App id from list_tabs or list_apps" },
                    "url": { "type": "string", "description": "Optional URL; defaults to the app's home page" }
                },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "close_sub_tab",
            "description": "Close one sub-tab. The app's first tab is pinned and cannot be closed: \
                            close the app itself instead. Requires \"Allow app edits\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Which sub-tab to close" }
                },
                "required": ["appId", "tabId"],
                "additionalProperties": false
            }
        },
        {
            "name": "tab_eval",
            "description": "Evaluate a JavaScript expression inside a tab and return its value. \
                            Promises are awaited, so `await fetch('/api')` works. This runs in the \
                            page's own origin with the user's session, so it can read the DOM, \
                            call authenticated APIs, and read storage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string", "description": "App id from list_tabs" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" },
                    "expression": { "type": "string", "description": "JavaScript expression" }
                },
                "required": ["appId", "expression"],
                "additionalProperties": false
            }
        },
        {
            "name": "tab_cdp",
            "description": "Send any Chrome DevTools Protocol command to a tab, e.g. \
                            `DOM.getDocument`, `Network.setBlockedURLs`, `Emulation.setDeviceMetricsOverride`. \
                            Use this for capabilities the dedicated tools do not cover.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" },
                    "method": { "type": "string", "description": "CDP method name" },
                    "params": { "type": "object", "description": "CDP parameters object" }
                },
                "required": ["appId", "method"],
                "additionalProperties": false
            }
        },
        {
            "name": "tab_console",
            "description": "Read console messages and uncaught exceptions from a tab. Pass `since` \
                            with a cursor from a previous call to get only new messages. Each \
                            sub-tab has its own console, so pass `tabId` to read a background tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" },
                    "since": { "type": "integer", "description": "Only entries with seq >= since" },
                    "limit": { "type": "integer", "description": "Max entries when `since` is omitted" }
                },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "tab_network",
            "description": "Read network requests from a tab with their HTTP status, type and \
                            failure reason. Pass `since` with a cursor to get only new requests. \
                            Each sub-tab has its own request log.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" },
                    "since": { "type": "integer" },
                    "limit": { "type": "integer" }
                },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "tab_navigate",
            "description": "Point a tab at a URL. A bare host like `example.com` is upgraded to https.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" },
                    "url": { "type": "string" }
                },
                "required": ["appId", "url"],
                "additionalProperties": false
            }
        },
        {
            "name": "tab_screenshot",
            "description": "Capture a PNG of a tab's viewport. Returns base64 image data. Works for \
                            hidden tabs too, so you can screenshot a tab that is not on screen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" }
                },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "bridge_status",
            "description": "Report whether the rustbox bridge is reachable and which methods it supports.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "list_apps",
            "description": "List every configured app in the workspace, open or not, with its id, \
                            name, URL, engine and proxy mode. Use it to find an app that is not \
                            currently open.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_app",
            "description": "Read one app's full configuration by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "appId": { "type": "string" } },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "activate_tab",
            "description": "Bring an app to the front so the user sees it, optionally selecting one of \
                            its sub-tabs. Refuses when the app has no open tab; it never opens one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one" }
                },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "set_link_target",
            "description": "Choose what happens when a link in an app opens a new window \
                            (target=\"_blank\"): \"ask\" shows the user a chooser, \"same\" replaces the \
                            current page, \"new-tab\" opens another tab in the same app, \"popup\" opens a \
                            floating window, \"external\" hands it to the default browser. \"ask\" is the \
                            default; the user can override this from the tab's right-click menu.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "target": {
                        "type": "string",
                        "enum": ["ask", "same", "new-tab", "popup", "external"]
                    }
                },
                "required": ["appId", "target"],
                "additionalProperties": false
            }
        },
        {
            "name": "add_app",
            "description": "Add an app to the workspace. Requires \"Allow app edits\" in Settings. \
                            `proxy` accepts \"scheme://user:pass@host:port\" or \"host:port\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "url": { "type": "string" },
                    "icon": { "type": "string", "description": "Emoji or data: URI" },
                    "proxyMode": { "type": "string", "enum": ["inherit", "none", "custom"] },
                    "proxy": { "type": "string" }
                },
                "required": ["name", "url"],
                "additionalProperties": false
            }
        },
        {
            "name": "update_app",
            "description": "Change an app. Only the fields you pass are changed; passing `proxy` \
                            replaces it, and `\"proxy\": null` clears it. Requires \"Allow app edits\".",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "name": { "type": "string" },
                    "url": { "type": "string" },
                    "icon": { "type": "string" },
                    "enabled": { "type": "boolean" },
                    "muted": { "type": "boolean" },
                    "notifications": { "type": "boolean" },
                    "proxyMode": { "type": "string", "enum": ["inherit", "none", "custom"] },
                    "proxy": { "type": ["string", "null"] }
                },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "delete_app",
            "description": "Remove an app and close its tab. Requires \"Allow app edits\".",
            "inputSchema": {
                "type": "object",
                "properties": { "appId": { "type": "string" } },
                "required": ["appId"],
                "additionalProperties": false
            }
        },
        {
            "name": "list_conversations",
            "description": "List the conversations in the rustbox chat panel, newest first, with \
                            their id, title and message count. Use it to follow what the user has \
                            already been discussing with the built-in assistant.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        },
        {
            "name": "get_conversation",
            "description": "Read one conversation in full: every message, tool call and result. \
                            This is how another agent can pick up the context of a chat.",
            "inputSchema": {
                "type": "object",
                "properties": { "conversationId": { "type": "string" } },
                "required": ["conversationId"],
                "additionalProperties": false
            }
        },
        {
            "name": "send_message",
            "description": "Post a message into a rustbox conversation and return the assistant's \
                            reply. The built-in assistant runs its tools, so this is a way to ask \
                            it to do something rather than doing it yourself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "conversationId": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["conversationId", "text"],
                "additionalProperties": false
            }
        }
    ])
}

/// Map an MCP tool name to a bridge method.
fn bridge_method(tool: &str) -> Option<&'static str> {
    match tool {
        "list_tabs" => Some("list_tabs"),
        "open_sub_tab" => Some("open_sub_tab"),
        "close_sub_tab" => Some("close_sub_tab"),
        "tab_eval" => Some("eval"),
        "tab_cdp" => Some("cdp"),
        "tab_console" => Some("console"),
        "tab_network" => Some("network"),
        "tab_navigate" => Some("navigate"),
        "tab_screenshot" => Some("screenshot"),
        "bridge_status" => Some("status"),
        "list_apps" => Some("list_apps"),
        "get_app" => Some("get_app"),
        "activate_tab" => Some("activate_tab"),
        "set_link_target" => Some("set_link_target"),
        "add_app" => Some("add_app"),
        "update_app" => Some("update_app"),
        "delete_app" => Some("delete_app"),
        "list_conversations" => Some("list_conversations"),
        "get_conversation" => Some("get_conversation"),
        "send_message" => Some("send_message"),
        _ => None,
    }
}

/// Run one tool call and format the MCP `content` array.
fn call_tool(name: &str, args: &serde_json::Value) -> Result<serde_json::Value, String> {
    let Some(method) = bridge_method(name) else {
        return Err(format!("unknown tool '{name}'"));
    };
    let result = forward(method, args.clone())?;

    // A screenshot comes back as a `data` field holding base64 PNG; MCP has a
    // first-class image content type, so send it as one instead of a JSON blob
    // the model would have to decode itself.
    if name == "tab_screenshot"
        && let Some(data) = result.get("data").and_then(|d| d.as_str())
    {
        return Ok(serde_json::json!({
            "content": [{
                "type": "image",
                "data": data,
                "mimeType": "image/png"
            }]
        }));
    }

    let text = serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string());
    Ok(serde_json::json!({
        "content": [{ "type": "text", "text": text }]
    }))
}

/// Handle one JSON-RPC message. `None` means "no response" (notifications).
fn handle(message: &serde_json::Value) -> Option<serde_json::Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");

    // Notifications carry no id and must never be answered.
    let id = id?;

    let reply = |result: serde_json::Value| {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
    };
    let fail = |code: i64, message: String| {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message }
        })
    };

    match method {
        "initialize" => Some(reply(serde_json::json!({
            // Answer with the revision this server implements rather than
            // echoing the client's. Echoing an unknown version would let a
            // client use features that are not actually here.
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "rustbox-mcp", "version": env!("CARGO_PKG_VERSION") }
        }))),
        "ping" => Some(reply(serde_json::json!({}))),
        "tools/list" => Some(reply(serde_json::json!({ "tools": tools() }))),
        "tools/call" => {
            let params = message.get("params").cloned().unwrap_or_default();
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            match call_tool(name, &args) {
                Ok(content) => Some(reply(content)),
                Err(e) => Some(reply(serde_json::json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true
                }))),
            }
        }
        "resources/list" => Some(reply(serde_json::json!({ "resources": [] }))),
        "prompts/list" => Some(reply(serde_json::json!({ "prompts": [] }))),
        other => Some(fail(-32601, format!("method '{other}' is not implemented"))),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let err = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                let _ = writeln!(out, "{err}");
                let _ = out.flush();
                continue;
            }
        };
        if let Some(response) = handle(&message) {
            if writeln!(out, "{response}").is_err() {
                break;
            }
            if out.flush().is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_answers_with_a_supported_version() {
        let msg = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        });
        let reply = handle(&msg).expect("initialize must reply");
        assert_eq!(reply["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(reply["result"]["serverInfo"]["name"], "rustbox-mcp");
    }

    #[test]
    fn notifications_get_no_reply() {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(&msg).is_none());
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 9, "method": "nope" });
        let reply = handle(&msg).unwrap();
        assert_eq!(reply["error"]["code"], -32601);
    }

    #[test]
    fn tools_list_covers_every_dispatchable_tool() {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let reply = handle(&msg).unwrap();
        let tools = reply["result"]["tools"].as_array().unwrap();
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert!(
                bridge_method(name).is_some(),
                "tool '{name}' is advertised but has no bridge method"
            );
            assert!(
                tool["inputSchema"]["type"] == "object",
                "tool '{name}' needs an object schema"
            );
        }
    }

    #[test]
    fn every_advertised_tool_has_a_description() {
        let tools = tools();
        for tool in tools.as_array().unwrap() {
            let desc = tool["description"].as_str().unwrap_or("");
            assert!(!desc.is_empty(), "tool '{}' has no description", tool["name"]);
        }
    }
}
