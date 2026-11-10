//! The AI chat panel: conversations, provider requests, and the tool loop.
//!
//! # Why the loop lives here
//!
//! The agent has to act on the browser, and only this process holds a
//! `WebView2` controller. So the tool calls run in-process against
//! [`crate::cdp`] and [`crate::tabs`] instead of going out over the MCP bridge
//! and back — one less hop, no token to manage, and the same code the bridge
//! uses for its own tools.
//!
//! # What is stored
//!
//! One JSON file per conversation under `<data root>/conversations`. A single
//! index file would have to be rewritten on every message, which turns one
//! failed write into a lost history; separate files fail one conversation at a
//! time. The list endpoint reads only the headers it needs.
//!
//! # Scope
//!
//! Tools respect the same `McpScope` setting as the bridge, so a user who
//! restricted the agent to the active tab gets that restriction in the panel
//! too. One setting, one meaning.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{Emitter, Manager};

use crate::config::AppState;
use crate::settings::{ApiKind, Provider};

/// Who produced a message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    User,
    Assistant,
    /// A tool result, fed back to the model on the next turn.
    Tool,
}

/// A file or image the user attached to a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    /// Base64 payload. Images are sent inline; other types are described to the
    /// model by name only, because there is no portable way to give a chat model
    /// arbitrary bytes.
    pub data: String,
}

/// One tool call the model asked for, and what came back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
    /// Filled in once the call has run.
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// One message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Unix milliseconds. Kept as a number so the UI can format it locally.
    pub at: u128,
}

/// A conversation as stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub created_at: u128,
    pub updated_at: u128,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
}

/// Header row for the conversation list. Separate from [`Conversation`] so
/// listing does not ship every message body to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct Summary {
    pub id: String,
    pub title: String,
    pub updated_at: u128,
    pub message_count: usize,
}

/// Where conversations live.
fn dir() -> PathBuf {
    crate::paths::root().join("conversations")
}

/// A file name that cannot escape the directory.
///
/// Conversation ids come from the UI and from an agent, so they are untrusted:
/// `../` in an id would otherwise let a caller read or overwrite any file this
/// process can reach.
fn safe_id(id: &str) -> Result<String, String> {
    let cleaned: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() || cleaned != id {
        return Err(format!("'{id}' is not a valid conversation id"));
    }
    Ok(cleaned)
}

fn path_for(id: &str) -> Result<PathBuf, String> {
    Ok(dir().join(format!("{}.json", safe_id(id)?)))
}

/// A new conversation id: time-ordered so the list sorts naturally.
fn new_id() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("conv-{stamp:x}")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Read one conversation.
pub fn load(id: &str) -> Result<Conversation, String> {
    let path = path_for(id)?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("could not read conversation '{id}': {e}"))?;
    serde_json::from_str(&raw).map_err(|e| format!("conversation '{id}' is damaged: {e}"))
}

/// Write one conversation, atomically.
fn save(conversation: &Conversation) -> Result<(), String> {
    let path = path_for(&conversation.id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(conversation).map_err(|e| e.to_string())?;
    // Temp file + rename: a crash mid-write must not leave a truncated
    // conversation behind, because that loses the whole history rather than the
    // last message.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// Every conversation, newest first.
pub fn list(_handle: &tauri::AppHandle) -> Result<serde_json::Value, String> {
    let dir = dir();
    let mut out: Vec<Summary> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // A damaged file must not hide the rest of the history.
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(conversation) = serde_json::from_str::<Conversation>(&raw) else {
                continue;
            };
            out.push(Summary {
                id: conversation.id,
                title: conversation.title,
                updated_at: conversation.updated_at,
                message_count: conversation.messages.len(),
            });
        }
    }
    out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(serde_json::json!({ "conversations": out }))
}

/// One conversation with its messages.
///
/// The stored model is reconciled against the provider's current list on the
/// way out: a model the user removed in Settings must not survive into the
/// panel's picker, where it would look selectable and then fail on send.
pub fn get(handle: &tauri::AppHandle, id: &str) -> Result<serde_json::Value, String> {
    let mut conversation = load(id)?;
    if align_model(&mut conversation, &read_settings(handle)?) {
        save(&conversation)?;
    }
    serde_json::to_value(conversation).map_err(|e| e.to_string())
}

/// Start an empty conversation.
pub fn create(
    handle: &tauri::AppHandle,
    provider_id: Option<String>,
    model: Option<String>,
) -> Result<Conversation, String> {
    let settings = read_settings(handle)?;
    let provider_id = provider_id.or_else(|| settings.chat.provider_id.clone());
    // The conversation's model wins; then the saved chat override; then the
    // provider's own default. Without the middle step a conversation started
    // while Settings held a model override would silently ignore it.
    let model = model
        .filter(|m| !m.trim().is_empty())
        .or_else(|| {
            let override_model = settings.chat.model.trim();
            (!override_model.is_empty()).then(|| override_model.to_string())
        })
        .unwrap_or_else(|| {
            provider_id
                .as_deref()
                .and_then(|id| settings.providers.iter().find(|p| p.id == id))
                .map(|p| {
                    let model = p.model.trim();
                    if model.is_empty() {
                        p.model_list().first().cloned().unwrap_or_default()
                    } else {
                        model.to_string()
                    }
                })
                .unwrap_or_default()
        });
    let mut conversation = Conversation {
        id: new_id(),
        title: "New conversation".to_string(),
        created_at: now_ms(),
        updated_at: now_ms(),
        provider_id,
        model,
        messages: Vec::new(),
    };
    // The chat-level model override is stored globally, so it can outlive the
    // model it names. Reconciling here keeps a new conversation from starting on
    // something the provider no longer serves.
    align_model(&mut conversation, &settings);
    save(&conversation)?;
    Ok(conversation)
}

/// Delete a conversation.
pub fn delete(_handle: &tauri::AppHandle, id: &str) -> Result<(), String> {
    let path = path_for(id)?;
    std::fs::remove_file(&path).map_err(|e| format!("could not delete '{id}': {e}"))
}

/// Rename a conversation.
pub fn rename(_handle: &tauri::AppHandle, id: &str, title: &str) -> Result<Conversation, String> {
    let mut conversation = load(id)?;
    let title = title.trim();
    if title.is_empty() {
        return Err("A conversation needs a title.".to_string());
    }
    conversation.title = title.chars().take(80).collect();
    conversation.updated_at = now_ms();
    save(&conversation)?;
    Ok(conversation)
}

/// Point a conversation at a different provider or model.
///
/// Switching the model is a property of the conversation, not of the panel: the
/// next request has to use it, and reopening the conversation later has to
/// remember it. Writing it here is what makes that true.
///
/// A provider switch also has to move the model, because a model id belongs to
/// the provider that serves it. Keeping the old one produced a request naming a
/// model the new endpoint had never heard of.
pub fn update(
    handle: &tauri::AppHandle,
    id: &str,
    provider_id: Option<String>,
    model: Option<String>,
) -> Result<Conversation, String> {
    let settings = read_settings(handle)?;
    let mut conversation = load(id)?;
    let provider_changed = match &provider_id {
        Some(next) => {
            let next = next.trim();
            let next = if next.is_empty() { None } else { Some(next.to_string()) };
            let changed = conversation.provider_id != next;
            conversation.provider_id = next;
            changed
        }
        None => false,
    };
    if let Some(model) = model {
        conversation.model = model.trim().to_string();
    } else if provider_changed {
        // A model id belongs to the provider that serves it, so switching
        // provider without naming a model has to drop the old one rather than
        // carry it to an endpoint that has never heard of it.
        conversation.model = String::new();
    }
    // An explicit model must be one the provider actually serves. Accepting an
    // unknown id here only defers the failure to the next send, where it arrives
    // as the provider's own 400 instead of a sentence naming the models that
    // would have worked.
    if !conversation.model.trim().is_empty()
        && let Some(provider) = provider_for_id(&settings, &conversation)
    {
        let available = provider.model_list();
        let chosen = conversation.model.trim();
        if !available.is_empty() && !available.iter().any(|m| m == chosen) {
            return Err(format!(
                "'{chosen}' is not one of {}'s models. Pick one of: {}.",
                provider.name,
                available.join(", ")
            ));
        }
    }
    // An empty model (just switched provider) or a stale one (removed in
    // Settings) becomes the provider's default, so the conversation is never
    // left pointing at something that cannot answer.
    align_model(&mut conversation, &settings);
    conversation.updated_at = now_ms();
    save(&conversation)?;
    Ok(conversation)
}

/// The provider a conversation resolves to, without failing when it is missing.
fn provider_for_id<'a>(
    settings: &'a crate::settings::Settings,
    conversation: &Conversation,
) -> Option<&'a Provider> {
    let id = conversation
        .provider_id
        .as_deref()
        .or(settings.chat.provider_id.as_deref())?;
    settings.providers.iter().find(|p| p.id == id)
}

/// Keep a conversation's model pointing at something its provider still serves.
///
/// A model leaves the list when the user removes it in Settings, or when a
/// router stops serving it. The conversation keeps the id it was created with,
/// so the panel goes on offering a model that cannot answer, and the next send
/// fails with the provider's own `400` — an error naming a model the user has no
/// way to fix from the chat panel. Reconciling makes the stored state agree with
/// the provider again, so the picker shows a model that exists and that failure
/// has nowhere to come from.
///
/// Returns whether the conversation changed, so the caller knows to persist it.
fn align_model(conversation: &mut Conversation, settings: &crate::settings::Settings) -> bool {
    let Some(provider) = provider_for_id(settings, conversation) else {
        // The provider itself is gone; that is a different problem and
        // `provider_for` already reports it by name.
        return false;
    };
    let available = provider.model_list();
    // A provider with no models is a configuration gap, not a stale reference:
    // leave it alone so `provider_for` can say what is actually missing.
    if available.is_empty() {
        return false;
    }
    let chosen = conversation.model.trim();
    if available.iter().any(|m| m == chosen) {
        return false;
    }
    // The provider's own default first, because that is what a new conversation
    // would start on; the first listed model otherwise.
    let fallback = {
        let default = provider.model.trim();
        if !default.is_empty() && available.iter().any(|m| m == default) {
            default.to_string()
        } else {
            available[0].clone()
        }
    };
    if chosen == fallback {
        return false;
    }
    conversation.model = fallback;
    true
}

fn read_settings(handle: &tauri::AppHandle) -> Result<crate::settings::Settings, String> {
    let state: tauri::State<AppState> = handle.state();
    let cfg = state.config.lock().map_err(|e| e.to_string())?;
    Ok(cfg.settings.clone())
}

/// The provider a conversation should use, or a message naming what is missing.
fn provider_for(
    handle: &tauri::AppHandle,
    conversation: &Conversation,
) -> Result<Provider, String> {
    let settings = read_settings(handle)?;
    let id = conversation
        .provider_id
        .as_deref()
        .or(settings.chat.provider_id.as_deref())
        .ok_or_else(|| {
            "No AI provider is configured. Add one in Settings -> AI Provider.".to_string()
        })?;
    let mut provider = settings
        .providers
        .iter()
        .find(|p| p.id == id)
        .cloned()
        .ok_or_else(|| format!("the selected provider '{id}' no longer exists"))?;
    // The conversation's model wins, but only while the provider still serves
    // it. A model removed in Settings leaves the conversation holding a dead id,
    // and sending it anyway produces the provider's own `400 model_not_found` —
    // an error the user cannot act on from the chat panel. Falling back to the
    // provider's default keeps the turn working; `align_model` writes the
    // corrected id back so the picker agrees.
    let chosen = conversation.model.trim();
    if !chosen.is_empty() {
        let available = provider.model_list();
        if available.is_empty() || available.iter().any(|m| m == chosen) {
            provider.model = chosen.to_string();
        }
    }
    if provider.base_url.trim().is_empty() {
        return Err(format!("Provider '{}' has no base URL.", provider.name));
    }
    // A provider may carry its models only in the list (a config edited by hand,
    // or one saved before the single field was kept in sync). Pick its first so
    // the request has a model rather than failing on an empty string.
    if provider.model.trim().is_empty()
        && let Some(first) = provider.model_list().first().cloned()
    {
        provider.model = first;
    }
    if provider.model.trim().is_empty() {
        return Err(format!("Provider '{}' has no model name.", provider.name));
    }
    Ok(provider)
}

/// Tool definitions offered to the model, in the shape the protocol expects.
///
/// `allow_edits` and `scope` are reflected in what is offered rather than only
/// being enforced at call time: a model that is told about a tool it may not use
/// will keep trying it and waste the turn.
fn tools(allow_edits: bool, active_only: bool) -> serde_json::Value {
    let app_hint = if active_only {
        "The only tab you may act on is the active one; omit appId to target it."
    } else {
        "Use list_tabs first to learn the app ids."
    };

    let mut list = vec![
        serde_json::json!({
            "name": "list_tabs",
            "description": "List the open tabs: each app's id, its active tabId, the URL, and every \
                            sub-tab it has open. Every tool that takes a tabId accepts one of those; \
                            leaving tabId out means the app's active tab.",
            "parameters": { "type": "object", "properties": {} }
        }),
        serde_json::json!({
            "name": "read_page",
            "description": format!("Read the visible text of a tab. {app_hint}"),
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string", "description": "App id of the tab." },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one." },
                    "maxChars": { "type": "number", "description": "Truncate to this many characters (default 6000)." }
                }
            }
        }),
        serde_json::json!({
            "name": "eval",
            "description": "Run JavaScript in a tab and return its value. Use it to read or change the page.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one." },
                    "expression": { "type": "string", "description": "JavaScript expression to evaluate." }
                },
                "required": ["expression"]
            }
        }),
        serde_json::json!({
            "name": "navigate",
            "description": "Point a tab at a URL.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one." },
                    "url": { "type": "string" }
                },
                "required": ["url"]
            }
        }),
        serde_json::json!({
            "name": "screenshot",
            "description": "Capture a tab as a PNG. Returns the image so you can look at the page.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one." }
                }
            }
        }),
        serde_json::json!({
            "name": "console_logs",
            "description": "Recent console output of a tab. Each sub-tab keeps its own console.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one." },
                    "limit": { "type": "number" }
                }
            }
        }),
        serde_json::json!({
            "name": "network_logs",
            "description": "Recent network requests of a tab. Each sub-tab keeps its own request log.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string", "description": "Optional sub-tab; defaults to the app's active one." },
                    "limit": { "type": "number" }
                }
            }
        }),
        serde_json::json!({
            "name": "list_apps",
            "description": "List every configured app, including ones that are not open.",
            "parameters": { "type": "object", "properties": {} }
        }),
    ];

    if allow_edits {
        list.push(serde_json::json!({
            "name": "open_tab",
            "description": "Open another tab in an app, sharing its cookies, storage and proxy.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "url": { "type": "string", "description": "Optional; defaults to the app's home page." }
                },
                "required": ["appId"]
            }
        }));
        list.push(serde_json::json!({
            "name": "close_tab",
            "description": "Close one sub-tab. The app's first tab is pinned and cannot be closed.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "tabId": { "type": "string" }
                },
                "required": ["appId", "tabId"]
            }
        }));
        list.push(serde_json::json!({
            "name": "set_link_target",
            "description": "Choose what a link that opens a new window does: \"ask\" shows the user a \
                            chooser, \"same\" replaces the page, \"new-tab\" opens a tab in the same app, \
                            \"popup\" opens a floating window, \"external\" hands it to the default browser.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "target": {
                        "type": "string",
                        "enum": ["ask", "same", "new-tab", "popup", "external"]
                    }
                },
                "required": ["appId", "target"]
            }
        }));
        list.push(serde_json::json!({
            "name": "add_app",
            "description": "Create a new app in the workspace.",
            "parameters": {
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "url": { "type": "string" },
                    "icon": { "type": "string", "description": "Optional emoji or data: URI." },
                    "proxyMode": { "type": "string", "enum": ["inherit", "none", "custom"] }
                },
                "required": ["name", "url"]
            }
        }));
        list.push(serde_json::json!({
            "name": "update_app",
            "description": "Change an existing app. Only the fields you pass are changed.",
            "parameters": {
                "type": "object",
                "properties": {
                    "appId": { "type": "string" },
                    "name": { "type": "string" },
                    "url": { "type": "string" },
                    "icon": { "type": "string" },
                    "enabled": { "type": "boolean" },
                    "muted": { "type": "boolean" },
                    "notifications": { "type": "boolean" },
                    "proxyMode": { "type": "string", "enum": ["inherit", "none", "custom"] }
                },
                "required": ["appId"]
            }
        }));
        list.push(serde_json::json!({
            "name": "delete_app",
            "description": "Remove an app from the workspace and close its tab.",
            "parameters": {
                "type": "object",
                "properties": { "appId": { "type": "string" } },
                "required": ["appId"]
            }
        }));
    }

    serde_json::Value::Array(list)
}

/// Which app a tool call targets: the one named, or the active tab.
fn target_app(_handle: &tauri::AppHandle, args: &serde_json::Value) -> Result<String, String> {
    if let Some(id) = args.get("appId").and_then(|v| v.as_str()) {
        return Ok(id.to_string());
    }
    crate::tabs::active_app()
        .ok_or_else(|| "no tab is currently shown; open a tab or pass appId".to_string())
}

/// The sub-tab a tool call targets, if it named one.
///
/// Optional on every tool: omitted means the app's active sub-tab, which is what
/// a model that has not learned about sub-tabs will send.
fn args_tab_id(args: &serde_json::Value) -> Option<String> {
    args.get("tabId")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Run one tool call and return its result.
///
/// Every failure is returned as a value, not propagated: a tool that fails is
/// information the model needs, and aborting the turn would hide it.
fn run_tool(
    handle: &tauri::AppHandle,
    name: &str,
    args: &serde_json::Value,
    allow_edits: bool,
) -> Result<serde_json::Value, String> {
    match name {
        "list_tabs" => crate::tabs::list_tabs(),
        "list_apps" => {
            let apps = crate::workspace::list(handle)?;
            Ok(serde_json::json!({ "apps": apps }))
        }
        "read_page" => {
            let app_id = target_app(handle, args)?;
            let max = args
                .get("maxChars")
                .and_then(|v| v.as_u64())
                .unwrap_or(6000) as usize;
            let win = crate::tabs::tab_window_of(&app_id, args_tab_id(args).as_deref(), handle)?;
            // `innerText` reflects what is actually rendered, which is what a
            // person reading the tab would see; `textContent` includes hidden
            // menus and script templates.
            let value = crate::cdp::evaluate(
                &win,
                "(() => { const t = document.body ? document.body.innerText : ''; \
                 return t.replace(/\\n{3,}/g, '\\n\\n').trim(); })()",
            )?;
            let text = value
                .get("result")
                .and_then(|r| r.get("value"))
                .or_else(|| value.get("value"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let truncated: String = text.chars().take(max).collect();
            Ok(serde_json::json!({
                "appId": app_id,
                "text": truncated,
                "truncated": text.chars().count() > max,
            }))
        }
        "eval" => {
            let app_id = target_app(handle, args)?;
            let expression = args
                .get("expression")
                .and_then(|v| v.as_str())
                .ok_or("eval needs an expression")?;
            let win = crate::tabs::tab_window_of(&app_id, args_tab_id(args).as_deref(), handle)?;
            crate::cdp::evaluate(&win, expression)
        }
        "navigate" => {
            let app_id = target_app(handle, args)?;
            let url = args
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or("navigate needs a url")?;
            let win = crate::tabs::tab_window_of(&app_id, args_tab_id(args).as_deref(), handle)?;
            crate::tabs::navigate_window(&win, url)?;
            Ok(serde_json::json!({ "navigated": url }))
        }
        "screenshot" => {
            let app_id = target_app(handle, args)?;
            let win = crate::tabs::tab_window_of(&app_id, args_tab_id(args).as_deref(), handle)?;
            let reply = crate::cdp::call(
                &win,
                "Page.captureScreenshot",
                serde_json::json!({ "format": "png", "fromSurface": true, "captureBeyondViewport": true }),
            )?;
            let data = reply
                .get("data")
                .or_else(|| reply.get("result").and_then(|r| r.get("data")))
                .and_then(|d| d.as_str())
                .ok_or("the browser returned no image data")?;
            Ok(serde_json::json!({ "png_base64": data }))
        }
        "console_logs" => {
            let app_id = target_app(handle, args)?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
            crate::tabs::tab_console(app_id, None, limit, args_tab_id(args))
        }
        "network_logs" => {
            let app_id = target_app(handle, args)?;
            let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
            crate::tabs::tab_network(app_id, None, limit, args_tab_id(args))
        }
        "add_app" => {
            if !allow_edits {
                return Err("editing apps is disabled in Settings".to_string());
            }
            let patch = patch_from(args);
            let app = crate::workspace::add(handle, patch)?;
            Ok(serde_json::json!({ "app": app }))
        }
        "update_app" => {
            if !allow_edits {
                return Err("editing apps is disabled in Settings".to_string());
            }
            let app_id = args
                .get("appId")
                .and_then(|v| v.as_str())
                .ok_or("update_app needs an appId")?;
            let app = crate::workspace::update(handle, app_id, patch_from(args))?;
            Ok(serde_json::json!({ "app": app }))
        }
        "delete_app" => {
            if !allow_edits {
                return Err("editing apps is disabled in Settings".to_string());
            }
            let app_id = args
                .get("appId")
                .and_then(|v| v.as_str())
                .ok_or("delete_app needs an appId")?;
            crate::workspace::delete(handle, app_id)?;
            Ok(serde_json::json!({ "deleted": app_id }))
        }
        "open_tab" => {
            if !allow_edits {
                return Err("editing apps is disabled in Settings".to_string());
            }
            let app_id = target_app(handle, args)?;
            let url = args.get("url").and_then(|v| v.as_str()).map(str::to_string);
            let app = crate::workspace::get(handle, &app_id)?;
            crate::tabs::schedule_sub_tab(handle.clone(), app, url)?;
            Ok(serde_json::json!({ "opened": app_id }))
        }
        "close_tab" => {
            if !allow_edits {
                return Err("editing apps is disabled in Settings".to_string());
            }
            let app_id = target_app(handle, args)?;
            let tab_id = args
                .get("tabId")
                .and_then(|v| v.as_str())
                .ok_or("close_tab needs a tabId")?
                .to_string();
            crate::tabs::close_sub_tab(app_id.clone(), tab_id.clone(), handle.clone())?;
            Ok(serde_json::json!({ "closed": tab_id, "appId": app_id }))
        }
        "set_link_target" => {
            if !allow_edits {
                return Err("editing apps is disabled in Settings".to_string());
            }
            let app_id = target_app(handle, args)?;
            let raw = args
                .get("target")
                .and_then(|v| v.as_str())
                .ok_or("set_link_target needs a target")?;
            let target = crate::settings::LinkTarget::parse(raw)
                .ok_or_else(|| format!("unknown link target '{raw}'"))?;
            crate::tabs::set_link_target(&app_id, target, handle)?;
            Ok(serde_json::json!({ "appId": app_id, "linkTarget": target.as_str() }))
        }
        other => Err(format!("unknown tool '{other}'")),
    }
}

/// Build an app patch from tool arguments.
fn patch_from(args: &serde_json::Value) -> crate::workspace::AppPatch {
    let text = |key: &str| {
        args
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let flag = |key: &str| args.get(key).and_then(|v| v.as_bool());
    crate::workspace::AppPatch {
        name: text("name"),
        url: text("url"),
        icon: text("icon"),
        engine: None,
        external_browser_path: None,
        profile_mode: None,
        fingerprint: None,
        external_args: None,
        proxy_mode: text("proxyMode").and_then(|m| match m.as_str() {
            "inherit" => Some(crate::settings::ProxyMode::Inherit),
            "none" => Some(crate::settings::ProxyMode::None),
            "custom" => Some(crate::settings::ProxyMode::Custom),
            _ => None,
        }),
        proxy: None,
        user_agent: None,
        link_target: text("linkTarget").and_then(|t| crate::settings::LinkTarget::parse(&t)),
        enabled: flag("enabled"),
        muted: flag("muted"),
        notifications: flag("notifications"),
    }
}

/// The system prompt: what the agent is, and the rules it must not break.
fn system_prompt(handle: &tauri::AppHandle, skills: &[crate::settings::Skill]) -> String {
    let mut prompt = String::from(
        "You are the assistant inside RustBox, a desktop app that keeps web apps in tabs.\n\
         You can read and drive the user's open tabs with the tools you have. \
         You are acting on the user's real, logged-in session: never post, send, pay, \
         or delete anything without being asked to in this conversation.\n\
         Prefer reading the page over guessing. If a tool fails, say what failed \
         instead of inventing a result.\n",
    );
    if let Some(active) = crate::tabs::active_app() {
        prompt.push_str(&format!(
            "\nThe tab currently on screen has app id '{active}'.\n"
        ));
    }
    if !skills.is_empty() {
        prompt.push_str("\nThe user enabled these skills. Follow them:\n");
        for skill in skills {
            prompt.push_str(&format!(
                "\n## {}\n{}\n",
                skill.name,
                skill.instructions.trim()
            ));
        }
    }
    let _ = handle;
    prompt
}

/// Build the request body for a provider.
fn request_body(
    provider: &Provider,
    system: &str,
    messages: &[Message],
    tools: &serde_json::Value,
    use_tools: bool,
) -> serde_json::Value {
    match provider.kind {
        ApiKind::ChatCompletions => {
            let mut turns: Vec<serde_json::Value> = vec![serde_json::json!({
                "role": "system",
                "content": system,
            })];
            for message in messages {
                turns.push(openai_message(message));
            }
            let mut body = serde_json::json!({
                "model": provider.model,
                "messages": turns,
            });
            if use_tools {
                body["tools"] = tools
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|tool| {
                                serde_json::json!({
                                    "type": "function",
                                    "function": {
                                        "name": tool["name"],
                                        "description": tool["description"],
                                        "parameters": tool["parameters"],
                                    }
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                    .into();
            }
            body
        }
        ApiKind::Anthropic => {
            let mut turns: Vec<serde_json::Value> = Vec::new();
            for message in messages {
                turns.push(anthropic_message(message));
            }
            let mut body = serde_json::json!({
                "model": provider.model,
                "max_tokens": 4096,
                "system": system,
                "messages": turns,
            });
            if use_tools {
                body["tools"] = tools
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|tool| {
                                serde_json::json!({
                                    "name": tool["name"],
                                    "description": tool["description"],
                                    "input_schema": tool["parameters"],
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                    .into();
            }
            body
        }
        ApiKind::Responses => {
            // The Responses API takes one `input` array of items and no separate
            // system role; the instructions field carries the system prompt.
            let mut items: Vec<serde_json::Value> = Vec::new();
            for message in messages {
                let role = match message.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "user",
                };
                // Images ride as input parts, the same way the other two
                // protocols carry them; plain text stays a plain string so a
                // text-only turn keeps the simplest shape.
                let images: Vec<serde_json::Value> = message
                    .attachments
                    .iter()
                    .filter(|a| a.mime.starts_with("image/"))
                    .map(|a| {
                        serde_json::json!({
                            "type": "input_image",
                            "image_url": format!("data:{};base64,{}", a.mime, a.data),
                        })
                    })
                    .collect();
                let content = if images.is_empty() {
                    serde_json::json!(message.content)
                } else {
                    let mut parts: Vec<serde_json::Value> = vec![serde_json::json!({
                        "type": "input_text",
                        "text": message.content,
                    })];
                    parts.extend(images);
                    serde_json::json!(parts)
                };
                items.push(serde_json::json!({
                    "type": "message",
                    "role": role,
                    "content": content,
                }));
            }
            let mut body = serde_json::json!({
                "model": provider.model,
                "instructions": system,
                "input": items,
            });
            if use_tools {
                body["tools"] = tools
                    .as_array()
                    .map(|list| {
                        list.iter()
                            .map(|tool| {
                                serde_json::json!({
                                    "type": "function",
                                    "name": tool["name"],
                                    "description": tool["description"],
                                    "parameters": tool["parameters"],
                                })
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
                    .into();
            }
            body
        }
    }
}

/// One message in OpenAI's shape, including image parts.
fn openai_message(message: &Message) -> serde_json::Value {
    match message.role {
        Role::Tool => serde_json::json!({
            "role": "tool",
            "tool_call_id": message
                .tool_calls
                .first()
                .map(|c| c.id.clone())
                .unwrap_or_default(),
            "content": message.content,
        }),
        Role::Assistant if !message.tool_calls.is_empty() => serde_json::json!({
            "role": "assistant",
            "content": message.content,
            "tool_calls": message
                .tool_calls
                .iter()
                .map(|call| serde_json::json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.name,
                        "arguments": call.arguments.to_string(),
                    }
                }))
                .collect::<Vec<_>>(),
        }),
        _ => {
            let images: Vec<serde_json::Value> = message
                .attachments
                .iter()
                .filter(|a| a.mime.starts_with("image/"))
                .map(|a| {
                    serde_json::json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{};base64,{}", a.mime, a.data) }
                    })
                })
                .collect();
            if images.is_empty() {
                serde_json::json!({ "role": "user", "content": message.content })
            } else {
                let mut parts: Vec<serde_json::Value> =
                    vec![serde_json::json!({ "type": "text", "text": message.content })];
                parts.extend(images);
                serde_json::json!({ "role": "user", "content": parts })
            }
        }
    }
}

/// One message in Anthropic's shape, including image blocks.
fn anthropic_message(message: &Message) -> serde_json::Value {
    match message.role {
        Role::Tool => serde_json::json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": message
                    .tool_calls
                    .first()
                    .map(|c| c.id.clone())
                    .unwrap_or_default(),
                "content": message.content,
            }],
        }),
        Role::Assistant if !message.tool_calls.is_empty() => {
            let mut blocks: Vec<serde_json::Value> = Vec::new();
            if !message.content.trim().is_empty() {
                blocks.push(serde_json::json!({ "type": "text", "text": message.content }));
            }
            for call in &message.tool_calls {
                blocks.push(serde_json::json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            serde_json::json!({ "role": "assistant", "content": blocks })
        }
        _ => {
            let mut blocks: Vec<serde_json::Value> = Vec::new();
            for attachment in &message.attachments {
                if attachment.mime.starts_with("image/") {
                    blocks.push(serde_json::json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": attachment.mime,
                            "data": attachment.data,
                        }
                    }));
                }
            }
            if !message.content.trim().is_empty() {
                blocks.push(serde_json::json!({ "type": "text", "text": message.content }));
            }
            serde_json::json!({ "role": "user", "content": blocks })
        }
    }
}

/// What the model said, normalised across the three protocols.
#[derive(Debug, Default)]
struct Reply {
    text: String,
    tool_calls: Vec<(String, String, serde_json::Value)>,
}

/// Read a reply out of whichever protocol answered.
fn parse_reply(kind: ApiKind, body: &serde_json::Value) -> Result<Reply, String> {
    match kind {
        ApiKind::ChatCompletions => {
            let choice = body
                .get("choices")
                .and_then(|c| c.get(0))
                .ok_or_else(|| {
                    format!(
                        "unexpected reply: {}",
                        crate::settings::shorten(&body.to_string(), 400)
                    )
                })?;
            let message = choice.get("message").unwrap_or(&serde_json::Value::Null);
            let text = message
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or_default()
                .to_string();
            let mut calls = Vec::new();
            if let Some(list) = message.get("tool_calls").and_then(|t| t.as_array()) {
                for call in list {
                    let id = call
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = call
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    // Arguments arrive as a JSON *string*, which is a common
                    // source of "the tool got nothing": parse it, and keep the
                    // raw text when it is not valid JSON so the model can be told.
                    let raw = call
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let args = serde_json::from_str(raw).unwrap_or_else(|_| {
                        serde_json::json!({ "__unparsed": raw })
                    });
                    calls.push((id, name, args));
                }
            }
            Ok(Reply {
                text,
                tool_calls: calls,
            })
        }
        ApiKind::Anthropic => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for block in body
                .get("content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default()
            {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(chunk) = block.get("text").and_then(|t| t.as_str()) {
                            text.push_str(chunk);
                        }
                    }
                    Some("tool_use") => {
                        calls.push((
                            block
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            block
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            block.get("input").cloned().unwrap_or(serde_json::json!({})),
                        ));
                    }
                    _ => {}
                }
            }
            Ok(Reply {
                text,
                tool_calls: calls,
            })
        }
        ApiKind::Responses => {
            let text = body
                .get("output_text")
                .and_then(|t| t.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| {
                    // Older replies only carry the structured output array.
                    body
                        .get("output")
                        .and_then(|o| o.as_array())
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(|item| {
                                    item.get("content")
                                        .and_then(|c| c.as_array())
                                        .map(|parts| {
                                            parts
                                                .iter()
                                                .filter_map(|p| {
                                                    p.get("text").and_then(|t| t.as_str())
                                                })
                                                .collect::<Vec<_>>()
                                                .join("")
                                        })
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                        .unwrap_or_default()
                });
            let mut calls = Vec::new();
            if let Some(items) = body.get("output").and_then(|o| o.as_array()) {
                for item in items {
                    if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                        let raw = item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("{}");
                        calls.push((
                            item.get("call_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            item
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string(),
                            serde_json::from_str(raw)
                                .unwrap_or_else(|_| serde_json::json!({ "__unparsed": raw })),
                        ));
                    }
                }
            }
            Ok(Reply {
                text,
                tool_calls: calls,
            })
        }
    }
}

/// One HTTP client for chat requests.
///
/// Reused so TLS setup and connection pooling are paid once. The timeout is
/// generous because a model answering a long question legitimately takes a
/// while, but not unbounded: a hung request must not pin a thread forever.
fn client() -> Result<&'static reqwest::Client, String> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(15))
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .map_err(|e| format!("could not start the HTTP client: {e}"))
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// Send one request to the provider and parse the reply.
async fn ask(provider: &Provider, body: &serde_json::Value) -> Result<Reply, String> {
    let client = client()?;
    let mut request = client
        .post(provider.endpoint())
        .header("content-type", "application/json")
        .json(body);
    for (name, value) in provider.request_headers() {
        request = request.header(name, value);
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("could not reach {}: {e}", provider.name))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("the reply could not be read: {e}"))?;
    if !status.is_success() {
        // Providers explain the real problem in the body; a bare status code
        // sends the user hunting.
        let explained = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("error")
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| text.clone());
        return Err(format!(
            "{} answered HTTP {}: {}",
            provider.name,
            status.as_u16(),
            crate::settings::shorten(&explained, 500)
        ));
    }
    let parsed = read_reply_body(&text)
        .map_err(|e| format!("{} returned invalid JSON: {e}", provider.name))?;
    parse_reply(provider.kind, &parsed)
}

/// Turn a reply body into one JSON value, tolerating a stream we did not ask for.
///
/// `ask` never sets `stream: true`, but a gateway is free to ignore that and
/// answer `text/event-stream` anyway — 9router does exactly that. The body is
/// then a perfectly good JSON object followed by `data: [DONE]`, and parsing it
/// as a single value fails with "trailing characters", which surfaced as the
/// chat panel refusing to answer a provider that works fine everywhere else.
///
/// So: take the leading JSON value if there is one, and otherwise reassemble the
/// reply from the `data:` frames. An SSE reply is reassembled into the same
/// non-streaming shape `parse_reply` already understands, which keeps one parser
/// for both transports instead of a second, subtly different one.
fn read_reply_body(text: &str) -> Result<serde_json::Value, String> {
    let trimmed = text.trim_start_matches('\u{feff}').trim();
    // A plain JSON reply is the common case and stays a single parse.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return Ok(value);
    }
    // `serde_json`'s streaming deserializer stops at the end of the first value
    // and ignores whatever follows, which is what makes `{...}data: [DONE]`
    // readable without hand-rolled brace counting.
    let mut stream = serde_json::Deserializer::from_str(trimmed).into_iter::<serde_json::Value>();
    if let Some(Ok(first)) = stream.next() {
        return Ok(first);
    }
    // No leading JSON at all: it really is a stream, so fold the frames.
    let folded = fold_event_stream(trimmed);
    if folded.is_null() {
        return Err(crate::settings::shorten(trimmed, 400));
    }
    Ok(folded)
}

/// Rebuild a non-streaming reply from `data:` frames.
///
/// Returns `Null` when the body carries no frames, so the caller can report the
/// body itself rather than an empty object. Text is concatenated and tool calls
/// are merged by index, which is how both the OpenAI and Anthropic streaming
/// formats accumulate.
fn fold_event_stream(text: &str) -> serde_json::Value {
    let mut text_out = String::new();
    let mut calls: Vec<(u64, String, String, String)> = Vec::new();
    let mut saw_frame = false;

    for line in text.lines() {
        let Some(payload) = line.trim().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        saw_frame = true;

        // A frame that already looks like a *whole* reply wins outright: some
        // gateways send one complete `chat.completion` object as the only event.
        // A delta frame also carries `choices`, so the test has to be "has a
        // message", not "has choices" — otherwise the first fragment is mistaken
        // for the finished reply and the answer is truncated to its first token.
        let complete = frame
            .get("choices")
            .and_then(|c| c.get(0))
            .map(|c| c.get("message").is_some())
            .unwrap_or(false);
        if complete {
            return frame;
        }
        // OpenAI-style delta.
        if let Some(delta) = frame
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
        {
            if let Some(chunk) = delta.get("content").and_then(|c| c.as_str()) {
                text_out.push_str(chunk);
            }
            if let Some(list) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for call in list {
                    merge_tool_call(&mut calls, call, "function");
                }
            }
        }
        // Anthropic-style event.
        match frame.get("type").and_then(|t| t.as_str()) {
            Some("content_block_delta") => {
                if let Some(chunk) = frame
                    .get("delta")
                    .and_then(|d| d.get("text"))
                    .and_then(|t| t.as_str())
                {
                    text_out.push_str(chunk);
                }
            }
            Some("content_block_start") => {
                if let Some(block) = frame.get("content_block")
                    && block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                {
                    merge_tool_call(&mut calls, block, "");
                }
            }
            _ => {}
        }
    }

    if !saw_frame {
        return serde_json::Value::Null;
    }

    let tool_calls: Vec<serde_json::Value> = calls
        .into_iter()
        .map(|(index, id, name, arguments)| {
            serde_json::json!({
                "id": id,
                "index": index,
                "function": { "name": name, "arguments": arguments },
            })
        })
        .collect();

    serde_json::json!({
        "choices": [{
            "index": 0,
            "finish_reason": "stop",
            "message": { "role": "assistant", "content": text_out, "tool_calls": tool_calls },
        }],
    })
}

/// Merge one tool-call fragment into `calls`, keyed by its index.
///
/// Arguments arrive in pieces and are concatenated; the name and id only appear
/// once, so a later empty value must not erase an earlier one.
fn merge_tool_call(
    calls: &mut Vec<(u64, String, String, String)>,
    source: &serde_json::Value,
    wrapper: &str,
) {
    let body = if wrapper.is_empty() {
        source
    } else {
        source.get(wrapper).unwrap_or(source)
    };
    let index = source
        .get("index")
        .and_then(|i| i.as_u64())
        .unwrap_or(calls.len() as u64);
    let id = source
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or_default();
    let args = body
        .get("arguments")
        .or_else(|| body.get("input"))
        .map(|v| match v.as_str() {
            Some(s) => s.to_string(),
            None => v.to_string(),
        })
        .unwrap_or_default();

    match calls.iter_mut().find(|(i, ..)| *i == index) {
        Some(slot) => {
            if !id.is_empty() {
                slot.1 = id.to_string();
            }
            if !name.is_empty() {
                slot.2 = name.to_string();
            }
            slot.3.push_str(&args);
        }
        None => calls.push((index, id.to_string(), name.to_string(), args)),
    }
}

/// Trim the conversation to the most recent turns the prompt may carry.
///
/// Without this, a long conversation grows without bound and every turn costs
/// more than the last. The first message is kept because it usually states what
/// the conversation is about.
fn windowed(messages: &[Message], limit: usize) -> Vec<Message> {
    if messages.len() <= limit {
        return messages.to_vec();
    }
    let tail = &messages[messages.len() - limit..];
    vec![messages[0].clone()]
        .into_iter()
        .chain(
            tail
                .iter()
                .filter(|m| m.id != messages[0].id)
                .cloned(),
        )
        .collect()
}

/// Push a new message and persist.
/// Copy one finished tool call's outcome onto the assistant turn that asked for it.
///
/// The panel renders the assistant message and skips the bare `tool` rows that
/// follow it, so the result has to live on the call inside that message. Only
/// the outcome is copied: the request payload is built from `id`, `name` and
/// `arguments`, so a filled-in result never reaches the provider.
fn record_tool_outcome(turn: &mut Message, call: &ToolCall) {
    if let Some(slot) = turn.tool_calls.iter_mut().find(|c| c.id == call.id) {
        slot.result = call.result.clone();
        slot.error = call.error.clone();
    }
}

fn push(conversation: &mut Conversation, message: Message) -> Result<(), String> {
    conversation.messages.push(message);
    conversation.updated_at = now_ms();
    // The first user message names the conversation, so the history list is
    // readable without opening each one.
    if conversation.title == "New conversation"
        && let Some(first) = conversation.messages.iter().find(|m| m.role == Role::User)
    {
        let title: String = first.content.chars().take(60).collect();
        if !title.trim().is_empty() {
            conversation.title = title.trim().to_string();
        }
    }
    save(conversation)
}

fn message_id(prefix: &str, counter: usize) -> String {
    format!("{prefix}-{}-{counter}", now_ms())
}

/// A fresh message id for this conversation.
///
/// Hoisted out of the `push` call so the id is computed before the mutable
/// borrow of the conversation begins.
fn next_message_id(conversation: &Conversation) -> String {
    message_id("msg", conversation.messages.len())
}

/// Everything the agent loop needs to run, resolved once per send.
struct Session {
    provider: Provider,
    use_tools: bool,
    allow_edits: bool,
    active_only: bool,
    skills: Vec<crate::settings::Skill>,
    history_limit: usize,
}

fn session_for(handle: &tauri::AppHandle, provider: Provider) -> Result<Session, String> {
    let settings = read_settings(handle)?;
    Ok(Session {
        provider,
        use_tools: settings.chat.tools_enabled,
        allow_edits: settings.mcp.allow_app_edits,
        active_only: settings.mcp.scope == crate::settings::McpScope::ActiveTab,
        skills: if settings.chat.use_skills {
            settings.skills.iter().filter(|s| s.enabled).cloned().collect()
        } else {
            Vec::new()
        },
        history_limit: settings.chat.history_limit.max(2),
    })
}

/// Normalise attachments that came from the UI.
///
/// The payload is untrusted in two ways that matter: a file name is displayed
/// back to the user, and a body is stored on disk and posted to a provider. So
/// names are bounded, the mime type is lowercased, and anything larger than the
/// cap is refused by name instead of silently eating the conversation file.
fn clean_attachments(raw: Vec<Attachment>) -> Vec<Attachment> {
    /// 8 MB of base64 is roughly 6 MB of file, which is the practical ceiling
    /// for an inline image part across the three protocols.
    const MAX_BASE64: usize = 8 * 1024 * 1024;
    raw.into_iter()
        .filter(|a| !a.data.is_empty() && a.data.len() <= MAX_BASE64)
        .map(|a| Attachment {
            name: crate::settings::shorten(a.name.trim(), 120),
            mime: {
                let mime = a.mime.trim().to_ascii_lowercase();
                if mime.contains('/') {
                    mime
                } else {
                    "application/octet-stream".to_string()
                }
            },
            data: a.data,
        })
        .collect()
}

/// Send a message and run the tool loop to completion.
///
/// `async` on purpose, and the two halves matter for different reasons:
///
/// * The HTTP request is awaited, so the thread running this is free while the
///   model thinks. Blocking it was the freeze: a synchronous command handler
///   runs on the WebView2 message thread, and a `block_on` there stops the
///   window pumping for the whole round trip — Windows paints it as "Not
///   responding".
/// * The tools are synchronous and are handed to [`tauri::async_runtime::spawn_blocking`].
///   They must never run on the message thread, because `crate::cdp` delivers
///   its reply *through* that thread's message pump: waiting for it there is a
///   self-deadlock, and every CDP call would burn its full 20 s timeout instead
///   of answering.
pub async fn send(
    handle: &tauri::AppHandle,
    conversation_id: &str,
    text: &str,
    attachments: Vec<Attachment>,
) -> Result<serde_json::Value, String> {
    let mut conversation = load(conversation_id)?;
    if text.trim().is_empty() && attachments.is_empty() {
        return Err("The message is empty.".to_string());
    }
    // A model removed from the provider since this conversation started must not
    // be sent: the provider would answer with its own 400, which names a model
    // the user cannot fix from here. Reconcile first, and persist the fix so the
    // picker and the request agree from now on.
    if align_model(&mut conversation, &read_settings(handle)?) {
        save(&conversation)?;
    }

    let user_message = Message {
        id: next_message_id(&conversation),
        role: Role::User,
        content: text.trim().to_string(),
        attachments: clean_attachments(attachments),
        tool_calls: Vec::new(),
        at: now_ms(),
    };
    push(&mut conversation, user_message)?;

    let provider = provider_for(handle, &conversation)?;
    let session = session_for(handle, provider)?;
    let tools = tools(session.allow_edits, session.active_only);
    let system = system_prompt(handle, &session.skills);

    // Bounded number of round trips. A model that keeps calling tools without
    // ever answering would otherwise loop forever on the user's bill.
    const MAX_ROUNDS: usize = 8;
    let mut final_text = String::new();

    for round in 0..MAX_ROUNDS {
        let history = windowed(&conversation.messages, session.history_limit);
        let body = request_body(
            &session.provider,
            &system,
            &history,
            &tools,
            session.use_tools,
        );
        let reply = ask(&session.provider, &body).await?;
        if reply.tool_calls.is_empty() {
            final_text = reply.text;
            if !final_text.trim().is_empty() {
                let id = next_message_id(&conversation);
                push(
                    &mut conversation,
                    Message {
                        id,
                        role: Role::Assistant,
                        content: final_text.clone(),
                        attachments: Vec::new(),
                        tool_calls: Vec::new(),
                        at: now_ms(),
                    },
                )?;
            }
            break;
        }

        // The assistant turn that asked for the tools, so the next request has
        // the call ids the results refer to.
        let calls: Vec<ToolCall> = reply
            .tool_calls
            .iter()
            .map(|(id, name, args)| ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: args.clone(),
                result: None,
                error: None,
            })
            .collect();
        let id = next_message_id(&conversation);
        let owner_id = id.clone();
        push(
            &mut conversation,
            Message {
                id,
                role: Role::Assistant,
                content: reply.text.clone(),
                attachments: Vec::new(),
                tool_calls: calls.clone(),
                at: now_ms(),
            },
        )?;
        // The panel renders the assistant turn and skips the bare tool rows that
        // follow it, so the outcome has to land on the call inside this message.
        // Leaving it on the copied call alone showed every finished tool as
        // "running" for good: the copy is dropped once the loop moves on.
        let owner = conversation
            .messages
            .iter()
            .rposition(|m| m.id == owner_id)
            .ok_or_else(|| "the assistant turn vanished while it ran".to_string())?;

        for mut call in calls {
            // Announced before the tool runs, not after. A tool can take seconds
            // (a page load, a screenshot), and the panel would otherwise sit on a
            // generic "waiting" for the whole time with no sign of what it is
            // waiting for. The event carries the name so the panel can say it.
            let _ = handle.emit(
                "chat-tool",
                serde_json::json!({
                    "round": round,
                    "tool": call.name,
                    "phase": "start",
                }),
            );
            let outcome = run_tool_off_thread(
                handle,
                &call.name,
                &call.arguments,
                session.allow_edits,
            )
            .await;
            let (payload, error) = match outcome {
                Ok(value) => (value, None),
                Err(e) => (serde_json::json!({ "error": e.clone() }), Some(e)),
            };
            // A screenshot is a megabyte of base64. Handing that back to the
            // model as text is useless and expensive, so the image is saved for
            // the UI and the model is told what it is looking at instead.
            let (content, saved) = if call.name == "screenshot" {
                match payload.get("png_base64").and_then(|v| v.as_str()) {
                    Some(data) => match crate::paths::save_screenshot(data) {
                        Ok(path) => (
                            format!("Screenshot saved to {}.", path.display()),
                            Some(path),
                        ),
                        Err(e) => (
                            format!("Screenshot captured but could not be saved: {e}"),
                            None,
                        ),
                    },
                    None => (payload.to_string(), None),
                }
            } else {
                (crate::settings::shorten(&payload.to_string(), 6000), None)
            };
            let _ = saved;

            call.result = Some(payload);
            call.error = error.clone();
            // Mirror the outcome onto the assistant turn the panel renders.
            record_tool_outcome(&mut conversation.messages[owner], &call);
            let id = next_message_id(&conversation);
            // Read before the call is moved into the message below.
            let tool_name = call.name.clone();
            let tool_ok = call.error.is_none();
            push(
                &mut conversation,
                Message {
                    id,
                    role: Role::Tool,
                    content,
                    attachments: Vec::new(),
                    tool_calls: vec![call],
                    at: now_ms(),
                },
            )?;
            let _ = handle.emit(
                "chat-tool",
                serde_json::json!({
                    "round": round,
                    "tool": tool_name,
                    "phase": "done",
                    "ok": tool_ok,
                }),
            );
        }
        // The last round may end with tool calls and no answer; the loop then
        // exits and `final_text` stays empty, which the caller reports.
    }

    if final_text.trim().is_empty() {
        final_text =
            "The agent used its tools but did not produce an answer. Try asking again, or \
             check the tool results above."
                .to_string();
    }

    save(&conversation)?;
    Ok(serde_json::json!({
        "conversationId": conversation.id,
        "reply": final_text,
        "messages": conversation.messages,
    }))
}

/// Run one tool call on a blocking-pool thread and await its result.
///
/// The tools are synchronous, and every one that touches a tab goes through
/// [`crate::cdp`], which posts to the main thread and waits for a reply that
/// only the main thread can deliver. Calling them from the async task would run
/// them wherever the runtime happens to schedule it — including the message
/// thread — and the CDP wait would then never be answered. Handing them to the
/// blocking pool puts them on a plain thread with no message pump, which is the
/// only context where waiting for CDP is safe.
async fn run_tool_off_thread(
    handle: &tauri::AppHandle,
    name: &str,
    args: &serde_json::Value,
    allow_edits: bool,
) -> Result<serde_json::Value, String> {
    let handle = handle.clone();
    let name = name.to_string();
    let args = args.clone();
    tauri::async_runtime::spawn_blocking(move || run_tool(&handle, &name, &args, allow_edits))
        .await
        .map_err(|e| format!("tool task failed: {e}"))?
}

/// Start a new conversation (command).
#[tauri::command]
pub fn chat_new(
    handle: tauri::AppHandle,
    provider_id: Option<String>,
    model: Option<String>,
) -> Result<Conversation, String> {
    create(&handle, provider_id, model)
}

/// List conversations (command).
#[tauri::command]
pub fn chat_list(handle: tauri::AppHandle) -> Result<serde_json::Value, String> {
    list(&handle)
}

/// One conversation (command).
#[tauri::command]
pub fn chat_get(handle: tauri::AppHandle, id: String) -> Result<serde_json::Value, String> {
    get(&handle, &id)
}

/// Delete a conversation (command).
#[tauri::command]
pub fn chat_delete(handle: tauri::AppHandle, id: String) -> Result<(), String> {
    delete(&handle, &id)
}

/// Rename a conversation (command).
#[tauri::command]
pub fn chat_rename(
    handle: tauri::AppHandle,
    id: String,
    title: String,
) -> Result<Conversation, String> {
    rename(&handle, &id, &title)
}

/// Point a conversation at a different provider or model (command).
#[tauri::command]
pub fn chat_update(
    handle: tauri::AppHandle,
    id: String,
    provider_id: Option<String>,
    model: Option<String>,
) -> Result<Conversation, String> {
    update(&handle, &id, provider_id, model)
}

/// Send a message (command).
///
/// `async`, and that is the whole fix for the freeze. A synchronous
/// `#[tauri::command]` runs inline on the WebView2 message thread, so the HTTP
/// round trip and the tool loop both stopped the window pumping — Windows
/// painted it "Not responding" until the agent finished. Worse, the CDP tools
/// wait for a reply that is delivered *by* that same thread, so each one could
/// only time out.
///
/// As an async command the HTTP call is awaited (thread free while the model
/// thinks) and every tool runs on the blocking pool via
/// [`run_tool_off_thread`].
#[tauri::command]
pub async fn chat_send(
    handle: tauri::AppHandle,
    conversation_id: String,
    text: String,
    attachments: Option<Vec<Attachment>>,
) -> Result<serde_json::Value, String> {
    send(
        &handle,
        &conversation_id,
        &text,
        attachments.unwrap_or_default(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: Role, content: &str) -> Message {
        Message {
            id: format!("m-{content}"),
            role,
            content: content.to_string(),
            attachments: Vec::new(),
            tool_calls: Vec::new(),
            at: 0,
        }
    }

    #[test]
    fn a_conversation_id_cannot_escape_its_directory() {
        // The id comes from the UI and from an agent, so `../` must not be able
        // to reach outside the conversations folder.
        assert!(safe_id("conv-abc123").is_ok());
        assert!(safe_id("../../secrets").is_err());
        assert!(safe_id("conv/../x").is_err());
        assert!(safe_id("").is_err());
        assert!(safe_id("conv 1").is_err());
    }

    #[test]
    fn the_prompt_window_keeps_the_first_message_and_the_latest_turns() {
        let messages: Vec<Message> = (0..30)
            .map(|i| message(Role::User, &format!("m{i}")))
            .collect();
        let window = windowed(&messages, 5);
        // First message (context) + last 5 turns.
        assert_eq!(window.len(), 6);
        assert_eq!(window.first().unwrap().content, "m0");
        assert_eq!(window.last().unwrap().content, "m29");
        // No duplicate of the first message when it is also in the tail.
        let unique: std::collections::HashSet<&str> =
            window.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(unique.len(), window.len());
    }

    #[test]
    fn a_short_conversation_is_not_trimmed() {
        let messages = vec![message(Role::User, "only one")];
        assert_eq!(windowed(&messages, 24).len(), 1);
    }

    #[test]
    fn every_tab_tool_takes_an_optional_tab_id() {
        // A sub-tab is only addressable if the model is told it can name one;
        // a tool that reads or drives a tab without the parameter silently
        // forces the agent back onto whatever is on screen.
        let list = tools(true, false);
        let tools_by_name: std::collections::HashMap<&str, &serde_json::Value> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|t| (t["name"].as_str().unwrap(), t))
            .collect();
        for name in ["read_page", "eval", "navigate", "screenshot", "console_logs", "network_logs"] {
            let tool = tools_by_name
                .get(name)
                .unwrap_or_else(|| panic!("tool '{name}' is missing"));
            assert!(
                tool["parameters"]["properties"]["tabId"].is_object(),
                "tool '{name}' has no tabId parameter"
            );
            assert!(
                !tool["parameters"]["required"]
                    .as_array()
                    .is_some_and(|r| r.iter().any(|v| v == "tabId")),
                "tool '{name}' must not require tabId"
            );
        }
    }

    #[test]
    fn every_advertised_tool_has_a_description() {
        let list = tools(true, false);
        for tool in list.as_array().unwrap() {
            let desc = tool["description"].as_str().unwrap_or("");
            assert!(!desc.is_empty(), "tool '{}' has no description", tool["name"]);
            assert_eq!(
                tool["parameters"]["type"], "object",
                "tool '{}' needs an object schema",
                tool["name"]
            );
        }
    }

    #[test]
    fn tool_definitions_exclude_edits_unless_they_are_allowed() {
        let without = tools(false, false);
        let names: Vec<&str> = without
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"add_app"));
        assert!(!names.contains(&"delete_app"));
        assert!(names.contains(&"list_tabs"));

        let with = tools(true, false);
        let names: Vec<&str> = with
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"add_app"));
        assert!(names.contains(&"update_app"));
        assert!(names.contains(&"delete_app"));
    }

    #[test]
    fn a_finished_tool_call_is_recorded_on_the_assistant_turn() {
        // The panel draws the assistant turn and skips the bare tool rows, so a
        // result left only on the loop's copy made every finished tool look like
        // it was still running, for good.
        let mut turn = Message {
            id: "msg-1".to_string(),
            role: Role::Assistant,
            content: String::new(),
            attachments: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "list_tabs".to_string(),
                arguments: serde_json::json!({}),
                result: None,
                error: None,
            }],
            at: 0,
        };
        let done = ToolCall {
            result: Some(serde_json::json!({ "tabs": 3 })),
            error: None,
            ..turn.tool_calls[0].clone()
        };
        record_tool_outcome(&mut turn, &done);
        assert_eq!(turn.tool_calls[0].result.as_ref().unwrap()["tabs"], 3);
        assert!(turn.tool_calls[0].error.is_none());

        // A different call id must not overwrite this one.
        let other = ToolCall { id: "call_2".to_string(), ..done };
        record_tool_outcome(&mut turn, &other);
        assert_eq!(turn.tool_calls[0].result.as_ref().unwrap()["tabs"], 3);
    }

    #[test]
    fn a_failed_tool_call_records_its_error() {
        let mut turn = Message {
            id: "msg-1".to_string(),
            role: Role::Assistant,
            content: String::new(),
            attachments: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "call_1".to_string(),
                name: "read_page".to_string(),
                arguments: serde_json::json!({}),
                result: None,
                error: None,
            }],
            at: 0,
        };
        let failed = ToolCall {
            result: Some(serde_json::json!({ "error": "no reply within 20s" })),
            error: Some("no reply within 20s".to_string()),
            ..turn.tool_calls[0].clone()
        };
        record_tool_outcome(&mut turn, &failed);
        assert_eq!(turn.tool_calls[0].error.as_deref(), Some("no reply within 20s"));
    }

    #[test]
    fn openai_reply_parses_tool_calls_and_text() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Let me look.",
                    "tool_calls": [{
                        "id": "call_1",
                        "function": { "name": "list_tabs", "arguments": "{\"a\":1}" }
                    }]
                }
            }]
        });
        let reply = parse_reply(ApiKind::ChatCompletions, &body).unwrap();
        assert_eq!(reply.text, "Let me look.");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].1, "list_tabs");
        assert_eq!(reply.tool_calls[0].2["a"], 1);
    }

    #[test]
    fn a_reply_with_no_choices_is_an_error_not_an_empty_answer() {
        // Silently returning "" would look like the model had nothing to say.
        let body = serde_json::json!({ "error": { "message": "bad key" } });
        assert!(parse_reply(ApiKind::ChatCompletions, &body).is_err());
    }

    #[test]
    fn anthropic_reply_splits_text_and_tool_use() {
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "Checking." },
                { "type": "tool_use", "id": "tu_1", "name": "read_page", "input": { "appId": "a" } }
            ]
        });
        let reply = parse_reply(ApiKind::Anthropic, &body).unwrap();
        assert_eq!(reply.text, "Checking.");
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].2["appId"], "a");
    }

    #[test]
    fn responses_reply_reads_both_output_text_and_function_calls() {
        let body = serde_json::json!({
            "output_text": "Done.",
            "output": [
                { "type": "function_call", "call_id": "fc_1", "name": "list_tabs", "arguments": "{}" }
            ]
        });
        let reply = parse_reply(ApiKind::Responses, &body).unwrap();
        assert_eq!(reply.text, "Done.");
        assert_eq!(reply.tool_calls.len(), 1);
    }

    #[test]
    fn a_reply_followed_by_a_done_marker_still_parses() {
        // 9router answers a non-streaming request with `text/event-stream` and
        // appends `data: [DONE]`. Parsing the whole body as one value used to
        // fail with "trailing characters", which the panel reported as the
        // provider being broken even though the reply was complete and valid.
        let body = concat!(
            r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
            "\n\ndata: [DONE]\n\n"
        );
        let value = read_reply_body(body).expect("a reply followed by a done marker");
        let reply = parse_reply(ApiKind::ChatCompletions, &value).unwrap();
        assert_eq!(reply.text, "ok");
    }

    #[test]
    fn a_byte_order_mark_does_not_defeat_the_parse() {
        let body = format!(
            "\u{feff}{}",
            r#"{"choices":[{"message":{"content":"hi"}}]}"#
        );
        let value = read_reply_body(&body).expect("a body with a BOM");
        assert_eq!(parse_reply(ApiKind::ChatCompletions, &value).unwrap().text, "hi");
    }

    #[test]
    fn a_real_stream_is_folded_into_one_reply() {
        // No leading JSON object at all: the frames are the only content.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let value = read_reply_body(body).expect("a folded stream");
        let reply = parse_reply(ApiKind::ChatCompletions, &value).unwrap();
        assert_eq!(reply.text, "Hello");
    }

    #[test]
    fn a_streamed_tool_call_is_reassembled_from_its_fragments() {
        // Arguments arrive split across frames; dropping any piece produces a
        // tool call the model never made.
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
            "\"function\":{\"name\":\"read_page\",\"arguments\":\"{\\\"app\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,",
            "\"function\":{\"arguments\":\"Id\\\":\\\"a\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let value = read_reply_body(body).expect("a folded tool call");
        let reply = parse_reply(ApiKind::ChatCompletions, &value).unwrap();
        assert_eq!(reply.tool_calls.len(), 1);
        assert_eq!(reply.tool_calls[0].1, "read_page");
        assert_eq!(reply.tool_calls[0].2["appId"], "a");
    }

    #[test]
    fn a_body_that_is_neither_json_nor_frames_is_reported_verbatim() {
        // The user has to see what actually came back; an empty error sends them
        // looking at the key or the network instead of the gateway.
        let err = read_reply_body("<html>bad gateway</html>").unwrap_err();
        assert!(err.contains("bad gateway"), "{err}");
    }

    #[test]
    fn unparseable_tool_arguments_are_kept_so_the_model_can_be_told() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "c1",
                        "function": { "name": "eval", "arguments": "{not json" }
                    }]
                }
            }]
        });
        let reply = parse_reply(ApiKind::ChatCompletions, &body).unwrap();
        assert_eq!(
            reply.tool_calls[0].2["__unparsed"], "{not json",
            "the raw text must survive so the failure is visible"
        );
    }

    #[test]
    fn attachments_are_normalised_and_bounded() {
        let kept = clean_attachments(vec![
            Attachment {
                name: "  shot.png  ".to_string(),
                mime: "IMAGE/PNG".to_string(),
                data: "AAAA".to_string(),
            },
            Attachment {
                name: "no-mime".to_string(),
                mime: String::new(),
                data: "AAAA".to_string(),
            },
            // Empty payloads and oversized ones are dropped rather than stored.
            Attachment {
                name: "empty".to_string(),
                mime: "text/plain".to_string(),
                data: String::new(),
            },
            Attachment {
                name: "huge".to_string(),
                mime: "image/png".to_string(),
                data: "A".repeat(8 * 1024 * 1024 + 1),
            },
        ]);
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].name, "shot.png");
        assert_eq!(kept[0].mime, "image/png", "mime must be lowercased");
        assert_eq!(
            kept[1].mime, "application/octet-stream",
            "a mime without a slash cannot be sent inline"
        );
    }

    #[test]
    fn an_image_attachment_becomes_an_inline_part() {
        let msg = Message {
            id: "m1".to_string(),
            role: Role::User,
            content: "what is this".to_string(),
            attachments: vec![Attachment {
                name: "a.png".to_string(),
                mime: "image/png".to_string(),
                data: "QUJD".to_string(),
            }],
            tool_calls: Vec::new(),
            at: 0,
        };
        let openai = openai_message(&msg);
        assert_eq!(openai["content"][1]["type"], "image_url");
        assert_eq!(
            openai["content"][1]["image_url"]["url"],
            "data:image/png;base64,QUJD"
        );
        let anthropic = anthropic_message(&msg);
        assert_eq!(anthropic["content"][0]["type"], "image");
        assert_eq!(anthropic["content"][0]["source"]["media_type"], "image/png");
        assert_eq!(anthropic["content"][1]["text"], "what is this");
    }

    fn provider_with(id: &str, default_model: &str, models: &[&str]) -> Provider {
        Provider {
            id: id.to_string(),
            name: id.to_string(),
            base_url: "https://example.test/v1".to_string(),
            model: default_model.to_string(),
            models: models.iter().map(|m| m.to_string()).collect(),
            ..Default::default()
        }
    }

    fn conversation_on(provider_id: &str, model: &str) -> Conversation {
        Conversation {
            id: "conv-1".to_string(),
            title: "t".to_string(),
            created_at: 0,
            updated_at: 0,
            provider_id: Some(provider_id.to_string()),
            model: model.to_string(),
            messages: Vec::new(),
        }
    }

    fn settings_with(providers: Vec<Provider>) -> crate::settings::Settings {
        crate::settings::Settings {
            providers,
            ..Default::default()
        }
    }

    #[test]
    fn a_model_that_was_removed_falls_back_to_the_provider_default() {
        // The exact case from the panel: the conversation still names a model the
        // user deleted, so the next send would 404 on an id they cannot fix.
        let settings = settings_with(vec![provider_with(
            "p1",
            "kept-model",
            &["kept-model", "other-model"],
        )]);
        let mut conversation = conversation_on("p1", "deleted-model");
        assert!(align_model(&mut conversation, &settings));
        assert_eq!(conversation.model, "kept-model");
    }

    #[test]
    fn a_model_that_still_exists_is_left_alone() {
        let settings = settings_with(vec![provider_with(
            "p1",
            "first",
            &["first", "second"],
        )]);
        let mut conversation = conversation_on("p1", "second");
        // Already valid: reporting a change here would rewrite the file on every
        // read for no reason.
        assert!(!align_model(&mut conversation, &settings));
        assert_eq!(conversation.model, "second");
    }

    #[test]
    fn a_stale_default_falls_back_to_the_first_listed_model() {
        // The provider's own `model` was also removed, so the default is unusable
        // and the first remaining entry is the only honest answer.
        let settings = settings_with(vec![provider_with(
            "p1",
            "gone-default",
            &["alpha", "beta"],
        )]);
        let mut conversation = conversation_on("p1", "also-gone");
        assert!(align_model(&mut conversation, &settings));
        assert_eq!(conversation.model, "alpha");
    }

    #[test]
    fn a_provider_without_models_is_not_reconciled() {
        // No list is a configuration gap. `provider_for` names it; rewriting the
        // conversation here would just move the error.
        let mut bare = provider_with("p1", "", &[]);
        bare.model.clear();
        let settings = settings_with(vec![bare]);
        let mut conversation = conversation_on("p1", "anything");
        assert!(!align_model(&mut conversation, &settings));
        assert_eq!(conversation.model, "anything");
    }

    #[test]
    fn a_missing_provider_is_not_reconciled() {
        let settings = settings_with(Vec::new());
        let mut conversation = conversation_on("p1", "anything");
        assert!(!align_model(&mut conversation, &settings));
    }
}
