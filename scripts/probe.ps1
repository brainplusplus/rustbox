# Probes the real UI headlessly: copies ui/index.html, injects a stub for the
# Tauri globals plus scripts/probe-catalog.js, runs it in headless Chrome, and
# prints the PASS/FAIL rows the probe wrote into <pre id="probe">.
#
# The copy is made next to the original so `catalog.js` and `icons/` resolve with
# no rewriting, and it is deleted afterwards (including on failure).
param(
  [string]$UiDir = "$PSScriptRoot\..\ui",
  [string]$ProbeScript = "$PSScriptRoot\probe-catalog.js",
  [int]$TimeoutSec = 180,
  # JSON array returned by the stubbed `list_apps`. Defaults to an empty
  # workspace, which is what the catalog probe wants; startup and reorder probes
  # need real entries to act on.
  [string]$ListAppsJson = '[]',
  # Seeds the remembered-tab preference before the UI boots, so the "reopen the
  # tab you had last" path can be exercised. Empty means a clean first launch.
  [string]$LastActiveId = '',
  # Tells the probe the workspace has nothing openable, so it should assert the
  # welcome screen instead of an opened tab.
  [switch]$ExpectWelcome
)

$ErrorActionPreference = 'Stop'

$chrome = @(
  'C:\Program Files\Google\Chrome\Application\chrome.exe',
  'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe',
  'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
  'C:\Program Files\Microsoft\Edge\Application\msedge.exe'
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) { throw 'No Chrome/Edge found.' }

$src = Join-Path $UiDir 'index.html'
if (-not (Test-Path $src)) { throw "missing $src" }
if (-not (Test-Path $ProbeScript)) { throw "missing $ProbeScript" }

$page = Join-Path $UiDir '__probe.html'
$work = Join-Path $env:TEMP ('rb-probe-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$outFile = Join-Path $work 'dom.html'

try {
  # The probe has to run AFTER the app's own scripts define their globals, so it
  # is appended at the end of <body>. The stub for window.__TAURI__ must exist
  # BEFORE them, so it is inserted as the first thing in <head>.
  $stub = @'
<script>
try { localStorage.setItem('rustbox.lastActiveId', '__PROBE_LAST__'); } catch (_) {}
// Uncaught errors are collected rather than left to the console: headless Chrome
// has no console anyone reads, so an exception in the page otherwise shows up
// only as "probe did not finish", which names no cause at all. The list is
// mirrored into the DOM so the runner can print it after a failure.
window.__probeErrors = [];
window.__probeNoteError = (text) => {
  window.__probeErrors.push(text);
  // Mirrored into the DOM: the runner reads the dumped document, not this
  // variable, so an error only survives a failed run if it lands in an element.
  const host = document.getElementById('probe-errors');
  if (host) host.textContent = JSON.stringify(window.__probeErrors);
};
window.addEventListener('error', (e) => {
  window.__probeNoteError(
    `${e.message || String(e)} @ ${e.filename || '?'}:${e.lineno || 0}:${e.colno || 0}`
  );
}, true);
window.addEventListener('unhandledrejection', (e) => {
  const r = e.reason;
  window.__probeNoteError(`unhandled rejection: ${(r && r.message) || String(r)}`);
});
// Read back by the probe instead of localStorage: activate() persists the tab
// it opens, so by the time the probe looks, localStorage can no longer
// distinguish "seeded" from "written during startup".
window.__probeSeed = '__PROBE_LAST__';
window.__probeExpectWelcome = __PROBE_WELCOME__;

// The settings page is draft-based: it adopts whatever get_settings returns and
// sends the whole block back. This builds a view shaped like the real one, from
// the last block the page saved, so a save round trip is exercised end to end.
window.__probeSettings = {
  global_proxy: {
    mode: 'custom',
    custom: { scheme: 'http', host: '10.9.8.7', port: 3128, username: 'probe', password: 'secret' },
    pool_id: null
  },
  pools: [{
    id: 'pool-1', name: 'Residential', strategy: 'sticky',
    proxies: [
      { scheme: 'http', host: '10.0.0.1', port: 8080, username: null, password: null },
      { scheme: 'socks5', host: '10.0.0.2', port: 1080, username: null, password: null }
    ]
  }],
  providers: [{
    id: 'prov-1', name: 'Probe Cloud', prefix: 'probe-cloud', kind: 'chat-completions',
    base_url: 'https://api.probe.test/v1', api_key: '****1234', model: 'probe-large',
    models: ['probe-large', 'probe-small', 'probe-vision'],
    agent: 'plain', headers: [], input_price: 1.5, output_price: 4.5
  }],
  mcp: { port: 7420, bind: 'loopback', scope: 'all-tabs', allow_app_edits: false },
  skills: [{
    id: 'skill-1', name: 'Be brief', description: 'Answers in two sentences.',
    instructions: 'Answer in at most two sentences.', enabled: true, source: 'manual', url: null
  }],
  theme: 'dark',
  chat: { provider_id: 'prov-1', model: '', tools_enabled: true, use_skills: true, history_limit: 24 },
  provider_rows: [{
    id: 'prov-1', name: 'Probe Cloud', model: 'probe-large',
    models: ['probe-large', 'probe-small', 'probe-vision'], kind: 'Chat Completions',
    agent: 'No agent headers', ready: true, key_masked: '****1234'
  }],
  env: { hostname: 'probe', lan_addresses: ['192.168.1.42'], public_address: null, platform: 'windows' },
  mcp_enabled: false,
  router: {
    enabled: false, port: 11930, publish: false, agent: 'plain', tailscale: false,
    tunnel: { mode: 'quick', token: '', hostname: '' },
    keys: [
      { id: 'key-1', name: 'Laptop', key: '****c0de', enabled: true, models: ['probe-cloud/*'], created_at: 1 },
      { id: 'key-2', name: 'Retired CI', key: '****beef', enabled: false, models: [], created_at: 2 }
    ],
    log: { enabled: true, limit: 100 }
  },
  combos: [{
    id: 'combo-1', name: 'fast', label: 'Fast fallback',
    members: ['probe-cloud/probe-large', 'probe-cloud/probe-small'],
    strategy: 'fallback', judge: '', enabled: true
  }]
};

// The router's live state is the backend's, not the page's: it is re-read after
// every start/stop, and the endpoint strip is built from it. Deriving it here
// from the saved block keeps a start from being invisible to the probe.
window.__probeRouterLive = () => {
  const r = window.__probeSettings.router || {};
  const on = !!window.__probeRouterOn;
  const port = r.port || 11930;
  const published = window.__probePublished();
  return {
    running: on,
    port: on ? port : 0,
    configuredPort: port,
    publish: !!r.publish,
    tailscale: !!r.tailscale,
    localEndpoint: `http://127.0.0.1:${port}/v1`,
    networkEndpoint: r.publish ? `http://192.168.1.42:${port}/v1` : null,
    tailnetEndpoint: r.tailscale ? `http://100.64.0.9:${port}/v1` : null,
    tunnel: window.__probeTunnel || { running: false, url: '', endpoint: '', error: '' },
    tailscaleState: { installed: true, address: '100.64.0.9', hostname: 'probe.tailnet.ts.net', enabled: !!r.tailscale, error: '' },
    cloudflaredAvailable: true,
    logFile: 'C:\\x\\logs\\ai-requests.jsonl',
    routerLog: 'C:\\x\\logs\\ai-router.log',
    logBytes: 2048,
    logEnabled: !r.log || r.log.enabled !== false,
    logLimit: (r.log && r.log.limit) || 100,
    keyCount: (r.keys || []).length,
    enabledKeys: (r.keys || []).filter((k) => k.enabled !== false).length,
    models: published.filter((p) => p.source === 'provider').length,
    combos: published.filter((p) => p.source === 'combo').length
  };
};

// What the router publishes, derived the way the backend derives it: every model
// on every provider under the provider's prefix, plus every enabled combo.
window.__probePublished = () => {
  const rows = [];
  for (const p of window.__probeSettings.providers || []) {
    const prefix = String(p.prefix || p.name || 'provider').toLowerCase().replace(/[^a-z0-9_.]+/g, '-');
    for (const m of (p.models && p.models.length ? p.models : (p.model ? [p.model] : []))) {
      rows.push({ id: `${prefix}/${m}`, prefix, upstream: m, source: 'provider', prefix_count: 1 });
    }
  }
  for (const c of window.__probeSettings.combos || []) {
    if (c.enabled === false) continue;
    rows.push({ id: `combo/${c.name}`, prefix: 'combo', upstream: c.name, source: 'combo', prefix_count: 1 });
  }
  return rows;
};

// One row per served request. Deliberately two failures and a streamed call, so
// the dashboard has something to be wrong about.
window.__probeLogs = {
  entries: [
    { at: Date.now() - 4000, endpoint: 'chat/completions', model: 'probe-cloud/probe-large',
      upstream: 'probe-large', provider: 'Probe Cloud', status: 200, ms: 940,
      input_tokens: 1200, output_tokens: 400, cost: 0.0042, key: 'Laptop', stream: false, error: '',
      ttft_ms: 0, tps: 425.5, estimated: false },
    { at: Date.now() - 90000, endpoint: 'messages', model: 'combo/fast',
      upstream: 'probe-large', provider: 'Probe Cloud', status: 200, ms: 1500,
      input_tokens: 800, output_tokens: 250, cost: 0.0021, key: 'Laptop', stream: true, error: '',
      ttft_ms: 320, tps: 212.1, estimated: true },
    { at: Date.now() - 600000, endpoint: 'responses', model: 'probe-cloud/probe-small',
      upstream: 'probe-small', provider: 'Probe Cloud', status: 429, ms: 120,
      input_tokens: 0, output_tokens: 0, cost: 0, key: '', stream: false,
      ttft_ms: 0, tps: 0, estimated: false,
      error: 'upstream rate limit: too many requests' }
  ],
  // A request that has not finished: the row exists, its numbers are still
  // moving, and it is the reason the pane polls at all.
  live: [
    { id: 7, at: Date.now() - 2500, endpoint: 'chat/completions', model: 'combo/fast',
      provider: 'Probe Cloud', key: 'Laptop', stream: true, elapsed_ms: 2500,
      ttft_ms: 410, output_tokens: 96, tps: 45.9 }
  ],
  stats: {
    requests: 3, failed: 1, input_tokens: 2000, output_tokens: 650, cost: 0.0063, avg_ms: 853,
    avg_ttft_ms: 320, avg_tps: 188.4, window_hours: 24,
    models: [
      { name: 'probe-cloud/probe-large', requests: 2, input_tokens: 2000, output_tokens: 650, cost: 0.0063, failed: 0 },
      { name: 'probe-cloud/probe-small', requests: 1, input_tokens: 0, output_tokens: 0, cost: 0, failed: 1 }
    ],
    series: Array.from({ length: 24 }, (_, i) => ({
      hours_ago: 23 - i, requests: i === 23 ? 2 : (i === 22 ? 1 : 0), tokens: i === 23 ? 2500 : (i === 22 ? 150 : 0)
    })),
    endpoints: [{ name: 'chat/completions', requests: 1 }]
  },
  limit: 100, enabled: true, bytes: 2048, file: 'C:\\x\\logs\\ai-requests.jsonl',
  stats_file: 'C:\\x\\logs\\ai-stats.sqlite'
};

// The backend owns this list (`settings::agent_presets`) and re-sends it on
// every reply, including after a save, so it lives outside the saved block. The
// hints are deliberately not the real strings: the page has a fallback copy for
// the window before the first reply, and a probe that could not tell the two
// apart would not notice the fallback winning.
window.__probeAgentPresets = [
  { id: 'plain', label: 'No agent headers', hint: 'stub: plain' },
  { id: 'opencode', label: 'opencode', hint: 'stub: opencode' },
  { id: 'oh-my-pi', label: 'oh-my-pi', hint: 'stub: oh-my-pi' },
  { id: 'claude-code', label: '[CC]', hint: 'stub: claude-code' },
  { id: 'codex', label: 'Codex CLI', hint: 'stub: codex' }
];
// Derived the way the backend derives it: one masked row per provider, so the
// chat picker can be asserted against a real shape rather than a fixture that
// only matches before the first save.
window.__probeSettingsView = () => {
  const view = JSON.parse(JSON.stringify(window.__probeSettings));
  view.provider_rows = (view.providers || []).map((p) => ({
    id: p.id,
    name: p.name,
    model: p.model,
    // Mirrors `Provider::model_list`: the list when populated, else the single
    // model. The chat picker reads this, so a stub that only sent `model` would
    // hide a real regression in the fallback.
    models: (p.models && p.models.length) ? p.models : (p.model ? [p.model] : []),
    kind: p.kind,
    agent: p.agent,
    ready: !!(p.base_url && (p.model || (p.models && p.models.length))),
    key_masked: /^\*+/.test(p.api_key || '') ? p.api_key : (p.api_key ? '****' + String(p.api_key).slice(-4) : ''),
  }));
  view.agent_presets = JSON.parse(JSON.stringify(window.__probeAgentPresets));
  view.mcp_enabled = !!window.__probeMcpOn;
  // Three keys the router and combo panes cannot render without. They are the
  // backend's own derived lists, not part of the saved block, so a page that
  // read them out of `settings` instead would render empty and the probe would
  // catch it here.
  view.router_live = window.__probeRouterLive();
  view.published = window.__probePublished();
  view.combo_strategies = [
    { id: 'fallback', label: 'Fallback', hint: 'stub: try each in order' },
    { id: 'round-robin', label: 'Round-robin', hint: 'stub: rotate' },
    { id: 'fusion', label: 'Fusion', hint: 'stub: ask all, judge merges' }
  ];
  return view;
};

// Every reply is deep-cloned on the way out, the way a real IPC boundary would
// serialize it. Handing back a live reference lets the page mutate the stub's
// own state, which turns a duplicate transcript into a passing test.
const clone = (v) => JSON.parse(JSON.stringify(v));

// The clipboard is a stub because a headless probe has none it can trust. It is
// installed unconditionally: Chrome does expose `navigator.clipboard` on a
// file:// page, but writing to the real one needs a focused document and a
// permission grant, so the call would silently fail and the probe would be
// asserting on an empty list. Recording the write is the only way to assert what
// a Copy button actually put on the clipboard.
window.__probeClipboard = [];
Object.defineProperty(navigator, 'clipboard', {
  configurable: true,
  value: {
    writeText: async (text) => {
      // A knob the probe flips to reproduce WebView2's real failure: the write is
      // rejected when the document is not focused. Without it the native fallback
      // is unreachable in a headless run, because the stub always resolves.
      if (window.__probeClipboardDeny) {
        throw new DOMException('Document is not focused.', 'NotAllowedError');
      }
      window.__probeClipboard.push(text);
    },
    readText: async () => window.__probeClipboard[window.__probeClipboard.length - 1] || ''
  }
});

window.__TAURI__ = {
  core: { invoke: async (cmd, args) => {
    switch (cmd) {
      case 'list_apps': return __PROBE_APPS__;
      case 'detect_browsers': return [
        { name: 'chrome', path: 'C:\\x\\chrome.exe', family: 'chromium', source: '' },
        // Two Camoufox installs side by side, as really happens when a profile
        // manager vendors its own copy.
        { name: 'camoufox', path: 'C:\\x\\a\\camoufox.exe', family: 'camoufox', source: 'camoufox' },
        { name: 'camoufox', path: 'C:\\x\\b\\camoufox.exe', family: 'camoufox', source: 'DonutBrowser' }
      ];
      case 'platform_info': return { light_profiles: true, named_data_stores: true, tab_mute: true };
      // Tuple, not an object: the real command returns (favorites, usage).
      case 'get_catalog_state': return [[], {}];
      // The settings surfaces read these on open. A stub that returned null
      // would make every pane render an error state and hide the bug the probe
      // is actually looking for.
      case 'get_settings': return window.__probeSettingsView();
      // save_settings echoes the view back, exactly like the real command, so a
      // page that adopts the response is exercised rather than skipped.
      case 'save_settings': {
        (window.__probeSaves = window.__probeSaves || []).push(JSON.parse(JSON.stringify(args.incoming)));
        window.__probeSettings = JSON.parse(JSON.stringify(args.incoming));
        return window.__probeSettingsView();
      }
      case 'test_proxy': return { ok: true, exit_ip: '203.0.113.7', ms: 42 };
      case 'test_provider': return { ok: true, ms: 88 };
      // Starting and stopping the router is one round trip that answers with the
      // live state, so the page must adopt that answer rather than assume the
      // listener came up. The stub records the calls so the probe can tell a
      // toggle that reached the backend from one that only repainted.
      case 'set_router_enabled': {
        window.__probeRouterOn = !!args.enabled;
        (window.__probeRouterCalls = window.__probeRouterCalls || []).push(args.enabled);
        return { enabled: !!args.enabled, port: args.enabled ? 11930 : 0 };
      }
      case 'set_router_tunnel': {
        window.__probeTunnel = args.enabled
          ? { running: true, url: 'https://calm-river-1234.trycloudflare.com', endpoint: 'https://calm-river-1234.trycloudflare.com/v1', error: '' }
          : { running: false, url: '', endpoint: '', error: '' };
        (window.__probeTunnelCalls = window.__probeTunnelCalls || []).push(args.enabled);
        return window.__probeTunnel;
      }
      case 'set_router_tailscale': {
        window.__probeSettings.router.tailscale = !!args.enabled;
        (window.__probeTailscaleCalls = window.__probeTailscaleCalls || []).push(args.enabled);
        return { installed: true, address: '100.64.0.9', hostname: 'probe.tailnet.ts.net', enabled: !!args.enabled, error: '' };
      }
      case 'router_logs': return clone(window.__probeLogs);
      // The narrow poll: only what is in flight. Answers from the same stub, so
      // a probe that sees the live row and a probe that sees it disappear both
      // exercise the real path the page uses.
      case 'router_live': return clone(window.__probeLogs.live || []);
      // The native clipboard path. It is the fallback the page uses when WebView2
      // rejects `navigator.clipboard` because the document is not focused, so the
      // probe records it separately: a write arriving here proves the fallback
      // ran, which is the whole point of the test that denies the browser API.
      case 'copy_text':
        (window.__probeNativeClipboard = window.__probeNativeClipboard || []).push(args.text);
        return null;
      case 'clear_router_logs': {
        // Clearing the list must not touch the aggregates: they live in SQLite,
        // and the probe keeps them to prove the page reads them separately.
        window.__probeLogs = {
          ...window.__probeLogs,
          entries: [],
          limit: 100, enabled: true, bytes: 0, file: 'C:\\\\x\\\\logs\\\\ai-requests.jsonl'
        };
        return null;
      }
      case 'new_router_key': return 'sk-rbx-probe0000';
      case 'router_state': return window.__probeRouterLive();
      case 'check_prefix': {
        const taken = (window.__probeSettings.providers || [])
          .filter((p) => p.id !== args.providerId)
          .map((p) => String(p.prefix || p.name || '').toLowerCase());
        if (!args.prefix) return null;
        if (args.prefix === 'combo') return '“combo” is reserved for combos.';
        if (taken.includes(args.prefix)) return 'Another provider already uses this prefix.';
        return null;
      }
      case 'network_facts': return { hostname: 'probe', lan_addresses: ['192.168.1.42'], public_address: '198.51.100.9' };
      case 'set_mcp_enabled': {
        window.__probeMcpOn = !!args.enabled;
        (window.__probeMcpCalls = window.__probeMcpCalls || []).push(args.enabled);
        return window.__probeMcpOn ? { enabled: true, port: 7420 } : { enabled: false, port: 0 };
      }
      case 'mcp_state': return {
        enabled: !!window.__probeMcpOn,
        port: window.__probeMcpOn ? 7420 : 0,
        configuredPort: 7420, bind: 'loopback', scope: 'all-tabs',
        allowAppEdits: false, guides: [],
        endpointFile: 'C:\\\\x\\\\mcp-endpoint.json',
        logFile: 'C:\\\\x\\\\mcp.log',
        shimPath: 'C:\\\\x\\\\rustbox-mcp.exe'
      };
      case 'data_paths': return {
        root: 'C:\\\\x', config: 'C:\\\\x\\\\config.json', profiles: 'C:\\\\x\\\\profiles'
      };
      case 'hide_all_tabs': (window.__probeCalls = window.__probeCalls || []).push('hide_all_tabs'); return null;
      case 'resize_tabs': (window.__probeCalls = window.__probeCalls || []).push('resize_tabs');
        (window.__probeRects = window.__probeRects || []).push([args.x, args.y, args.width, args.height]);
        return null;
      case 'activate_app': (window.__probeCalls = window.__probeCalls || []).push('activate_app'); return null;
      case 'set_active_tab': window.__probeActiveTab = args.appId; return null;
      // Every chat reply is deep-cloned on the way out. A real command crosses
      // IPC and gets serialized, so the page never shares a live reference with
      // the backend; handing back the same object lets the page's optimistic
      // turn mutate the stub's transcript and makes a duplicate look real.
      case 'chat_list': return clone({ conversations: window.__probeConversations || [] });
      case 'chat_new': {
        const conversation = {
          id: 'conv-probe', title: 'New conversation', created_at: 1, updated_at: 1,
          provider_id: args.providerId || null, model: args.model || '', messages: []
        };
        window.__probeConversation = conversation;
        (window.__probeConversations = window.__probeConversations || []).unshift({
          id: conversation.id, title: conversation.title, updated_at: 1, message_count: 0
        });
        return clone(conversation);
      }
      case 'chat_get': return window.__probeConversation ? clone(window.__probeConversation) : null;
      case 'chat_update': {
        const conversation = window.__probeConversation || {};
        conversation.provider_id = args.providerId;
        conversation.model = args.model || '';
        window.__probeConversation = conversation;
        return clone(conversation);
      }
      case 'chat_send': {
        (window.__probeSent = window.__probeSent || []).push(args);
        const conversation = window.__probeConversation || { messages: [] };
        // The real command owns the transcript: it appends the user turn and the
        // reply to its own copy and returns the whole list. Anything the page
        // drew optimistically is replaced, not merged.
        conversation.messages = [
          {
            id: 'm-user', role: 'user', content: args.text,
            attachments: args.attachments || [], tool_calls: [], at: 2
          },
          {
            id: 'm-assistant', role: 'assistant',
            content: 'Probe reply: the page title is **Example**.',
            attachments: [],
            tool_calls: [{
              id: 'c1', name: 'read_page', arguments: { appId: 'app-a' },
              result: { title: 'Example' }, error: null
            }],
            at: 3
          }
        ];
        conversation.title = 'What is on this page?';
        window.__probeConversation = conversation;
        const list = window.__probeConversations || [];
        if (list[0]) { list[0].title = conversation.title; list[0].message_count = 2; }
        return clone({ reply: 'Probe reply', messages: conversation.messages });
      }
      case 'chat_rename': return window.__probeConversation ? clone(window.__probeConversation) : null;
      case 'chat_delete': return null;
      case 'save_apps': (window.__probeSaved = window.__probeSaved || []).push(JSON.parse(JSON.stringify(args.apps))); return null;
      case 'open_embedded_tab': (window.__probeOpened = window.__probeOpened || []).push(args.app.id); return null;
      // The tab strip is rebuilt from this. Returning the fixture lets a probe
      // assert the pinned-first rule and the drag wiring without a real webview.
      case 'list_sub_tabs': return window.__probeSubTabs || [];
      case 'reorder_sub_tab':
        (window.__probeReorders = window.__probeReorders || []).push({
          appId: args.appId, tabId: args.tabId, targetId: args.targetId, after: args.after,
        });
        return null;
      case 'fetch_favicon': throw new Error('offline');
      default: return null;
    }
  } },
  // A probe can push a backend event to the page by calling `window.__probeEmit`
  // (defined just below the stub), which is how a suite exercises a handler that
  // only reacts to an event. The sub-tab strip is fed this way. Listeners are
  // keyed by name and return an unlisten function, matching Tauri's contract.
  event: {
    listen: async (name, handler) => {
      (window.__probeListeners = window.__probeListeners || {})[name] =
        (window.__probeListeners[name] || []).concat([handler]);
      return () => {};
    },
  }
};
window.__probeEmit = (name, payload) => {
  for (const fn of (window.__probeListeners || {})[name] || []) {
    try { fn({ payload }); } catch (_) {}
  }
};
</script>
'@

  # Explicit UTF-8: this runs under Windows PowerShell 5.1 as often as not, and
  # its Get-Content falls back to the ANSI codepage for files without a BOM,
  # which turns every em dash in the UI into mojibake.
  $probe = [System.IO.File]::ReadAllText($ProbeScript, [System.Text.Encoding]::UTF8)
  $html = [System.IO.File]::ReadAllText($src, [System.Text.Encoding]::UTF8)

  # The stub is a literal here-string, so the apps JSON is substituted in after
  # the fact rather than interpolated (interpolating would also expand every
  # `$` in the stub).
  $stub = $stub.Replace('__PROBE_APPS__', $ListAppsJson)
  $stub = $stub.Replace('__PROBE_LAST__', $LastActiveId)
  $stub = $stub.Replace('__PROBE_WELCOME__', $(if ($ExpectWelcome) { 'true' } else { 'false' }))

  # Probe output sink, placed where the tab-error strip lives so it is in the DOM
  # from the start rather than appended mid-run. The second element mirrors the
  # page's uncaught errors so a run that dies before writing any rows still says
  # what killed it.
  $sink = '<pre id="probe" style="position:fixed;left:-9999px">pending</pre>' +
    '<pre id="probe-errors" style="position:fixed;left:-9999px">[]</pre>'

  # `-replace` treats the replacement as a substitution template, so a `$0` or
  # `$1` inside the probe's own source (a regex like /\$0\.0063/) would be
  # expanded as a backreference and silently corrupt the script. `.Replace()`
  # is a literal replace, which is what inserting source code needs.
  $html = $html.Replace('<head>', "<head>`n$stub")
  $html = $html.Replace('</body>', "$sink`n<script>`n$probe`n</script>`n</body>")

  [System.IO.File]::WriteAllText($page, $html, (New-Object System.Text.UTF8Encoding($false)))

  $url = 'file:///' + $page.Replace('\', '/')
  $args = @(
    '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
    '--allow-file-access-from-files', '--hide-scrollbars', '--window-size=1500,1000',
    "--user-data-dir=$work\profile",
    '--virtual-time-budget=40000',
    '--dump-dom', $url
  )

  $proc = Start-Process -FilePath $chrome -ArgumentList $args -NoNewWindow -PassThru `
    -RedirectStandardOutput $outFile -RedirectStandardError "$work\err.txt"
  if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
    $proc.Kill()
    throw "headless run timed out after ${TimeoutSec}s"
  }

  # Chrome children can still hold the file open for a moment.
  $dom = $null
  for ($i = 0; $i -lt 60; $i++) {
    try {
      $fs = [System.IO.File]::Open($outFile, 'Open', 'Read', 'ReadWrite')
      $sr = New-Object System.IO.StreamReader($fs)
      $dom = $sr.ReadToEnd()
      $sr.Close(); $fs.Close()
      break
    } catch { Start-Sleep -Milliseconds 150 }
  }
  if (-not $dom) { throw "could not read probe output ($outFile)" }

  $m = [regex]::Match($dom, '(?s)<pre id="probe"[^>]*>(.*?)</pre>')
  if (-not $m.Success) {
    $err = if (Test-Path "$work\err.txt") { Get-Content "$work\err.txt" -Raw } else { '' }
    throw "probe element missing (DOM $($dom.Length) bytes). stderr: $err"
  }
  $payload = $m.Groups[1].Value
  $pageErrors = ''
  $em = [regex]::Match($dom, '(?s)<pre id="probe-errors"[^>]*>(.*?)</pre>')
  if ($em.Success) {
    $pageErrors = $em.Groups[1].Value.Replace('&quot;', '"').Replace('&#39;', "'").
      Replace('&lt;', '<').Replace('&gt;', '>').Replace('&amp;', '&')
  }
  if ($payload -eq 'pending') {
    # "did not finish" names no cause on its own, so whatever the page threw is
    # printed alongside it: a syntax error, a rejected promise, a call into a
    # binding that does not exist. Without this the only symptom is silence.
    $err = if (Test-Path "$work\err.txt") { Get-Content "$work\err.txt" -Raw } else { '' }
    throw "probe did not finish. page errors: $pageErrors`nstderr: $err"
  }
  $payload = $payload.Replace('&lt;', '<').Replace('&gt;', '>').Replace('&quot;', '"').
    Replace('&#39;', "'").Replace('&amp;', '&')

  # "PASS|name|detail" per line. Parsed by hand rather than as JSON so a broken
  # row fails loudly instead of silently collapsing into one object.
  $rows = @($payload -split "`n" | Where-Object { $_ -match '^(PASS|FAIL)\|' } | ForEach-Object { $_.TrimEnd("`r") })
  if (-not $rows.Count) { throw "probe produced no rows: $payload" }

  $fail = 0
  foreach ($line in $rows) {
    $parts = $line -split '\|', 3
    $ok = $parts[0] -eq 'PASS'
    if (-not $ok) { $fail++ }
    $detail = if ($parts.Count -gt 2 -and $parts[2]) { "  [$($parts[2])]" } else { '' }
    '{0}  {1}{2}' -f $parts[0], $parts[1], $detail
  }
  if ($fail -gt 0 -and $pageErrors -and $pageErrors -ne '[]') {
    # Only on failure: an uncaught error is normal noise on some runs (the stub
    # refuses a favicon fetch on purpose), and printing it every time trains
    # people to ignore it.
    ''
    "page errors: $pageErrors"
  }
  ''
  '{0}/{1} checks passed' -f ($rows.Count - $fail), $rows.Count
  if ($fail -gt 0) { exit 1 }
  # Explicit success: without it the caller's $LASTEXITCODE keeps whatever the
  # last native command set, which makes a passing run look like a failure.
  exit 0
}
finally {
  Remove-Item $page -Force -ErrorAction SilentlyContinue
  Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
