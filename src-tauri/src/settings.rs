//! Settings: global proxy, proxy pools, AI providers, MCP bridge options,
//! skills, theme.
//!
//! # Why proxy resolution lives here
//!
//! A proxy can be decided at three levels (per-app, global, pool) and two
//! places consume the answer (embedded tabs, external browsers). Keeping the
//! resolution in one function is what stops those two call sites from drifting,
//! and it makes the precedence unit-testable without a webview.

use serde::{Deserialize, Serialize};

/// How an app decides its proxy.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyMode {
    /// Follow the global setting.
    #[default]
    Inherit,
    /// Always connect directly, even when a global proxy exists.
    None,
    /// Use this app's own proxy, ignoring the global setting.
    Custom,
}

/// What the global proxy is.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum GlobalProxyMode {
    #[default]
    None,
    Custom,
    Pool,
}

/// One proxy endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Proxy {
    /// `http` or `socks5`. `https`/`socks` are accepted from older configs and
    /// normalised by [`Proxy::normalized_scheme`], because the engines only
    /// parse the two canonical forms.
    pub scheme: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Proxy {
    /// Scheme the engines can actually honour.
    ///
    /// WebView2 and wry only parse `http` and `socks5`. An `https` proxy is
    /// treated as plain HTTP: the hop to the proxy is not TLS-wrapped, which is
    /// what both engines do for `http` anyway, so the request still reaches the
    /// proxy instead of failing to parse and leaving the app with no proxy.
    pub fn normalized_scheme(&self) -> &'static str {
        match self
            .scheme
            .as_deref()
            .unwrap_or("http")
            .to_ascii_lowercase()
            .as_str()
        {
            "socks" | "socks5" => "socks5",
            _ => "http",
        }
    }

    /// A proxy with no host is not a proxy. Treating it as one produces "proxy
    /// to nowhere", which fails in a way that looks like the setting was
    /// ignored.
    pub fn is_usable(&self) -> bool {
        !self.host.trim().is_empty() && self.port > 0
    }

    /// `scheme://[user:pass@]host:port`, credentials included.
    ///
    /// For display and for the proxy tester only: the engines get a URI without
    /// inline credentials because neither can use them (see
    /// `external::resolve_proxy_uri`).
    pub fn to_uri(&self) -> String {
        let scheme = self.normalized_scheme();
        match (&self.username, &self.password) {
            (Some(u), Some(p)) if !u.is_empty() => {
                format!("{scheme}://{u}:{p}@{}:{}", self.host, self.port)
            }
            _ => format!("{scheme}://{}:{}", self.host, self.port),
        }
    }

    /// Parse `scheme://[user:pass@]host:port`.
    ///
    /// This is fed straight from a text field, so every failure names the
    /// actual problem instead of saying "invalid".
    pub fn parse(raw: &str) -> Result<Proxy, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("Enter a proxy address.".to_string());
        }
        let (scheme, rest) = match raw.split_once("://") {
            Some((s, rest)) => (s.to_ascii_lowercase(), rest),
            // No scheme: assume http, which is what pasting "1.2.3.4:8080"
            // means.
            None => ("http".to_string(), raw),
        };
        if !matches!(scheme.as_str(), "http" | "https" | "socks5" | "socks") {
            return Err(format!(
                "'{scheme}' is not a supported scheme. Use http, https or socks5."
            ));
        }
        // Credentials sit before the LAST '@', so a password containing '@'
        // survives (a host cannot contain one).
        let (auth, hostport) = match rest.rsplit_once('@') {
            Some((auth, hostport)) => (Some(auth), hostport),
            None => (None, rest),
        };
        let (username, password) = match auth {
            Some(a) => match a.split_once(':') {
                Some((u, p)) => (Some(u.to_string()), Some(p.to_string())),
                None => (Some(a.to_string()), None),
            },
            None => (None, None),
        };
        // Drop a pasted path/query: providers hand out URLs, not socket addresses.
        let hostport = hostport
            .split(['/', '?', '#'])
            .next()
            .unwrap_or(hostport)
            .trim();
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) => (h.trim().to_string(), p.trim()),
            // Guessing a port would silently send traffic somewhere unintended.
            None => {
                return Err(format!("'{hostport}' has no port. Use scheme://host:port."))
            }
        };
        if host.is_empty() {
            return Err("The proxy address has no host.".to_string());
        }
        let port: u16 = port
            .parse()
            .map_err(|_| format!("'{port}' is not a valid port (1-65535)."))?;
        if port == 0 {
            return Err("Port 0 is not a valid proxy port.".to_string());
        }
        Ok(Proxy {
            scheme: Some(scheme),
            host,
            port,
            username,
            password,
        })
    }
}

/// How a pool hands out members.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PoolStrategy {
    /// The same app always gets the same member.
    ///
    /// This is the default because a tab that reloads must not change exit IP:
    /// services bind a session to an address, and rotating it mid-session logs
    /// the user out.
    #[default]
    Sticky,
    /// Rotate in sidebar order.
    RoundRobin,
}

/// A named group of proxies.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyPool {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub proxies: Vec<Proxy>,
    #[serde(default)]
    pub strategy: PoolStrategy,
}

impl ProxyPool {
    /// Which member an app gets, or `None` when the pool has nothing usable.
    ///
    /// `app_index` only matters for round-robin; sticky hashes the app id so
    /// reordering the sidebar cannot move an app to a different exit.
    pub fn pick(&self, app_id: &str, app_index: usize) -> Option<&Proxy> {
        let usable: Vec<&Proxy> = self.proxies.iter().filter(|p| p.is_usable()).collect();
        if usable.is_empty() {
            return None;
        }
        let slot = match self.strategy {
            PoolStrategy::Sticky => (stable_hash(app_id) % usable.len() as u64) as usize,
            PoolStrategy::RoundRobin => app_index % usable.len(),
        };
        usable.get(slot).copied()
    }
}

/// FNV-1a. Stable across runs and platforms, which is the only property that
/// matters: the same app must keep the same pool member.
fn stable_hash(value: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The global proxy choice.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalProxy {
    #[serde(default)]
    pub mode: GlobalProxyMode,
    #[serde(default)]
    pub custom: Option<Proxy>,
    /// Pool id rather than a copy, so editing a pool updates every app using it.
    #[serde(default)]
    pub pool_id: Option<String>,
}

/// Which coding agent's header conventions a provider imitates.
///
/// Values are the headers those clients are observed to send. They are a
/// starting point, not a contract: a vendor can change them at any time, which
/// is why the provider editor also accepts arbitrary custom headers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AgentPreset {
    #[default]
    Plain,
    Opencode,
    OhMyPi,
    ClaudeCode,
    Codex,
}

impl AgentPreset {
    /// Every variant, in the order the picker shows them.
    pub const ALL: [AgentPreset; 5] = [
        AgentPreset::Plain,
        AgentPreset::Opencode,
        AgentPreset::OhMyPi,
        AgentPreset::ClaudeCode,
        AgentPreset::Codex,
    ];

    /// Headers this preset contributes, lowest precedence first.
    pub fn headers(self) -> Vec<(String, String)> {
        let pairs: &[(&str, &str)] = match self {
            AgentPreset::Plain => &[],
            // opencode tags every request with project/session/request ids and a
            // client name; the ids are per-request and cannot be preset, so only
            // the constant ones are set here.
            AgentPreset::Opencode => &[
                ("x-opencode-client", "cli"),
                ("x-opencode-project", "global"),
                ("user-agent", "opencode/0.0.0"),
            ],
            // oh-my-pi marks agent-initiated turns so the provider can tell them
            // apart from a human typing in the web UI.
            AgentPreset::OhMyPi => &[("x-initiator", "agent")],
            // Claude Code identifies as a CLI app and asks for direct access.
            AgentPreset::ClaudeCode => &[
                ("x-app", "cli"),
                ("anthropic-dangerous-direct-browser-access", "true"),
                ("user-agent", "claude-cli/0.0.0 (external, cli)"),
            ],
            // Codex CLI sends `originator` plus its own x-codex-* telemetry.
            AgentPreset::Codex => &[
                ("originator", "codex_cli_rs"),
                ("user-agent", "codex_cli_rs/0.0.0"),
            ],
        };
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    pub fn label(self) -> &'static str {
        match self {
            AgentPreset::Plain => "No agent headers",
            AgentPreset::Opencode => "opencode",
            AgentPreset::OhMyPi => "oh-my-pi",
            AgentPreset::ClaudeCode => "Claude Code",
            AgentPreset::Codex => "Codex CLI",
        }
    }

    /// One line explaining what the preset actually sends, shown under the
    /// picker so the choice is not a guess.
    ///
    /// Exported to the page rather than copied into JavaScript: the text
    /// describes headers this module really sends, so the two cannot drift
    /// apart.
    pub fn hint(self) -> &'static str {
        match self {
            AgentPreset::Plain => "Sends only the headers the provider requires.",
            AgentPreset::Opencode => "Sends x-opencode-client and x-opencode-project.",
            AgentPreset::OhMyPi => "Sends X-Initiator: agent.",
            AgentPreset::ClaudeCode => "Sends x-app: cli and the CLI user-agent.",
            AgentPreset::Codex => "Sends originator: codex_cli_rs.",
        }
    }
}

/// Every preset, in picker order, with the label and hint the page shows.
///
/// The list lives here rather than in JavaScript for the same reason the headers
/// do: a preset the page offers but the backend does not know about would be a
/// choice that silently sends nothing.
pub fn agent_presets() -> Vec<AgentPresetRow> {
    AgentPreset::ALL
        .iter()
        .map(|p| AgentPresetRow {
            id: serde_json::to_value(p)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "plain".to_string()),
            label: p.label(),
            hint: p.hint(),
        })
        .collect()
}

/// One row of the agent-header picker.
#[derive(Debug, Clone, Serialize)]
pub struct AgentPresetRow {
    pub id: String,
    pub label: &'static str,
    pub hint: &'static str,
}

/// One header. A list of pairs rather than a map so duplicate names and order
/// survive a JSON round trip.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Header {
    pub name: String,
    pub value: String,
}

/// Wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ApiKind {
    /// `POST {base}/chat/completions`.
    #[default]
    ChatCompletions,
    /// `POST {base}/messages` (Anthropic).
    Anthropic,
    /// `POST {base}/responses`.
    Responses,
}

impl ApiKind {
    /// Path appended to the base URL.
    pub fn path(self) -> &'static str {
        match self {
            ApiKind::ChatCompletions => "/chat/completions",
            ApiKind::Anthropic => "/messages",
            ApiKind::Responses => "/responses",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            ApiKind::ChatCompletions => "Chat Completions",
            ApiKind::Anthropic => "Anthropic Messages",
            ApiKind::Responses => "Responses API",
        }
    }
}

/// Prefix a combo answers to, and the one token a provider may not claim.
///
/// A combo id is always `combo/<name>`, so `combo` is reserved: a provider that
/// took it would make `combo/foo` mean two different things depending on which
/// table was consulted first.
pub const RESERVED_PREFIX: &str = "combo";

/// Turn free text into a usable model-id segment.
///
/// Lowercase, ASCII alphanumerics plus `-`, `_` and `.`; every other run of
/// characters collapses to a single `-`. An empty result stays empty so the
/// caller can decide whether to fall back rather than inventing a name.
pub fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            pending_dash = false;
            out.push(ch.to_ascii_lowercase());
        } else {
            // `/` is deliberately a separator here: a prefix may not contain
            // one, because `prefix/model` is split on the first `/`.
            pending_dash = true;
        }
    }
    out.trim_matches(['-', '.']).to_string()
}

/// A configured model endpoint the chat panel can talk to.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Provider {
    pub id: String,
    pub name: String,
    /// The name this provider's models answer to on the AI Router.
    ///
    /// The router publishes every model a provider serves as
    /// `{prefix}/{model}`, so the prefix is what keeps two providers that both
    /// offer `gpt-4o` apart. Empty means "derive one from the name", which is
    /// what a provider added before this field existed gets: its models are
    /// published immediately instead of appearing to be missing.
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub kind: ApiKind,
    /// Base URL including the version segment, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// Stored in config.json. The UI only ever sees [`Provider::masked`].
    #[serde(default)]
    pub api_key: String,
    /// The model a new conversation starts on. One string because a request
    /// carries exactly one model; the alternatives live in [`Provider::models`].
    #[serde(default)]
    pub model: String,
    /// Every model this provider serves.
    ///
    /// One endpoint usually offers several — a model family, or a router that
    /// fronts many vendors — and the chat panel lets each conversation pick a
    /// different one. Kept separate from `model` so the picker has something to
    /// offer without a second round trip to the provider.
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub agent: AgentPreset,
    /// Extra headers, applied after the preset so a user can override one.
    #[serde(default)]
    pub headers: Vec<Header>,
    /// USD per million input tokens, for the cost estimate in the request log.
    ///
    /// Optional and zero by default: RustBox has no billing relationship with
    /// the provider, so a figure it invents would be worse than a blank. When
    /// both rates are zero the estimator falls back to a small table of
    /// published prices keyed on the model id, and says "estimate" everywhere.
    #[serde(default)]
    pub input_price: f64,
    #[serde(default)]
    pub output_price: f64,
}

impl Provider {
    /// Headers to send: preset first, then the provider's own, so a later entry
    /// with the same name wins (case-insensitively, as HTTP requires).
    pub fn resolved_headers(&self) -> Vec<(String, String)> {
        let mut out = self.agent.headers();
        for extra in &self.headers {
            if extra.name.trim().is_empty() {
                continue;
            }
            let lower = extra.name.to_ascii_lowercase();
            out.retain(|(k, _)| k.to_ascii_lowercase() != lower);
            out.push((extra.name.trim().to_string(), extra.value.clone()));
        }
        out
    }

    /// Whether there is enough here to make a request.
    ///
    /// The model check goes through [`Provider::model_list`], so a provider whose
    /// models live only in the list is still ready.
    pub fn is_ready(&self) -> bool {
        !self.base_url.trim().is_empty() && !self.model_list().is_empty()
    }

    /// The models the picker should offer.
    ///
    /// Falls back to the single `model` when `models` is empty, so a config
    /// written before this list existed keeps working and shows its one model
    /// instead of an empty picker.
    pub fn model_list(&self) -> Vec<String> {
        if !self.models.is_empty() {
            return self.models.clone();
        }
        let single = self.model.trim();
        if single.is_empty() {
            Vec::new()
        } else {
            vec![single.to_string()]
        }
    }

    /// The prefix this provider's models are published under.
    ///
    /// Falls back to a slug of the name, then to the id, so a provider added
    /// before the field existed still publishes under something stable and
    /// readable rather than disappearing from the router. The reserved `combo`
    /// token is never produced: a provider named "Combo" gets `combo-2`.
    pub fn effective_prefix(&self) -> String {
        let explicit = slugify(&self.prefix);
        if !explicit.is_empty() && explicit != RESERVED_PREFIX {
            return explicit;
        }
        let from_name = slugify(&self.name);
        let candidate = if from_name.is_empty() {
            slugify(&self.id)
        } else {
            from_name
        };
        if candidate.is_empty() {
            // Both the name and the id were unusable; `provider` is still a
            // usable segment and is at least honest about where it came from.
            format!("provider-{}", &stable_hash(&self.id) % 10_000)
        } else if candidate == RESERVED_PREFIX {
            // A provider literally named "Combo" must not steal the token the
            // combo namespace answers to, so it gets a suffixed form instead of
            // a refusal: the provider still has to be publishable.
            format!("{candidate}-2")
        } else {
            candidate
        }
    }

    /// Keep `model` and `models` consistent with each other.
    ///
    /// Two directions, both needed: a provider saved with only `model` (an older
    /// config, or a form that never opened the list) gains that model in the
    /// list, and a provider whose `model` was removed from the list falls back to
    /// the first remaining entry rather than pointing at something unlisted.
    ///
    /// An *empty* prefix is left alone: it means "derive one from the name", and
    /// [`Provider::effective_prefix`] does that at read time. Writing the derived
    /// value here would make every load look like a change and would freeze a
    /// name-derived prefix that should follow a rename. A prefix that is present
    /// but unusable — spaces, a slash, or the reserved `combo` — is rewritten,
    /// because that is a value the router could never match.
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;
        if !self.prefix.trim().is_empty() {
            let slug = slugify(&self.prefix);
            if slug.is_empty() || slug == RESERVED_PREFIX {
                self.prefix = String::new();
                changed = true;
            } else if slug != self.prefix {
                self.prefix = slug;
                changed = true;
            }
        }
        let single = self.model.trim().to_string();
        self.models.retain(|m| !m.trim().is_empty());
        for model in self.models.iter_mut() {
            let trimmed = model.trim().to_string();
            if *model != trimmed {
                *model = trimmed;
                changed = true;
            }
        }
        if !single.is_empty() && !self.models.iter().any(|m| m == &single) {
            self.models.insert(0, single.clone());
            changed = true;
        }
        if single.is_empty()
            && let Some(first) = self.models.first().cloned()
        {
            self.model = first;
            changed = true;
        }
        changed
    }

    /// Full request URL: base + the path the protocol uses.
    ///
    /// A base URL that already ends in the protocol path is left alone, because
    /// providers hand out "endpoint" URLs that way and appending again produces
    /// `.../chat/completions/chat/completions`.
    pub fn endpoint(&self) -> String {
        let base = self.base_url.trim().trim_end_matches('/');
        let path = self.kind.path();
        if base.ends_with(path) {
            base.to_string()
        } else {
            format!("{base}{path}")
        }
    }

    /// Headers that carry the credential and the protocol version.
    ///
    /// Anthropic does not use `Authorization`: it authenticates with `x-api-key`
    /// and rejects a request without `anthropic-version`.
    pub fn auth_headers(&self) -> Vec<(String, String)> {
        let key = self.api_key.trim().to_string();
        let mut out: Vec<(String, String)> = Vec::new();
        if !key.is_empty() {
            match self.kind {
                ApiKind::Anthropic => {
                    out.push(("x-api-key".to_string(), key));
                    out.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
                }
                _ => out.push(("authorization".to_string(), format!("Bearer {key}"))),
            }
        }
        out
    }

    /// Every header a request needs, credential and protocol first so a custom
    /// header can still override one deliberately.
    pub fn request_headers(&self) -> Vec<(String, String)> {
        let mut out = self.auth_headers();
        for (name, value) in self.resolved_headers() {
            let lower = name.to_ascii_lowercase();
            out.retain(|(k, _)| k.to_ascii_lowercase() != lower);
            out.push((name, value));
        }
        out
    }

    /// Minimal body used by the connection test: one short turn.
    pub fn probe_body(&self) -> serde_json::Value {
        match self.kind {
            ApiKind::ChatCompletions => serde_json::json!({
                "model": self.model,
                "messages": [{ "role": "user", "content": "ping" }],
                "max_tokens": 1,
                "stream": false,
            }),
            ApiKind::Anthropic => serde_json::json!({
                "model": self.model,
                "max_tokens": 1,
                "messages": [{ "role": "user", "content": "ping" }],
            }),
            ApiKind::Responses => serde_json::json!({
                "model": self.model,
                "input": "ping",
                "max_output_tokens": 16,
            }),
        }
    }

    /// Key form safe to hand to the UI: last four characters only. The full key
    /// stays in the backend so a script in the page cannot read it back out.
    pub fn masked(&self) -> String {
        let key = self.api_key.trim();
        if key.is_empty() {
            return String::new();
        }
        if key.len() <= 4 {
            return "****".to_string();
        }
        format!("****{}", &key[key.len() - 4..])
    }

    /// The same provider with the key replaced by [`Provider::masked`].
    pub fn redacted(&self) -> Provider {
        Provider {
            api_key: self.masked(),
            ..self.clone()
        }
    }
}

/// True when `value` is a masked key rather than a real one.
///
/// The UI is only ever given masked keys, so a form submitted without touching
/// the key field sends the mask back. Treating that as the new key would
/// replace a working credential with the literal string `****1234`.
pub fn is_masked_key(value: &str) -> bool {
    value.trim().starts_with("****")
}

/// Where the MCP bridge listens.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum McpBind {
    /// `127.0.0.1` only.
    #[default]
    Loopback,
    /// Every interface. A deliberate exposure: the token is still required, but
    /// the port becomes reachable from the network, so enabling it is confirmed.
    Network,
}

/// Which tabs an agent may act on.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum McpScope {
    /// Every open tab, whether or not it is on screen. A tab that is merely
    /// hidden is still open and usable.
    #[default]
    AllTabs,
    /// Only the tab currently shown.
    ActiveTab,
}

/// MCP bridge options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpSettings {
    /// Fixed port, or `None` for an ephemeral one. A fixed port is what keeps a
    /// client config valid across launches, so the guidance text can name it.
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub bind: McpBind,
    #[serde(default)]
    pub scope: McpScope,
    /// Allow an agent to add, edit and delete apps. Off by default: it changes
    /// the workspace, not just a page.
    #[serde(default)]
    pub allow_app_edits: bool,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            port: None,
            bind: McpBind::Loopback,
            scope: McpScope::AllTabs,
            allow_app_edits: false,
        }
    }
}

/// Default port for the AI Router.
///
/// 11930 is the number the settings page and the documentation name, and a fixed
/// default is what makes a client config (`http://127.0.0.1:11930/v1`) work on
/// the first try instead of requiring a lookup.
pub const DEFAULT_ROUTER_PORT: u16 = 11930;

/// How a combo picks which member answers.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ComboStrategy {
    /// Try the members in order and return the first usable answer.
    #[default]
    Fallback,
    /// Rotate through the members, one per request, so load spreads.
    RoundRobin,
    /// Ask every member at once and let a judge model synthesize one answer.
    Fusion,
}

impl ComboStrategy {
    pub fn label(self) -> &'static str {
        match self {
            ComboStrategy::Fallback => "Fallback",
            ComboStrategy::RoundRobin => "Round-robin",
            ComboStrategy::Fusion => "Fusion",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            ComboStrategy::Fallback => {
                "Try each model in order and return the first usable answer. A model that errors, times out or answers with nothing is skipped."
            }
            ComboStrategy::RoundRobin => {
                "Rotate through the models one request at a time, so no single provider carries every call."
            }
            ComboStrategy::Fusion => {
                "Ask every model at once, then have the judge model combine the answers into one. Costs one call per member."
            }
        }
    }
}

/// A named group of published models that answers as a single model id.
///
/// Lives in the `combo/` namespace, which is why `combo` is a reserved provider
/// prefix: `combo/x` must always mean a combo.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Combo {
    pub id: String,
    /// The segment after `combo/`. Slugs are kept in sync with [`Combo::label`]
    /// by [`Combo::normalize`].
    pub name: String,
    /// Display label; empty falls back to the slug.
    #[serde(default)]
    pub label: String,
    /// Full published model ids (`prefix/model`), in priority order.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub strategy: ComboStrategy,
    /// Full model id used to synthesize a fusion answer. Empty means "the first
    /// member that answered".
    #[serde(default)]
    pub judge: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Combo {
    /// The id a client passes as `model`.
    pub fn model_id(&self) -> String {
        format!("{RESERVED_PREFIX}/{}", self.name)
    }

    pub fn display(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.name
        } else {
            &self.label
        }
    }

    /// Repair a combo whose name is unusable or collides with a sibling.
    ///
    /// Runs on load, not only on save: a config edited by hand, or a combo
    /// created before names were slugged, would otherwise be unreachable — the
    /// router could list it and never match it.
    pub fn normalize(&mut self, taken: &[String]) -> bool {
        let mut changed = false;
        let mut wanted = slugify(&self.name);
        if wanted.is_empty() {
            wanted = slugify(self.display());
        }
        if wanted.is_empty() {
            wanted = slugify(&self.id);
        }
        if wanted.is_empty() {
            wanted = "combo".to_string();
        }
        // A duplicate name would make one of the two unreachable, and
        // `combo/<name>` cannot carry a disambiguator the user would know about.
        if taken.iter().any(|t| t == &wanted) {
            let mut n = 2;
            loop {
                let candidate = format!("{wanted}-{n}");
                if !taken.iter().any(|t| t == &candidate) {
                    wanted = candidate;
                    break;
                }
                n += 1;
            }
        }
        if self.name != wanted {
            self.name = wanted;
            changed = true;
        }
        self.members.retain(|m| !m.trim().is_empty());
        for member in self.members.iter_mut() {
            let trimmed = member.trim().to_string();
            if *member != trimmed {
                *member = trimmed;
                changed = true;
            }
        }
        if self.judge.trim().is_empty()
            && self.strategy == ComboStrategy::Fusion
            && let Some(first) = self.members.first().cloned()
        {
            self.judge = first;
            changed = true;
        }
        changed
    }
}

/// How the Cloudflare tunnel is authenticated.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TunnelMode {
    /// `cloudflared tunnel --url`: a random `*.trycloudflare.com` host that
    /// needs no Cloudflare account. The address changes on every start, which
    /// is why the page shows the live one instead of a saved string.
    #[default]
    Quick,
    /// A tunnel the user created in the Cloudflare dashboard, run with its
    /// token. The hostname is stable, so it is stored.
    Token,
}

/// Cloudflare tunnel options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelSettings {
    #[serde(default)]
    pub mode: TunnelMode,
    /// Stored in config.json; the UI only ever sees it masked.
    #[serde(default)]
    pub token: String,
    /// The public hostname a token-mode tunnel is published at, e.g.
    /// `router.example.com`. Only used to show the client URL.
    #[serde(default)]
    pub hostname: String,
}

impl Default for TunnelSettings {
    fn default() -> Self {
        Self {
            mode: TunnelMode::Quick,
            token: String::new(),
            hostname: String::new(),
        }
    }
}

/// One API key a client can present to the router.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RouterKey {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// The secret. Stored in config.json and never sent back to the page in the
    /// clear, exactly like a provider key.
    #[serde(default)]
    pub key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Restricted access: which published models this key may call.
    ///
    /// Empty means every published model. Entries are matched against the full
    /// model id (`risun/gpt-4o`) as glob patterns (`*`, `?`), and an entry
    /// starting with `!` removes a match — so `["risun/*", "!risun/expensive"]`
    /// is expressible. This is the same idea 9router uses, and it is the only
    /// shape that stays readable once a workspace has thirty models.
    #[serde(default)]
    pub models: Vec<String>,
    /// Unix milliseconds, so the page can show "added 3 days ago".
    #[serde(default)]
    pub created_at: u64,
}

impl RouterKey {
    /// Key form safe to hand to the UI: last four characters only.
    pub fn masked(&self) -> String {
        let key = self.key.trim();
        if key.is_empty() {
            return String::new();
        }
        if key.len() <= 4 {
            return "****".to_string();
        }
        format!("****{}", &key[key.len() - 4..])
    }

    pub fn redacted(&self) -> RouterKey {
        RouterKey {
            key: self.masked(),
            ..self.clone()
        }
    }

    /// Whether this key may call `model_id`.
    ///
    /// An empty list is "everything", because a key created before the
    /// restriction existed must not silently lose access. A list containing
    /// only negations starts from "everything" too, which is what
    /// `["!opus"]` is asking for.
    pub fn allows(&self, model_id: &str) -> bool {
        let patterns: Vec<&str> = self
            .models
            .iter()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
            .collect();
        if patterns.is_empty() {
            return true;
        }
        let has_positive = patterns.iter().any(|p| !p.starts_with('!'));
        let mut allowed = !has_positive;
        for pattern in patterns {
            if let Some(negated) = pattern.strip_prefix('!') {
                if glob_match(negated.trim(), model_id) {
                    allowed = false;
                }
            } else if glob_match(pattern, model_id) {
                allowed = true;
            }
        }
        allowed
    }
}

/// Case-insensitive glob match supporting `*` and `?`.
///
/// Hand-rolled because the only alternative is a dependency for two characters,
/// and the semantics need to be exactly "model id matching" — not filesystem
/// matching, where `*` would stop at a separator.
pub fn glob_match(pattern: &str, value: &str) -> bool {
    let pattern: Vec<char> = pattern.to_ascii_lowercase().chars().collect();
    let value: Vec<char> = value.to_ascii_lowercase().chars().collect();
    // Iterative matcher with backtracking on the last `*`: linear in practice
    // and constant stack, unlike the recursive form.
    let (mut p, mut v) = (0usize, 0usize);
    let (mut star, mut resume) = (usize::MAX, 0usize);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = p;
            resume = v;
            p += 1;
        } else if star != usize::MAX {
            p = star + 1;
            resume += 1;
            v = resume;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Which requests the router records, and how many it keeps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterLogSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Entries kept on disk. Bounded on purpose: an unbounded request log is a
    /// file that grows until someone notices the disk.
    #[serde(default = "default_log_limit")]
    pub limit: usize,
}

impl Default for RouterLogSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            limit: default_log_limit(),
        }
    }
}

impl RouterLogSettings {
    /// The row count to prune to, accepting only the two offered values.
    ///
    /// A config edited by hand could name 100 000; the page only offers 50 and
    /// 100, so anything else is clamped to the nearest allowed value instead of
    /// letting the log grow to whatever the file says.
    pub fn effective_limit(&self) -> usize {
        match self.limit {
            0..=74 => 50,
            _ => 100,
        }
    }
}

fn default_log_limit() -> usize {
    100
}

/// The AI Router: a local endpoint that republishes every provider model under
/// `prefix/model`, plus every combo under `combo/name`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterSettings {
    /// Whether the listener runs. Off by default: opening a port is a deliberate
    /// act, not something a settings page should do on its own.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_router_port")]
    pub port: u16,
    /// Publish beyond this machine. Off means loopback only, which is the safe
    /// default and the state the router is designed to work in.
    #[serde(default)]
    pub publish: bool,
    /// Agent headers sent upstream, on top of whatever the provider itself
    /// carries. The same presets the provider editor offers.
    #[serde(default)]
    pub agent: AgentPreset,
    #[serde(default)]
    pub tunnel: TunnelSettings,
    /// Publish over the Tailscale interface instead of the LAN.
    #[serde(default)]
    pub tailscale: bool,
    #[serde(default)]
    pub keys: Vec<RouterKey>,
    #[serde(default)]
    pub log: RouterLogSettings,
}

impl Default for RouterSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            port: default_router_port(),
            publish: false,
            agent: AgentPreset::Plain,
            tunnel: TunnelSettings::default(),
            tailscale: false,
            keys: Vec::new(),
            log: RouterLogSettings::default(),
        }
    }
}

fn default_router_port() -> u16 {
    DEFAULT_ROUTER_PORT
}

impl RouterSettings {
    /// The port, repaired when it cannot be bound.
    ///
    /// Port 0 is the fingerprint of a config edited by hand or written by a
    /// build that had no default: binding it would put the router on a random
    /// port that no client config could name.
    pub fn effective_port(&self) -> u16 {
        if self.port == 0 { DEFAULT_ROUTER_PORT } else { self.port }
    }

    pub fn normalize(&mut self) -> bool {
        let mut changed = false;
        if self.port == 0 {
            self.port = DEFAULT_ROUTER_PORT;
            changed = true;
        }
        if self.log.limit == 0 {
            self.log.limit = default_log_limit();
            changed = true;
        }
        for key in self.keys.iter_mut() {
            if key.key.trim().is_empty() {
                key.key = new_router_key();
                changed = true;
            }
            if key.id.trim().is_empty() {
                key.id = format!("key-{}", &stable_hash(&key.key) % 1_000_000);
                changed = true;
            }
            if key.name.trim().is_empty() {
                key.name = "Key".to_string();
                changed = true;
            }
        }
        changed
    }

    /// The key that matches `presented`, if any.
    ///
    /// Disabled keys are skipped here rather than at the call site, so a key
    /// switched off in the page cannot authenticate through any path.
    pub fn key_for(&self, presented: &str) -> Option<&RouterKey> {
        self.keys.iter().find(|k| {
            k.enabled && !k.key.is_empty() && constant_time_eq(k.key.trim(), presented.trim())
        })
    }
}

/// Compare two secrets without leaking their length or content through timing.
///
/// The router is a local endpoint, but a key is still a credential and the
/// comparison is two lines.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// A fresh router key: `sk-rbx-` plus 32 hex characters.
///
/// The prefix makes a key recognisable in a config file or a pasted bug report,
/// and the random part is what actually guards the endpoint.
pub fn new_router_key() -> String {
    format!("sk-rbx-{}", random_hex(32))
}

/// `bytes * 2` lowercase hex characters from the OS random source.
///
/// Sourced from `getrandom` through `std`'s hasher seeding is not available, so
/// this mixes the OS-provided `RandomState` (seeded from the platform CSPRNG)
/// with a monotonic counter and the clock. Enough for a local API key that is
/// also rate-limited by being local; not a substitute for a CSPRNG in a
/// network-exposed product, which this is not by default.
fn random_hex(chars: usize) -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(chars);
    while out.len() < chars {
        let state = RandomState::new();
        let mut hasher = state.build_hasher();
        hasher.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
        );
        hasher.write_u64(out.len() as u64);
        let value = hasher.finish();
        out.push_str(&format!("{value:016x}"));
    }
    out.truncate(chars);
    out
}

/// Where a link opened from inside an app tab goes.
///
/// A page can ask for a new window (`target="_blank"`, `window.open`) at any
/// time, and WebView2's answer with no handler installed is to drop the request
/// silently — the click looks like it did nothing at all. That is why links
/// inside the embedded browser appeared dead.
///
/// `Ask` is the default because every other value is a guess about intent that
/// the click did not make. A link that opens a new window is a page asking for
/// a *second* surface, and answering that with "replace what you are reading"
/// (which is what the old `Same` default did) silently destroys the page the
/// user was on. One dialog per click, with a "remember" box, converges on the
/// right per-app answer within a day of normal use without ever guessing wrong
/// on the first click.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum LinkTarget {
    /// Show the chooser and let the user decide. Default.
    #[default]
    Ask,
    /// Follow the link in the sub-tab it was clicked from. The session, the
    /// profile and the login all carry over, and the tab keeps its history, so
    /// Back returns to the page the link was clicked from — but the page the
    /// user was reading is replaced.
    Same,
    /// Open the link in a new sub-tab of the same app: same profile, same
    /// cookies, same session, and the page it came from stays where it was.
    NewTab,
    /// Open the link in its own floating window, sharing the app's profile and
    /// session. For "open this alongside" cases the page itself asks for with
    /// `window.open(url, name, 'width=…,height=…')`.
    Popup,
    /// Hand the link to the OS default browser. Predictable, and it keeps RustBox
    /// out of the way, but that browser has its own cookie jar, so a link that
    /// needs the app's login lands on a login wall.
    External,
}

impl LinkTarget {
    /// Parse the wire value used by the UI, the chat tools and the MCP bridge.
    ///
    /// One parser for all three call sites: they used to each match on two
    /// strings, so adding a variant meant finding every one of them or silently
    /// ignoring the new value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" => Some(Self::Ask),
            "same" | "here" => Some(Self::Same),
            "new-tab" | "newtab" => Some(Self::NewTab),
            "popup" => Some(Self::Popup),
            "external" | "browser" => Some(Self::External),
            _ => None,
        }
    }

    /// The same wire value `parse` accepts, for echoing a choice back.
    ///
    /// Hand-written rather than derived from serde so the answer a caller gets
    /// is exactly the token it can send back, including through `parse`, which
    /// accepts aliases serde does not.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Same => "same",
            Self::NewTab => "new-tab",
            Self::Popup => "popup",
            Self::External => "external",
        }
    }
}

#[cfg(test)]
mod link_target_tests {
    use super::LinkTarget;

    #[test]
    fn every_wire_value_round_trips() {
        // The value the UI sends must survive a trip through the enum and back,
        // or a saved choice silently reverts to the default on reload.
        for (wire, want) in [
            ("ask", LinkTarget::Ask),
            ("same", LinkTarget::Same),
            ("new-tab", LinkTarget::NewTab),
            ("popup", LinkTarget::Popup),
            ("external", LinkTarget::External),
        ] {
            assert_eq!(LinkTarget::parse(wire), Some(want), "parse({wire})");
            let json = serde_json::to_string(&want).unwrap();
            assert_eq!(json, format!("\"{wire}\""), "serde name for {want:?}");
        }
    }

    #[test]
    fn the_aliases_the_ui_may_send_are_accepted() {
        assert_eq!(LinkTarget::parse("here"), Some(LinkTarget::Same));
        assert_eq!(LinkTarget::parse("Browser"), Some(LinkTarget::External));
        assert_eq!(LinkTarget::parse(" nope "), None);
    }
}

/// Where a skill came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SkillSource {
    #[default]
    Manual,
    Marketplace,
}

/// A reusable instruction block the chat agent can load.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Skill {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The instructions themselves, injected into the system prompt.
    #[serde(default)]
    pub instructions: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub source: SkillSource,
    #[serde(default)]
    pub url: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Theme choice. `System` follows the OS.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

/// Chat panel preferences.
///
/// `Default` is written out by hand rather than derived: the serde defaults say
/// tools and skills are on and the history window is 24, and a derived `Default`
/// would silently disagree (false, false, 0). The disagreement is invisible on a
/// config that already has a `chat` block and produces a chat panel that sends no
/// tools and two turns of context on a fresh install.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatSettings {
    /// Provider used for new conversations.
    #[serde(default)]
    pub provider_id: Option<String>,
    /// Model override; empty means the provider's own model.
    #[serde(default)]
    pub model: String,
    /// Let the agent call browser and workspace tools.
    #[serde(default = "default_true")]
    pub tools_enabled: bool,
    /// Include enabled skills in the system prompt.
    #[serde(default = "default_true")]
    pub use_skills: bool,
    /// Trim old turns out of the prompt so a long conversation does not grow
    /// without bound. Keeps the token bill proportional to the recent context.
    #[serde(default = "default_history_limit")]
    pub history_limit: usize,
}

impl Default for ChatSettings {
    fn default() -> Self {
        ChatSettings {
            provider_id: None,
            model: String::new(),
            tools_enabled: true,
            use_skills: true,
            history_limit: default_history_limit(),
        }
    }
}

fn default_history_limit() -> usize {
    24
}

/// The settings block persisted inside `config.json`.
///
/// A `window` block is no longer part of this struct. It only ever held
/// `always_on_top`, which made the shell a topmost window and broke Alt+Tab
/// (a topmost window sits above everything, including the switcher's
/// foreground pick), so the whole feature was removed. Older configs still
/// carry the key; serde ignores fields it does not know, so they load clean.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    #[serde(default)]
    pub global_proxy: GlobalProxy,
    #[serde(default)]
    pub pools: Vec<ProxyPool>,
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default)]
    pub mcp: McpSettings,
    #[serde(default)]
    pub skills: Vec<Skill>,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub chat: ChatSettings,
    #[serde(default)]
    pub startup: StartupSettings,
    #[serde(default)]
    pub router: RouterSettings,
    #[serde(default)]
    pub combos: Vec<Combo>,
}

/// What RustBox does when it launches.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct StartupSettings {
    /// Build every enabled embedded app at launch, not just the tab that was
    /// open last.
    ///
    /// On by default because that is what a multi-app workspace is for: an app's
    /// session, unread badge and push channel should be live before the user
    /// clicks it, instead of the click starting a cold page load. The cost is
    /// one webview per app and the memory that goes with it, so it is a switch
    /// rather than a rule.
    #[serde(default = "default_true")]
    pub warm_start: bool,
}

impl Default for StartupSettings {
    fn default() -> Self {
        Self { warm_start: true }
    }
}

impl Settings {
    /// Repair values that cannot mean what they say.
    ///
    /// Run on every load, not as a versioned migration: these are invariants,
    /// not schema changes.
    ///
    /// `history_limit: 0` is the fingerprint. It means "send only the first
    /// message", which is not a preference anyone can hold on purpose, and it is
    /// what an earlier build wrote when its `Default` disagreed with its serde
    /// defaults. The same block also carried `tools_enabled: false` and
    /// `use_skills: false` from that same default, so the whole block is
    /// restored — but only when the fingerprint is present, because otherwise
    /// "tools off" is a real choice and must be left alone.
    ///
    /// Returns whether anything changed, so the caller can persist the repair:
    /// healing only in memory leaves the bad values on disk, where the next
    /// build that reads them (or a user editing the file) sees them again.
    pub fn normalize(&mut self) -> bool {
        let mut changed = false;
        if self.chat.history_limit == 0 {
            let defaults = ChatSettings::default();
            self.chat.tools_enabled = defaults.tools_enabled;
            self.chat.use_skills = defaults.use_skills;
            self.chat.history_limit = defaults.history_limit;
            changed = true;
        }
        // Every provider carries its own model list, and a provider added before
        // the list existed has to keep working: `normalize` copies its single
        // model into the list instead of leaving the picker empty.
        for provider in self.providers.iter_mut() {
            changed |= provider.normalize();
        }
        changed |= self.router.normalize();
        // Combo names are model ids, so they must be unique and slug-shaped.
        // Collecting the taken names first and then normalizing against that
        // list keeps the result deterministic: the first combo keeps its name.
        let mut taken: Vec<String> = Vec::new();
        for combo in self.combos.iter_mut() {
            if combo.normalize(&taken) {
                changed = true;
            }
            taken.push(combo.name.clone());
        }
        changed
    }
}

/// What one app should connect through, once every level has been applied.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    /// Connect directly.
    Direct,
    /// Use this proxy.
    Use(Proxy),
    /// The app asked for a pool that cannot supply one.
    ///
    /// Reported rather than falling back to direct: a silent fallback would
    /// send traffic that was meant to be tunnelled out in the clear.
    PoolUnavailable(String),
}

/// Resolve one app's proxy.
///
/// `app_index` only matters for a round-robin pool.
pub fn resolve(settings: &Settings, app: &crate::config::App, app_index: usize) -> Resolved {
    match app.proxy_mode {
        ProxyMode::None => Resolved::Direct,
        ProxyMode::Custom => match &app.proxy {
            Some(proxy) if proxy.is_usable() => Resolved::Use(proxy.clone()),
            _ => Resolved::Direct,
        },
        ProxyMode::Inherit => resolve_global(settings, &app.id, app_index),
    }
}

/// Resolve the global setting on its own, so the settings page can preview it
/// without inventing an app.
pub fn resolve_global(settings: &Settings, app_id: &str, app_index: usize) -> Resolved {
    let global = &settings.global_proxy;
    match global.mode {
        GlobalProxyMode::None => Resolved::Direct,
        GlobalProxyMode::Custom => match &global.custom {
            Some(proxy) if proxy.is_usable() => Resolved::Use(proxy.clone()),
            _ => Resolved::Direct,
        },
        GlobalProxyMode::Pool => {
            let Some(pool_id) = global.pool_id.as_deref() else {
                return Resolved::PoolUnavailable("no pool selected".to_string());
            };
            let Some(pool) = settings.pools.iter().find(|p| p.id == pool_id) else {
                return Resolved::PoolUnavailable("the selected pool was deleted".to_string());
            };
            match pool.pick(app_id, app_index) {
                Some(proxy) => Resolved::Use(proxy.clone()),
                None => Resolved::PoolUnavailable(format!(
                    "pool \"{}\" has no usable proxies",
                    pool.name
                )),
            }
        }
    }
}

/// Values the settings page needs that come from the environment rather than the
/// config, so the UI can build copy-paste guidance without guessing.
#[derive(Serialize)]
pub struct EnvFacts {
    /// Non-loopback IPv4 addresses, so the LAN guidance can name the real one.
    pub lan_addresses: Vec<String>,
    /// Best-effort public address. `None` means the lookup failed, which the UI
    /// must state instead of printing a wrong address.
    pub public_address: Option<String>,
    pub platform: String,
}

/// Collect the addresses used to write MCP guidance.
///
/// The public lookup is deliberately not attempted here: it needs a network
/// round trip, and the settings page must open instantly. `public_address` is
/// filled in by the caller when the user asks for it.
pub fn env_facts() -> EnvFacts {
    EnvFacts {
        lan_addresses: lan_ipv4_addresses(),
        public_address: None,
        platform: std::env::consts::OS.to_string(),
    }
}

/// Non-loopback, non-link-local IPv4 addresses of this machine.
fn lan_ipv4_addresses() -> Vec<String> {
    // A UDP socket to a public address performs no traffic; it only makes the
    // OS pick the interface it would route through, which reveals the address.
    let mut out = Vec::new();
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0")
        && socket.connect("8.8.8.8:80").is_ok()
        && let Ok(addr) = socket.local_addr()
    {
        out.push(addr.ip().to_string());
    }
    out
}

/// The address a client on the same network would use, when there is one.
///
/// `None` means the machine has no routable interface, which the page states
/// rather than printing `0.0.0.0` as if it were an address.
pub fn network_address() -> Option<String> {
    lan_ipv4_addresses().into_iter().next()
}

/// Ask a third party what address this machine appears to come from.
///
/// Only used when the user asks for it: the request itself reveals that this
/// machine is online to whoever answers it, which is not something to do
/// silently while a settings page opens.
pub async fn public_address() -> Result<String, String> {
    let client = probe_client(None)?;
    let resp = client
        .get(IP_ECHO)
        .send()
        .await
        .map_err(|e| format!("could not reach the address service: {e}"))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("unexpected reply from the address service: {e}"))?;
    body.get("ip")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "the address service returned no address".to_string())
}

/// A client for one probe.
///
/// `reqwest` only accepts a proxy on the builder, not on a finished client, so a
/// proxied probe needs its own client. That is fine here: a probe runs once.
fn probe_client(proxy: Option<&Proxy>) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .user_agent("rustbox-probe/1.0")
        .connect_timeout(std::time::Duration::from_secs(8))
        .timeout(std::time::Duration::from_secs(20));
    if let Some(proxy) = proxy {
        // `to_uri` keeps credentials inline; reqwest parses them into basic auth
        // for both http and socks5, which is what a pool entry needs.
        let configured = reqwest::Proxy::all(proxy.to_uri())
            .map_err(|e| format!("that proxy address is not usable: {e}"))?;
        builder = builder.proxy(configured);
    }
    builder
        .build()
        .map_err(|e| format!("could not start the test client: {e}"))
}

/// Endpoint that echoes the caller's address, used to prove a proxy works.
const IP_ECHO: &str = "https://api.ipify.org?format=json";

/// Outcome of testing a proxy or a provider.
///
/// One shape for both because the settings page shows them the same way: a tick
/// or a reason, plus how long it took.
#[derive(Serialize)]
pub struct Probe {
    pub ok: bool,
    /// What happened, in words the user can act on.
    pub message: String,
    pub ms: u64,
    /// Address the target echoed back, for a proxy test. `None` when the service
    /// did not report one, which is not a failure.
    pub exit_ip: Option<String>,
}

/// Relay a request through `proxy` and report the address it came from.
///
/// The test fetches an address echo rather than a fixed page: a 200 through the
/// proxy only proves it forwarded something, while the echoed address proves the
/// traffic actually left from the proxy and not from this machine.
pub async fn test_proxy(proxy: &Proxy) -> Probe {
    let started = std::time::Instant::now();
    let fail = |message: String| Probe {
        ok: false,
        message,
        ms: started.elapsed().as_millis() as u64,
        exit_ip: None,
    };

    if !proxy.is_usable() {
        return fail("That proxy has no host or no port.".to_string());
    }
    let client = match probe_client(Some(proxy)) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };

    match client.get(IP_ECHO).send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            let exit_ip = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("ip").and_then(|i| i.as_str()).map(str::to_string));
            if !status.is_success() {
                return Probe {
                    ok: false,
                    message: format!(
                        "The proxy answered with HTTP {}: {}",
                        status.as_u16(),
                        truncate(body.trim(), 200)
                    ),
                    ms: started.elapsed().as_millis() as u64,
                    exit_ip,
                };
            }
            Probe {
                ok: true,
                message: match &exit_ip {
                    Some(ip) => format!("Working. Traffic left from {ip}."),
                    None => "Working, but the test service reported no address.".to_string(),
                },
                ms: started.elapsed().as_millis() as u64,
                exit_ip,
            }
        }
        Err(e) => fail(explain(&e)),
    }
}

/// Send one minimal request to a provider.
///
/// The point is to separate the three things that go wrong here — wrong URL,
/// rejected key, unknown model — so the message names which one it was instead
/// of saying "failed".
pub async fn test_provider(provider: &Provider) -> Probe {
    let started = std::time::Instant::now();
    let fail = |message: String| Probe {
        ok: false,
        message,
        ms: started.elapsed().as_millis() as u64,
        exit_ip: None,
    };

    if provider.base_url.trim().is_empty() {
        return fail("Enter a base URL first.".to_string());
    }
    if provider.model.trim().is_empty() {
        return fail("Enter a model name first.".to_string());
    }

    let client = match probe_client(None) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let mut request = client
        .post(provider.endpoint())
        .header("content-type", "application/json")
        .json(&provider.probe_body());
    for (name, value) in provider.request_headers() {
        request = request.header(name, value);
    }

    match request.send().await {
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if status.is_success() {
                Probe {
                    ok: true,
                    message: format!("Connected. The model replied (HTTP {}).", status.as_u16()),
                    ms: started.elapsed().as_millis() as u64,
                    exit_ip: None,
                }
            } else {
                // Providers put the real reason in the body: `invalid_api_key`,
                // `model_not_found`, `insufficient_quota`.
                Probe {
                    ok: false,
                    message: format!("HTTP {}: {}", status.as_u16(), truncate(body.trim(), 400)),
                    ms: started.elapsed().as_millis() as u64,
                    exit_ip: None,
                }
            }
        }
        Err(e) => fail(explain(&e)),
    }
}

/// Flatten a reqwest error into the chain that actually explains it.
///
/// `Display` on its own says "error sending request", which names nothing. The
/// source chain is what says "connection refused" or "certificate expired".
fn explain(error: &reqwest::Error) -> String {
    let mut out = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let text = cause.to_string();
        if !out.contains(&text) {
            out.push_str(&format!(": {text}"));
        }
        source = cause.source();
    }
    out
}

/// Trim a string to `max` characters, marking the cut.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// Public form of [`truncate`], for anything that has to hand a model or the UI
/// a bounded amount of text.
pub fn shorten(text: &str, max: usize) -> String {
    truncate(text, max)
}

/// Deduplicated provider list for the chat model picker, with keys masked.
#[derive(Serialize)]
pub struct ProviderSummary {
    pub id: String,
    pub name: String,
    pub model: String,
    /// Every model the provider serves, so the picker can offer them all
    /// without a second round trip. Falls back to `[model]` for a provider that
    /// has not been given a list.
    pub models: Vec<String>,
    pub kind: String,
    pub agent: String,
    pub ready: bool,
    pub key_masked: String,
}

/// Map provider ids to summaries. Used by the chat panel, which must never see a
/// raw key.
pub fn provider_summaries(settings: &Settings) -> Vec<ProviderSummary> {
    settings
        .providers
        .iter()
        .map(|p| ProviderSummary {
            id: p.id.clone(),
            name: p.name.clone(),
            model: p.model.clone(),
            models: p.model_list(),
            kind: p.kind.label().to_string(),
            agent: p.agent.label().to_string(),
            ready: p.is_ready(),
            key_masked: p.masked(),
        })
        .collect()
}

/// The settings blob the UI is allowed to see: every provider key replaced by
/// its masked form.
///
/// The page still has to render the provider list and let the user edit
/// everything else, so the whole structure is sent; only the secret is
/// substituted. [`merge_secrets`] reverses the substitution on save.
pub fn redacted(settings: &Settings) -> Settings {
    let mut out = Settings {
        providers: settings.providers.iter().map(Provider::redacted).collect(),
        ..settings.clone()
    };
    out.router.keys = settings.router.keys.iter().map(RouterKey::redacted).collect();
    out
}

/// Carry stored API keys onto an incoming settings blob.
///
/// The UI never receives a real key, so a form that did not touch the key field
/// sends the mask back (or an empty string). Writing either to disk would
/// replace a working credential with `****1234`, so a key that is empty or
/// masked keeps whatever is already stored for that provider id.
///
/// The router's own keys and Cloudflare tunnel token follow the same rule, for
/// the same reason: they are secrets the page is never trusted with.
pub fn merge_secrets(incoming: &mut Settings, stored: &Settings) {
    for provider in incoming.providers.iter_mut() {
        let unchanged = provider.api_key.trim().is_empty() || is_masked_key(&provider.api_key);
        if !unchanged {
            continue;
        }
        let kept = stored
            .providers
            .iter()
            .find(|p| p.id == provider.id)
            .map(|p| p.api_key.clone())
            .unwrap_or_default();
        provider.api_key = kept;
    }
    for key in incoming.router.keys.iter_mut() {
        if !(key.key.trim().is_empty() || is_masked_key(&key.key)) {
            continue;
        }
        key.key = stored
            .router
            .keys
            .iter()
            .find(|k| k.id == key.id)
            .map(|k| k.key.clone())
            // A new key with no secret is generated rather than left blank: a
            // blank key can never authenticate, so saving one would create a
            // credential the page claims exists and the router rejects.
            .unwrap_or_else(new_router_key);
    }
    let tunnel = &mut incoming.router.tunnel;
    if tunnel.token.trim().is_empty() || is_masked_key(&tunnel.token) {
        tunnel.token = stored.router.tunnel.token.clone();
    }
}

/// Which MCP guidance variants the settings page should show.
///
/// Loopback-only means one guide: the same client config works for every editor
/// on this machine. A published bridge needs three, because the address a client
/// must use depends on where that client runs.
pub fn mcp_guides(bind: McpBind) -> Vec<&'static str> {
    match bind {
        McpBind::Loopback => vec!["localhost"],
        McpBind::Network => vec!["localhost", "lan", "public"],
    }
}

/// One combo strategy, with the sentence explaining it.
///
/// Exported to the page for the same reason the agent presets are: a strategy the
/// picker offers but the router does not implement would be a choice that
/// silently does nothing.
#[derive(Debug, Clone, Serialize)]
pub struct ComboStrategyRow {
    pub id: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
}

pub fn combo_strategies() -> Vec<ComboStrategyRow> {
    [
        ComboStrategy::Fallback,
        ComboStrategy::RoundRobin,
        ComboStrategy::Fusion,
    ]
    .iter()
    .map(|s| ComboStrategyRow {
        id: match s {
            ComboStrategy::Fallback => "fallback",
            ComboStrategy::RoundRobin => "round-robin",
            ComboStrategy::Fusion => "fusion",
        },
        label: s.label(),
        hint: s.hint(),
    })
    .collect()
}

/// One model the router publishes right now.
///
/// The page shows this list so "what is exposed" is a fact rather than an
/// inference from the provider list: a provider with no models, or one whose
/// prefix collided, is visible here instead of being discovered by a client that
/// gets a 404.
#[derive(Debug, Clone, Serialize)]
pub struct PublishedRow {
    /// The id a client passes as `model`, e.g. `risun/bansos/deepseek-v4.1`.
    pub id: String,
    pub provider_id: String,
    pub provider_name: String,
    /// The model id without the prefix, i.e. what is sent upstream.
    pub upstream: String,
    /// `provider` or `combo`.
    pub source: &'static str,
    /// How many models this prefix publishes, so a duplicate prefix is visible.
    pub prefix_count: usize,
}

/// Build the published-model list from the live catalogue.
pub fn published_rows(
    settings: &Settings,
    catalogue: &crate::ai_router::Catalogue,
) -> Vec<PublishedRow> {
    let counts = {
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for model in &catalogue.models {
            let prefix = model.id.split('/').next().unwrap_or_default();
            *counts.entry(prefix).or_insert(0) += 1;
        }
        counts
    };
    let _ = settings;
    catalogue
        .models
        .iter()
        .map(|m| {
            let prefix = m.id.split('/').next().unwrap_or_default();
            PublishedRow {
                id: m.id.clone(),
                provider_id: m.provider_id.clone(),
                provider_name: m.provider_name.clone(),
                upstream: m.upstream.clone(),
                source: "provider",
                prefix_count: counts.get(prefix).copied().unwrap_or(0),
            }
        })
        .chain(catalogue.combos.iter().map(|c| PublishedRow {
            id: c.model_id(),
            provider_id: c.id.clone(),
            provider_name: c.display().to_string(),
            upstream: c.members.join(", "),
            source: "combo",
            prefix_count: 1,
        }))
        .collect()
}

/// A provider prefix that would collide with another one, or is reserved.
///
/// Returns the reason, so the page can say why instead of only refusing.
pub fn prefix_problem(settings: &Settings, provider_id: &str, prefix: &str) -> Option<String> {
    let slug = slugify(prefix);
    if slug.is_empty() {
        return Some("a prefix must contain at least one letter or digit".to_string());
    }
    if slug == RESERVED_PREFIX {
        return Some(format!(
            "'{RESERVED_PREFIX}' is reserved for combos, so a model called \
             {RESERVED_PREFIX}/… always means a combo. Pick another prefix."
        ));
    }
    let taken = settings
        .providers
        .iter()
        .filter(|p| p.id != provider_id)
        .any(|p| p.effective_prefix() == slug);
    if taken {
        return Some(format!(
            "'{slug}' is already used by another provider. Two providers sharing a \
             prefix would make {slug}/… ambiguous."
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{App, Engine, ProfileMode};

    fn proxy(host: &str, port: u16) -> Proxy {
        Proxy {
            scheme: Some("http".to_string()),
            host: host.to_string(),
            port,
            username: None,
            password: None,
        }
    }

    fn app(id: &str, mode: ProxyMode, own: Option<Proxy>) -> App {
        App {
            id: id.to_string(),
            name: id.to_string(),
            url: "https://example.com".to_string(),
            icon: None,
            engine: Engine::OsWebview,
            external_browser_path: None,
            session: None,
            profile_mode: ProfileMode::Isolated,
            fingerprint: None,
            external_args: Vec::new(),
            proxy_mode: mode,
            proxy: own,
            user_agent: None,
            link_target: Default::default(),
            enabled: true,
            muted: false,
            notifications: true,
        }
    }

    fn settings_with_pool(strategy: PoolStrategy) -> Settings {
        let mut settings = Settings::default();
        settings.global_proxy.mode = GlobalProxyMode::Pool;
        settings.global_proxy.pool_id = Some("p1".to_string());
        settings.pools.push(ProxyPool {
            id: "p1".to_string(),
            name: "Main".to_string(),
            proxies: vec![proxy("10.0.0.1", 8001), proxy("10.0.0.2", 8002)],
            strategy,
        });
        settings
    }

    #[test]
    fn chat_defaults_match_the_serde_defaults() {
        // A derived `Default` here would say false/false/0 while serde says
        // true/true/24, and nothing would notice: a config that already has a
        // `chat` block deserializes field by field, so the disagreement only
        // shows on a fresh install, where it silently disables tools and cuts
        // the prompt to two turns.
        let fresh = ChatSettings::default();
        assert!(fresh.tools_enabled);
        assert!(fresh.use_skills);
        assert_eq!(fresh.history_limit, 24);

        // The same must hold for an empty object, which is what a config with a
        // partial `chat` block deserializes to.
        let parsed: ChatSettings = serde_json::from_str("{}").expect("empty chat block");
        assert!(parsed.tools_enabled);
        assert!(parsed.use_skills);
        assert_eq!(parsed.history_limit, 24);
    }

    #[test]
    fn an_old_window_block_is_ignored_and_dropped_on_save() {
        // `window.always_on_top` is gone: a topmost shell broke Alt+Tab, so the
        // feature was removed rather than defaulted off. A config written by an
        // older build still carries the key, and that must load clean and come
        // back out of the next save without it.
        let legacy: Settings = serde_json::from_str(
            "{\"theme\":\"dark\",\"window\":{\"always_on_top\":true}}",
        )
        .expect("legacy config with a window block");
        assert_eq!(legacy.theme, Theme::Dark);

        let json = serde_json::to_string(&legacy).expect("serialize");
        assert!(
            !json.contains("always_on_top"),
            "the removed setting must not survive a save: {json}"
        );
    }

    #[test]
    fn a_provider_can_serve_several_models() {
        let mut provider = Provider {
            id: "p1".to_string(),
            name: "Router".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            model: "deepseek-v3".to_string(),
            models: vec![
                "deepseek-v3".to_string(),
                "claude-sonnet".to_string(),
                "gpt-4o".to_string(),
            ],
            ..Provider::default()
        };
        assert_eq!(provider.model_list().len(), 3);
        assert!(provider.is_ready());

        // Saving keeps the default at the front; normalizing must not reorder or
        // duplicate what the form already produced.
        assert!(!provider.normalize(), "a consistent provider was rewritten");
        assert_eq!(provider.models[0], "deepseek-v3");

        // An older provider carries only `model`. It must still offer one entry
        // in the picker rather than an empty list.
        let legacy = Provider {
            model: "gpt-4o-mini".to_string(),
            base_url: "https://api.example.com/v1".to_string(),
            ..Provider::default()
        };
        assert_eq!(legacy.model_list(), vec!["gpt-4o-mini".to_string()]);
        assert!(legacy.is_ready());
    }

    #[test]
    fn normalizing_a_provider_keeps_model_and_models_in_step() {
        // A provider whose model is missing from the list gains it, at the front:
        // it is the one the user last chose as the default.
        let mut provider = Provider {
            model: "gpt-4o".to_string(),
            models: vec!["claude-sonnet".to_string()],
            base_url: "https://api.example.com/v1".to_string(),
            ..Provider::default()
        };
        assert!(provider.normalize());
        assert_eq!(provider.models, vec!["gpt-4o".to_string(), "claude-sonnet".to_string()]);

        // A provider whose list has entries but whose `model` is blank adopts the
        // first entry, so a request never goes out with an empty model.
        let mut blank = Provider {
            model: "   ".to_string(),
            models: vec!["  gpt-4o  ".to_string(), String::new()],
            base_url: "https://api.example.com/v1".to_string(),
            ..Provider::default()
        };
        assert!(blank.normalize());
        assert_eq!(blank.model, "gpt-4o");
        assert_eq!(blank.models, vec!["gpt-4o".to_string()]);
        assert!(blank.is_ready());

        // A provider with nothing to offer stays not-ready instead of inventing a
        // model name.
        let mut empty = Provider {
            base_url: "https://api.example.com/v1".to_string(),
            ..Provider::default()
        };
        assert!(!empty.normalize());
        assert!(empty.model_list().is_empty());
        assert!(!empty.is_ready());
    }

    #[test]
    fn a_zero_history_limit_is_repaired_but_a_real_choice_is_not() {
        // The fingerprint of the bad default: all three fields at once.
        let mut broken = Settings::default();
        broken.chat.history_limit = 0;
        broken.chat.tools_enabled = false;
        broken.chat.use_skills = false;
        assert!(broken.normalize(), "the repair was not reported");
        assert_eq!(broken.chat.history_limit, 24);
        assert!(broken.chat.tools_enabled);
        assert!(broken.chat.use_skills);

        // Someone who turned tools off on purpose keeps that choice: the
        // history limit is what distinguishes the two states.
        let mut chosen = Settings::default();
        chosen.chat.tools_enabled = false;
        assert!(!chosen.normalize(), "a healthy block was reported as repaired");
        assert_eq!(chosen.chat.history_limit, 24);
        assert!(!chosen.chat.tools_enabled, "an explicit choice was overwritten");
    }

    #[test]
    fn custom_beats_global_and_none_beats_both() {
        let mut settings = Settings::default();
        settings.global_proxy.mode = GlobalProxyMode::Custom;
        settings.global_proxy.custom = Some(proxy("9.9.9.9", 1111));

        assert_eq!(
            resolve(&settings, &app("a", ProxyMode::Inherit, None), 0),
            Resolved::Use(proxy("9.9.9.9", 1111))
        );
        // None ignores a configured global proxy: it is the escape hatch.
        assert_eq!(
            resolve(&settings, &app("a", ProxyMode::None, None), 0),
            Resolved::Direct
        );
        assert_eq!(
            resolve(
                &settings,
                &app("a", ProxyMode::Custom, Some(proxy("1.2.3.4", 2222))),
                0
            ),
            Resolved::Use(proxy("1.2.3.4", 2222))
        );
    }

    #[test]
    fn a_half_filled_custom_proxy_means_direct() {
        // An empty host is an unfinished form, not a request to dial nowhere.
        let settings = Settings::default();
        let empty = Proxy {
            scheme: Some("http".to_string()),
            host: "   ".to_string(),
            port: 8080,
            username: None,
            password: None,
        };
        assert_eq!(
            resolve(&settings, &app("a", ProxyMode::Custom, Some(empty)), 0),
            Resolved::Direct
        );
    }

    #[test]
    fn sticky_pool_keeps_an_app_on_one_exit() {
        let settings = settings_with_pool(PoolStrategy::Sticky);
        let a = app("app-mvixi8yk", ProxyMode::Inherit, None);
        let first = resolve(&settings, &a, 0);
        // Reordering the sidebar changes the index; sticky must not care.
        assert_eq!(first, resolve(&settings, &a, 7));
        assert!(matches!(first, Resolved::Use(_)));
    }

    #[test]
    fn sticky_pool_does_not_pin_every_app_to_one_member() {
        let settings = settings_with_pool(PoolStrategy::Sticky);
        let picks: std::collections::HashSet<String> = (0..16)
            .filter_map(|i| match resolve(
                &settings,
                &app(&format!("app-{i}"), ProxyMode::Inherit, None),
                i,
            ) {
                Resolved::Use(p) => Some(p.to_uri()),
                _ => None,
            })
            .collect();
        assert!(picks.len() > 1, "every app landed on the same proxy");
    }

    #[test]
    fn round_robin_follows_the_app_index() {
        let settings = settings_with_pool(PoolStrategy::RoundRobin);
        let a = app("a", ProxyMode::Inherit, None);
        assert_eq!(
            resolve(&settings, &a, 0),
            Resolved::Use(proxy("10.0.0.1", 8001))
        );
        assert_eq!(
            resolve(&settings, &a, 1),
            Resolved::Use(proxy("10.0.0.2", 8002))
        );
        assert_eq!(
            resolve(&settings, &a, 2),
            Resolved::Use(proxy("10.0.0.1", 8001))
        );
    }

    #[test]
    fn a_missing_pool_is_reported_not_silently_direct() {
        // Falling back to direct would send tunnelled traffic in the clear.
        let mut settings = Settings::default();
        settings.global_proxy.mode = GlobalProxyMode::Pool;
        settings.global_proxy.pool_id = Some("gone".to_string());
        assert!(matches!(
            resolve(&settings, &app("a", ProxyMode::Inherit, None), 0),
            Resolved::PoolUnavailable(_)
        ));
    }

    #[test]
    fn an_empty_or_unusable_pool_is_unavailable() {
        let mut settings = Settings::default();
        settings.global_proxy.mode = GlobalProxyMode::Pool;
        settings.global_proxy.pool_id = Some("p1".to_string());
        settings.pools.push(ProxyPool {
            id: "p1".to_string(),
            name: "Empty".to_string(),
            proxies: vec![Proxy::default()],
            strategy: PoolStrategy::Sticky,
        });
        assert!(matches!(
            resolve(&settings, &app("a", ProxyMode::Inherit, None), 0),
            Resolved::PoolUnavailable(_)
        ));
    }

    #[test]
    fn unusable_pool_members_are_skipped() {
        let mut settings = Settings::default();
        settings.global_proxy.mode = GlobalProxyMode::Pool;
        settings.global_proxy.pool_id = Some("p1".to_string());
        settings.pools.push(ProxyPool {
            id: "p1".to_string(),
            name: "Mixed".to_string(),
            proxies: vec![Proxy::default(), proxy("10.0.0.9", 9009)],
            strategy: PoolStrategy::Sticky,
        });
        assert_eq!(
            resolve(&settings, &app("a", ProxyMode::Inherit, None), 0),
            Resolved::Use(proxy("10.0.0.9", 9009))
        );
    }

    #[test]
    fn pool_uri_parsing_accepts_both_documented_shapes() {
        let with_auth = Proxy::parse("socks5://bob:s3cret@1.2.3.4:1080").unwrap();
        assert_eq!(with_auth.host, "1.2.3.4");
        assert_eq!(with_auth.port, 1080);
        assert_eq!(with_auth.username.as_deref(), Some("bob"));
        assert_eq!(with_auth.password.as_deref(), Some("s3cret"));
        assert_eq!(with_auth.normalized_scheme(), "socks5");

        let plain = Proxy::parse("http://5.6.7.8:3128").unwrap();
        assert_eq!(plain.host, "5.6.7.8");
        assert_eq!(plain.port, 3128);
        assert!(plain.username.is_none());
    }

    #[test]
    fn pool_uri_parsing_names_what_is_wrong() {
        // These strings go straight into the pool editor, so each has to say
        // what the actual problem is.
        assert!(Proxy::parse("").unwrap_err().contains("Enter a proxy"));
        assert!(Proxy::parse("ftp://1.2.3.4:21").unwrap_err().contains("scheme"));
        assert!(Proxy::parse("http://1.2.3.4").unwrap_err().contains("no port"));
        assert!(
            Proxy::parse("http://1.2.3.4:99999")
                .unwrap_err()
                .contains("valid port")
        );
        assert!(Proxy::parse("http://:8080").unwrap_err().contains("no host"));
    }

    #[test]
    fn pool_uri_parsing_survives_a_pasted_path_and_an_at_sign_password() {
        // Both happen when copying from a provider dashboard.
        let p = Proxy::parse("http://u:p@ss@1.2.3.4:8080/").unwrap();
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 8080);
        // The LAST '@' separates credentials from host, so a password with '@'
        // survives intact.
        assert_eq!(p.username.as_deref(), Some("u"));
        assert_eq!(p.password.as_deref(), Some("p@ss"));
    }

    #[test]
    fn a_bare_host_defaults_to_http() {
        assert_eq!(
            Proxy::parse("10.1.1.1:8080").unwrap().normalized_scheme(),
            "http"
        );
    }

    #[test]
    fn provider_headers_let_a_user_override_a_preset() {
        let provider = Provider {
            id: "p".to_string(),
            name: "P".to_string(),
            prefix: "p".to_string(),
            kind: ApiKind::ChatCompletions,
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-secret-value".to_string(),
            model: "gpt-4o".to_string(),
            models: Vec::new(),
            agent: AgentPreset::Opencode,
            input_price: 0.0,
            output_price: 0.0,
            headers: vec![
                Header {
                    name: "User-Agent".to_string(),
                    value: "mine/2.0".to_string(),
                },
                Header {
                    name: "x-extra".to_string(),
                    value: "1".to_string(),
                },
            ],
        };
        let resolved = provider.resolved_headers();
        let agents: Vec<&String> = resolved
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
            .map(|(_, v)| v)
            .collect();
        // Exactly one, and it is the user's: two would be a malformed request.
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0], "mine/2.0");
        // The preset's other headers survive.
        assert!(resolved.iter().any(|(k, _)| k == "x-opencode-client"));
        assert!(resolved.iter().any(|(k, v)| k == "x-extra" && v == "1"));
    }

    #[test]
    fn provider_keys_are_never_returned_in_full() {
        let mut provider = Provider {
            api_key: "sk-abcdefgh1234".to_string(),
            ..Provider::default()
        };
        assert_eq!(provider.masked(), "****1234");
        provider.api_key = "abc".to_string();
        assert_eq!(provider.masked(), "****");
        provider.api_key = String::new();
        assert_eq!(provider.masked(), "");
    }

    #[test]
    fn every_agent_preset_is_distinct_and_explained() {
        for preset in [
            AgentPreset::Plain,
            AgentPreset::Opencode,
            AgentPreset::OhMyPi,
            AgentPreset::ClaudeCode,
            AgentPreset::Codex,
        ] {
            assert!(!preset.label().is_empty());
            assert!(!preset.hint().is_empty());
        }
        // Plain must send nothing, or choosing it would be a lie.
        assert!(AgentPreset::Plain.headers().is_empty());
        // oh-my-pi must not impersonate opencode, or a provider that tells them
        // apart would misroute the request.
        let omp = AgentPreset::OhMyPi.headers();
        assert!(omp.iter().all(|(k, _)| !k.starts_with("x-opencode")));
        assert!(omp.iter().any(|(k, v)| k == "x-initiator" && v == "agent"));
    }

    #[test]
    fn api_kinds_map_to_their_real_paths() {
        assert_eq!(ApiKind::ChatCompletions.path(), "/chat/completions");
        assert_eq!(ApiKind::Anthropic.path(), "/messages");
        assert_eq!(ApiKind::Responses.path(), "/responses");
    }

    #[test]
    fn proxy_scheme_is_normalized_for_the_engines() {
        // WebView2 and wry only parse http and socks5; anything else has to be
        // mapped or the webview is built with no proxy at all.
        let mut p = proxy("10.0.0.1", 8080);
        assert_eq!(p.normalized_scheme(), "http");
        p.scheme = Some("https".to_string());
        assert_eq!(p.normalized_scheme(), "http");
        p.scheme = Some("SOCKS5".to_string());
        assert_eq!(p.normalized_scheme(), "socks5");
        p.scheme = Some("garbage".to_string());
        assert_eq!(p.normalized_scheme(), "http");
    }

    #[test]
    fn a_masked_key_never_overwrites_the_stored_one() {
        // The page only ever sees `****1234`. Saving the form without touching
        // the key field must not replace a working credential with the mask.
        let mut stored = Settings::default();
        stored.providers.push(Provider {
            id: "p1".to_string(),
            api_key: "sk-real-key-9999".to_string(),
            ..Provider::default()
        });

        let mut incoming = stored.clone();
        incoming.providers[0].api_key = "****9999".to_string();
        merge_secrets(&mut incoming, &stored);
        assert_eq!(incoming.providers[0].api_key, "sk-real-key-9999");

        // An empty field means the same thing: leave the credential alone.
        let mut incoming = stored.clone();
        incoming.providers[0].api_key = String::new();
        merge_secrets(&mut incoming, &stored);
        assert_eq!(incoming.providers[0].api_key, "sk-real-key-9999");

        // A genuinely new key is written through untouched.
        let mut incoming = stored.clone();
        incoming.providers[0].api_key = "sk-brand-new-1111".to_string();
        merge_secrets(&mut incoming, &stored);
        assert_eq!(incoming.providers[0].api_key, "sk-brand-new-1111");
    }

    #[test]
    fn redaction_leaves_nothing_secret_in_the_blob_handed_to_the_ui() {
        let mut settings = Settings::default();
        settings.providers.push(Provider {
            id: "p1".to_string(),
            api_key: "sk-super-secret-abcdef".to_string(),
            ..Provider::default()
        });
        let view = redacted(&settings);
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("sk-super-secret-abcdef"));
        assert!(json.contains("****cdef"));
    }

    #[test]
    fn endpoint_is_not_doubled_when_the_base_url_already_ends_in_the_path() {
        // Providers hand out full endpoint URLs; appending the path again would
        // produce .../chat/completions/chat/completions and a 404.
        let p = Provider {
            base_url: "https://api.openai.com/v1".to_string(),
            kind: ApiKind::ChatCompletions,
            ..Provider::default()
        };
        assert_eq!(p.endpoint(), "https://api.openai.com/v1/chat/completions");

        let full = Provider {
            base_url: "https://api.openai.com/v1/chat/completions".to_string(),
            kind: ApiKind::ChatCompletions,
            ..Provider::default()
        };
        assert_eq!(full.endpoint(), "https://api.openai.com/v1/chat/completions");

        // A trailing slash is a typo, not a path segment.
        let slash = Provider {
            base_url: "https://api.anthropic.com/v1/".to_string(),
            kind: ApiKind::Anthropic,
            ..Provider::default()
        };
        assert_eq!(slash.endpoint(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn anthropic_authenticates_with_a_key_header_not_a_bearer_token() {
        // Sending `Authorization: Bearer` to Anthropic fails; it wants x-api-key
        // plus a version header.
        let p = Provider {
            api_key: "sk-ant-1".to_string(),
            kind: ApiKind::Anthropic,
            ..Provider::default()
        };
        let headers = p.auth_headers();
        assert!(headers.iter().any(|(k, _)| k == "x-api-key"));
        assert!(headers.iter().any(|(k, _)| k == "anthropic-version"));
        assert!(!headers.iter().any(|(k, _)| k == "authorization"));

        let openai = Provider {
            api_key: "sk-1".to_string(),
            kind: ApiKind::ChatCompletions,
            ..Provider::default()
        };
        assert!(
            openai
                .auth_headers()
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer sk-1")
        );
    }

    #[test]
    fn a_custom_header_still_beats_the_credential() {
        // Deliberate override, e.g. a gateway that wants a different auth header.
        let p = Provider {
            api_key: "sk-1".to_string(),
            headers: vec![Header {
                name: "Authorization".to_string(),
                value: "Bearer gateway-token".to_string(),
            }],
            ..Provider::default()
        };
        let headers = p.request_headers();
        let auth: Vec<&(String, String)> = headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .collect();
        assert_eq!(auth.len(), 1, "the same header must appear exactly once");
        assert_eq!(auth[0].1, "Bearer gateway-token");
    }

    #[test]
    fn a_loopback_bridge_needs_one_guide_and_a_published_one_needs_three() {
        assert_eq!(mcp_guides(McpBind::Loopback).len(), 1);
        assert_eq!(mcp_guides(McpBind::Network).len(), 3);
        // Every published variant has to include localhost: a client on this
        // machine still uses it.
        assert!(mcp_guides(McpBind::Network).contains(&"localhost"));
    }
}
