# Screenshots the real UI in each state worth reviewing.
#
# Works the same way scripts/probe.ps1 does: copies ui/index.html next to itself,
# injects a Tauri stub plus scripts/shot-states.js, then loads it in headless
# Chrome and captures a PNG. The copy is removed afterwards.
#
# Usage: pwsh -File scripts/shots.ps1 [-States catalog,templated,selfhosted,proxy]
param(
  [string[]]$States = @('catalog', 'tabbar', 'templated', 'selfhosted', 'proxy', 'router', 'combo', 'logs'),
  [string]$UiDir = "$PSScriptRoot\..\ui",
  [string]$OutDir = "$PSScriptRoot\..\shots",
  [int]$Width = 1440,
  [int]$Height = 900
)

$ErrorActionPreference = 'Stop'

# `pwsh -File` passes arguments as plain strings, so "-States a,b" arrives as one
# element rather than an array. Split it so both call styles work.
$States = $States -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_ }

$chrome = @(
  'C:\Program Files\Google\Chrome\Application\chrome.exe',
  'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe',
  'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
  'C:\Program Files\Microsoft\Edge\Application\msedge.exe'
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) { throw 'No Chrome/Edge found.' }

$src = Join-Path $UiDir 'index.html'
$driver = Join-Path $PSScriptRoot 'shot-states.js'
New-Item -ItemType Directory -Path $OutDir -Force | Out-Null

# The stub only needs to satisfy the boot sequence: the states captured here are
# all reachable without a real backend.
$stub = @'
<script>
// Fixtures for the settings surfaces. Kept next to the stub because they only
// exist to make the panes render something real-shaped in a screenshot.
window.__shotSettings = {
  global_proxy: { mode: 'none', custom: { scheme: 'http', host: '', port: 0, username: null, password: null }, pool_id: null },
  pools: [],
  providers: [
    { id: 'prov-1', name: 'Risun', prefix: 'risun', kind: 'chat-completions',
      base_url: 'https://api.risun.test/v1', api_key: '****8f21',
      model: 'bansos/deepseek-v4.1-flash-1037',
      models: ['bansos/deepseek-v4.1-flash-1037', 'bansos/kimi-k2-thinking', 'bansos/glm-4.6'],
      agent: 'plain', headers: [], input_price: 0.28, output_price: 1.1 },
    { id: 'prov-2', name: 'Anthropic', prefix: 'anthropic', kind: 'anthropic',
      base_url: 'https://api.anthropic.com', api_key: '****3d9c',
      model: 'claude-sonnet-4-5', models: ['claude-sonnet-4-5', 'claude-haiku-4-5'],
      agent: 'claude-code', headers: [], input_price: 3, output_price: 15 }
  ],
  mcp: { port: 7420, bind: 'loopback', scope: 'all-tabs', allow_app_edits: false },
  skills: [],
  theme: 'dark',
  chat: { provider_id: 'prov-1', model: '', tools_enabled: true, use_skills: true, history_limit: 24 },
  provider_rows: [],
  env: { hostname: 'workstation', lan_addresses: ['192.168.1.42'], public_address: null, platform: 'windows' },
  mcp_enabled: true,
  router: {
    enabled: true, port: 11930, publish: true, agent: 'opencode', tailscale: false,
    tunnel: { mode: 'quick', token: '', hostname: '' },
    keys: [
      { id: 'key-1', name: 'Laptop', key: '****c0de', enabled: true, models: ['risun/*', 'combo/*'], created_at: 1 },
      { id: 'key-2', name: 'Phone', key: '****77aa', enabled: true, models: [], created_at: 2 },
      { id: 'key-3', name: 'Retired CI', key: '****beef', enabled: false, models: [], created_at: 3 }
    ],
    log: { enabled: true, limit: 100 }
  },
  combos: [
    { id: 'combo-1', name: 'deepseek-v4.1-flash-1037', label: 'DeepSeek flash, two vendors',
      members: ['risun/bansos/deepseek-v4.1-flash-1037', 'anthropic/claude-haiku-4-5'],
      strategy: 'fallback', judge: '', enabled: true },
    { id: 'combo-2', name: 'quality-first', label: 'Judge picks the better answer',
      members: ['risun/bansos/deepseek-v4.1-flash-1037', 'anthropic/claude-sonnet-4-5'],
      strategy: 'fusion', judge: 'anthropic/claude-sonnet-4-5', enabled: true }
  ]
};
window.__shotSettings.provider_rows = window.__shotSettings.providers.map((p) => ({
  id: p.id, name: p.name, prefix: p.prefix, model: p.model, models: p.models,
  kind: p.kind === 'anthropic' ? 'Anthropic Messages' : 'Chat Completions',
  agent: p.agent, ready: true, key_masked: p.api_key
}));
window.__shotView = () => JSON.parse(JSON.stringify(window.__shotSettings));

window.__shotRouter = () => {
  const r = window.__shotSettings.router;
  return {
    running: true, port: r.port, configuredPort: r.port, publish: r.publish, tailscale: r.tailscale,
    localEndpoint: 'http://127.0.0.1:' + r.port + '/v1',
    networkEndpoint: r.publish ? 'http://192.168.1.42:' + r.port + '/v1' : null,
    tailnetEndpoint: null,
    tunnel: { running: true, url: 'https://dusk-harbor-rm7.trycloudflare.com', endpoint: 'https://dusk-harbor-rm7.trycloudflare.com/v1', error: '' },
    tailscaleState: { installed: false, address: '', hostname: '', enabled: false, error: '' },
    cloudflared: 'C:\\x\\cloudflared.exe',
    cloudflaredAvailable: true,
    logFile: 'C:\\x\\logs\\ai-requests.jsonl',
    routerLog: 'C:\\x\\logs\\ai-router.log',
    logBytes: 40960, logEnabled: true, logLimit: 100,
    keyCount: r.keys.length, enabledKeys: 2,
    models: 5, combos: 2
  };
};
// The pane reads `view.router_live`, which get_settings carries — router_state
// only refreshes it after a start/stop. Seeding both keeps the shot honest.
window.__shotSettings.router_live = window.__shotRouter();
// `published` is what the reach strip counts models from.
window.__shotSettings.published = [
  { id: 'risun/bansos/deepseek-v4.1-flash-1037', source: 'provider', provider: 'Risun', upstream: 'bansos/deepseek-v4.1-flash-1037' },
  { id: 'risun/bansos/kimi-k2-thinking', source: 'provider', provider: 'Risun', upstream: 'bansos/kimi-k2-thinking' },
  { id: 'risun/bansos/glm-4.6', source: 'provider', provider: 'Risun', upstream: 'bansos/glm-4.6' },
  { id: 'anthropic/', source: 'provider', provider: 'Anthropic', upstream: '' },
  { id: 'combo/deepseek-v4.1-flash-1037', source: 'combo', provider: '', upstream: '' },
  { id: 'combo/quality-first', source: 'combo', provider: '', upstream: '' }
];

const __shotNow = Date.now();
window.__shotLogs = {
  entries: [
    { at: __shotNow - 4000, endpoint: 'chat/completions', model: 'combo/deepseek-v4.1-flash-1037',
      upstream: 'bansos/deepseek-v4.1-flash-1037', provider: 'Risun', status: 200, ms: 1840,
      input_tokens: 12480, output_tokens: 2130, cost: 0.0058, key: 'Laptop', stream: true, error: '',
      ttft_ms: 410, tps: 149.0, estimated: false },
    { at: __shotNow - 42000, endpoint: 'messages', model: 'anthropic/claude-sonnet-4-5',
      upstream: 'claude-sonnet-4-5', provider: 'Anthropic', status: 200, ms: 3120,
      input_tokens: 8200, output_tokens: 1450, cost: 0.0464, key: 'Laptop', stream: true, error: '',
      ttft_ms: 980, tps: 67.8, estimated: false },
    { at: __shotNow - 96000, endpoint: 'responses', model: 'risun/bansos/kimi-k2-thinking',
      upstream: 'bansos/kimi-k2-thinking', provider: 'Risun', status: 200, ms: 5400,
      input_tokens: 6100, output_tokens: 3800, cost: 0.0059, key: 'Phone', stream: false, error: '',
      ttft_ms: 0, tps: 0, estimated: false },
    { at: __shotNow - 180000, endpoint: 'chat/completions', model: 'risun/bansos/glm-4.6',
      upstream: 'bansos/glm-4.6', provider: 'Risun', status: 429, ms: 210,
      input_tokens: 0, output_tokens: 0, cost: 0, key: 'Laptop', stream: false,
      error: 'upstream rate limit: too many requests', ttft_ms: 0, tps: 0, estimated: false },
    { at: __shotNow - 300000, endpoint: 'chat/completions', model: 'combo/quality-first',
      upstream: 'claude-sonnet-4-5', provider: 'Anthropic', status: 200, ms: 6900,
      input_tokens: 9400, output_tokens: 2600, cost: 0.0672, key: 'Laptop', stream: true, error: '',
      ttft_ms: 1240, tps: 45.9, estimated: false }
  ],
  // One request being served right now, so the shot shows the live row too.
  live: [
    { id: 1, at: __shotNow - 2500, endpoint: 'chat/completions', model: 'risun/bansos/deepseek-v4.1-flash-1037',
      provider: 'Risun', key: 'Laptop', stream: true, elapsed_ms: 2500, ttft_ms: 380,
      output_tokens: 612, tps: 288.7 }
  ],
  stats: {
    requests: 5, failed: 1, input_tokens: 36180, output_tokens: 9980, cost: 0.1253, avg_ms: 3494,
    avg_ttft_ms: 876, avg_tps: 74.3, window_hours: 24,
    models: [
      { name: 'combo/deepseek-v4.1-flash-1037', requests: 1, input_tokens: 12480, output_tokens: 2130, cost: 0.0058, failed: 0 },
      { name: 'anthropic/claude-sonnet-4-5', requests: 1, input_tokens: 8200, output_tokens: 1450, cost: 0.0464, failed: 0 },
      { name: 'risun/bansos/kimi-k2-thinking', requests: 1, input_tokens: 6100, output_tokens: 3800, cost: 0.0059, failed: 0 },
      { name: 'combo/quality-first', requests: 1, input_tokens: 9400, output_tokens: 2600, cost: 0.0672, failed: 0 },
      { name: 'risun/bansos/glm-4.6', requests: 1, input_tokens: 0, output_tokens: 0, cost: 0, failed: 1 }
    ],
    series: Array.from({ length: 24 }, (_, i) => ({
      hours_ago: 23 - i,
      requests: [0,0,0,0,1,0,0,0,0,2,0,0,1,0,0,0,0,0,1,0,0,0,1,1][i],
      tokens: [0,0,0,0,1500,0,0,0,0,4200,0,0,2600,0,0,0,0,0,6100,0,0,0,3800,12480][i]
    })),
    endpoints: [
      { name: 'chat/completions', requests: 3 },
      { name: 'messages', requests: 1 },
      { name: 'responses', requests: 1 }
    ]
  },
  limit: 100, enabled: true, bytes: 40960, file: 'C:\\x\\logs\\ai-requests.jsonl',
  stats_file: 'C:\\x\\logs\\ai-stats.sqlite'
};

// The sidebar state. Half these icons are near-black glyphs on a transparent
// field, which is exactly the case the resting socket exists for: without a
// container they read as gaps in the rail rather than as closed apps.
window.__shotApps = [
  { id: 'app-wa', name: 'WhatsApp', url: 'https://web.whatsapp.com/', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Ccircle cx=%2216%22 cy=%2216%22 r=%2215%22 fill=%22%2325D366%22/%3E%3C/svg%3E' },
  { id: 'app-gm', name: 'Gmail', url: 'https://mail.google.com/', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Crect width=%2232%22 height=%2232%22 rx=%226%22 fill=%22%23EA4335%22/%3E%3C/svg%3E' },
  { id: 'app-vault', name: 'Vault', url: 'https://vault.example.com/', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Crect x=%224%22 y=%224%22 width=%2224%22 height=%2224%22 rx=%225%22 fill=%22%230B0F14%22/%3E%3C/svg%3E' },
  { id: 'app-discord', name: 'Discord', url: 'https://discord.com/app', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Cellipse cx=%2216%22 cy=%2216%22 rx=%2214%22 ry=%2211%22 fill=%22%23131A24%22/%3E%3C/svg%3E' },
  { id: 'app-infer', name: 'Inferhub', url: 'https://inferhub.dev/', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Cpath d=%22M8 6l16 20M24 6L8 26%22 stroke=%22%230A0A0A%22 stroke-width=%223%22/%3E%3C/svg%3E' },
  { id: 'app-mail', name: 'Mail', url: 'https://mail.example.com/', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Cpath d=%22M5 9h22v14H5z%22 fill=%22%23101820%22/%3E%3C/svg%3E' },
  { id: 'app-slack', name: 'Slack', url: 'https://app.slack.com/client', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Ccircle cx=%2216%22 cy=%2216%22 r=%2213%22 fill=%22%23611F69%22/%3E%3C/svg%3E' },
  { id: 'app-gamma', name: 'Gamma', url: 'https://gamma.app/', engine: 'os-webview',
    profile_mode: 'light', enabled: true, muted: false, notifications: true,
    icon: 'data:image/svg+xml,%3Csvg xmlns=%22http://www.w3.org/2000/svg%22 viewBox=%220 0 32 32%22%3E%3Cpath d=%22M16 4l11 24H5z%22 fill=%22%23F8FAFC%22/%3E%3C/svg%3E' }
];

window.__TAURI__ = {
  core: { invoke: async (cmd, args) => {
    switch (cmd) {
      // Only the states that are about the rail itself get apps: the others
      // capture a surface whose layout would change if the sidebar were full.
      case 'list_apps': {
        const st = new URLSearchParams(location.search).get('state');
        return (st === 'sidebar' || st === 'tabbar') ? window.__shotApps : [];
      }
      case 'detect_browsers': return [
        { name: 'chrome', path: 'C:\\x\\chrome.exe', family: 'chromium', source: '' },
        { name: 'edge', path: 'C:\\x\\msedge.exe', family: 'chromium', source: '' },
        // Two Camoufox installs, as really happens when a profile manager
        // vendors its own copy next to the standalone one.
        { name: 'camoufox', path: 'C:\\x\\a\\camoufox.exe', family: 'camoufox', source: 'camoufox' },
        { name: 'camoufox', path: 'C:\\x\\b\\camoufox.exe', family: 'camoufox', source: 'DonutBrowser' },
        { name: 'cloakbrowser', path: 'C:\\x\\c\\chrome.exe', family: 'chromium', source: 'cloakbrowser' }
      ];
      case 'platform_info': return { light_profiles: true, named_data_stores: true, tab_mute: true };
      // get_catalog_state returns a tuple, so JSON sees [favorites, usage].
      // An object here silently yields no favorites at all.
      case 'get_catalog_state': return [['https://kiwiirc.com/nextclient/'], { 'https://meet.jit.si/': 4, 'https://web.whatsapp.com': 2 }];
      case 'save_apps': return null;
      case 'fetch_favicon': throw new Error('offline');
      // Settings surfaces (AI Router / AI Combo / Request logs) read everything
      // from these three commands. The shapes mirror what the backend returns, so
      // the shots show the real layout rather than an empty state.
      case 'get_settings': return window.__shotView();
      case 'router_state': return window.__shotRouter();
      case 'router_logs': return window.__shotLogs;
      case 'network_facts': return { hostname: 'workstation', lan_addresses: ['192.168.1.42'], public_address: null };
      case 'data_paths': return { root: 'C:\\x', config: 'C:\\x\\config.json', profiles: 'C:\\x\\profiles' };
      case 'mcp_state': return {
        enabled: true, port: 7420, configuredPort: 7420, bind: 'loopback', scope: 'all-tabs',
        allowAppEdits: false, guides: [], endpointFile: 'C:\\x\\mcp-endpoint.json',
        logFile: 'C:\\x\\mcp.log', shimPath: 'C:\\x\\rustbox-mcp.exe'
      };
      case 'hide_all_tabs': return null;
      case 'resize_tabs': return null;
      default: return null;
    }
  } },
  event: { listen: async () => () => {} }
};
</script>
'@

$html = [System.IO.File]::ReadAllText($src, [System.Text.Encoding]::UTF8)
$probe = [System.IO.File]::ReadAllText($driver, [System.Text.Encoding]::UTF8)
$html = $html -replace '(?s)(<head>)', "`$1`n$stub"
$html = $html -replace '(?s)(</body>)', "<script>`n$probe`n</script>`n`$1"

$work = Join-Path $env:TEMP ('rb-shots-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$page = Join-Path $UiDir '__shot.html'

try {
  [System.IO.File]::WriteAllText($page, $html, (New-Object System.Text.UTF8Encoding($false)))
  $base = 'file:///' + $page.Replace('\', '/')

  foreach ($state in $States) {
    $out = Join-Path $OutDir "$state.png"
    Remove-Item $out -Force -ErrorAction SilentlyContinue
    $args = @(
      '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
      '--allow-file-access-from-files', '--hide-scrollbars',
      "--window-size=$Width,$Height",
      "--user-data-dir=$work\p-$state",
      '--virtual-time-budget=20000',
      "--screenshot=$out",
      "$base`?state=$state"
    )
    $p = Start-Process -FilePath $chrome -ArgumentList $args -NoNewWindow -PassThru `
      -RedirectStandardError "$work\err-$state.txt"
    $p.WaitForExit(120000) | Out-Null
    if (-not (Test-Path $out)) {
      $err = Get-Content "$work\err-$state.txt" -Raw -ErrorAction SilentlyContinue
      throw "no screenshot for '$state'. stderr: $err"
    }
    $size = (Get-Item $out).Length
    '{0,-12} -> {1} ({2:N0} bytes)' -f $state, $out, $size
  }
}
finally {
  Remove-Item $page -Force -ErrorAction SilentlyContinue
  Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
