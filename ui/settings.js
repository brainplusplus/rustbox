/* Settings page: six sections, one save.
 *
 * The page renders from a `draft` that is a clone of the backend's `Settings`
 * block. Inputs write into the draft by path, so nothing typed is lost when a
 * structural change (add a provider, switch proxy mode) forces a re-render, and
 * Save sends exactly one object.
 *
 * Three rules the code follows everywhere:
 *
 *   1. A secret never round-trips. Provider keys arrive masked, and a masked key
 *      is sent back untouched so the backend keeps the stored one.
 *   2. Nothing is claimed without being shown. The proxy test reports the exit
 *      address it actually used; the MCP guides are generated from the bind mode
 *      the bridge is really on; a pool with an unparsable line refuses to save
 *      instead of silently keeping the parsable ones.
 *   3. The rail states the current value of every section. The page should answer
 *      "what is this app doing" before a single click.
 */
'use strict';
(function () {
  const { invoke } = window.__TAURI__.core;

  const esc = (s) => String(s ?? '').replace(/[&<>"']/g, c => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));

  // The agent-header presets come from the backend (`agent_presets` on the
  // settings view), not from a list here. A picker that offers a preset the
  // backend does not implement would be a choice that silently sends nothing,
  // and two copies of the same list drift the moment one is edited.
  //
  // This is the fallback for the window between page load and the first
  // `get_settings` reply, and for a view that predates the field.
  const AGENTS_FALLBACK = [
    { id: 'plain', label: 'No agent headers', hint: 'Sends only the headers the provider requires.' },
    { id: 'opencode', label: 'opencode', hint: 'Sends x-opencode-client and x-opencode-project.' },
    { id: 'oh-my-pi', label: 'oh-my-pi', hint: 'Sends X-Initiator: agent.' },
    { id: 'claude-code', label: '[CC]', hint: 'Sends x-app: cli and the CLI user-agent.' },
    { id: 'codex', label: 'Codex CLI', hint: 'Sends originator: codex_cli_rs.' },
  ];

  /** The preset list currently in effect. */
  function agentPresets() {
    const fromView = state.view && state.view.agent_presets;
    return Array.isArray(fromView) && fromView.length ? fromView : AGENTS_FALLBACK;
  }

  /** The sentence explaining one preset, or an empty string. */
  function agentHint(id) {
    const found = agentPresets().find(a => a.id === id);
    return found ? found.hint || '' : '';
  }

  const KINDS = [
    { id: 'chat-completions', label: 'Chat Completions', path: '/chat/completions' },
    { id: 'anthropic', label: 'Anthropic Messages', path: '/messages' },
    { id: 'responses', label: 'Responses API', path: '/responses' },
  ];

  const TABS = [
    { id: 'general', label: 'General' },
    { id: 'proxy', label: 'Global proxy' },
    { id: 'pools', label: 'Proxy pool' },
    { id: 'ai', label: 'AI provider' },
    { id: 'router', label: 'AI Router' },
    { id: 'combo', label: 'AI Combo' },
    { id: 'logs', label: 'Request logs' },
    { id: 'mcp', label: 'MCP' },
    { id: 'skills', label: 'Skills' },
  ];

  const state = {
    open: false,
    tab: 'general',
    view: null,      // SettingsView from get_settings / save_settings
    mcp: null,       // mcp_state: live port, endpoint file, shim path
    draft: null,     // working copy, sent back on save
    facts: null,     // env facts, refilled when the public address is asked for
    dirty: false,
    probes: {},      // key -> {status:'busy'|'ok'|'fail', text, ms}
    editing: null,   // {kind:'provider'|'pool'|'skill'|'key'|'combo', index, draft?}
    logs: null,      // router_logs payload: entries, live, stats, limit, bytes, file
    logDetail: null, // index of the request-log row whose detail is open
    logLiveCount: 0, // how many requests were in flight on the last poll
  };

  const $ = (sel) => document.querySelector(sel);

  // ---- helpers -------------------------------------------------------------

  const clone = (v) => JSON.parse(JSON.stringify(v));

  function setPath(root, path, value) {
    const parts = path.split('.');
    let node = root;
    for (let i = 0; i < parts.length - 1; i += 1) {
      // Create the branch when it is missing rather than bailing: a config
      // written by an older build has no `startup` block, and returning here
      // would make the checkbox look live while silently changing nothing.
      if (node[parts[i]] == null || typeof node[parts[i]] !== 'object') node[parts[i]] = {};
      node = node[parts[i]];
    }
    node[parts[parts.length - 1]] = value;
  }

  function uid(prefix) {
    return `${prefix}-${Date.now().toString(36)}${Math.floor(Math.random() * 1e4).toString(36)}`;
  }

  /** A proxy object with every field present.
   *
   *  `Proxy` in Rust has no serde defaults, so a missing key is a deserialize
   *  error rather than an empty field. Building them all here is what keeps a
   *  half-filled form from failing to save with "missing field `username`". */
  function proxyObject(p) {
    return {
      scheme: (p && p.scheme) || 'http',
      host: (p && p.host) || '',
      port: Number((p && p.port) || 0),
      username: (p && p.username) || null,
      password: (p && p.password) || null,
    };
  }

  /** `scheme://[user:pass@]host:port`, the format a pool member is typed in. */
  function proxyUri(p) {
    if (!p || !p.host || !p.port) return '';
    const scheme = (p.scheme || 'http').toLowerCase() === 'socks5' ? 'socks5' : 'http';
    const auth = p.username ? `${p.username}${p.password ? `:${p.password}` : ''}@` : '';
    return `${scheme}://${auth}${p.host}:${p.port}`;
  }

  /** Parse one pool line, mirroring `Proxy::parse` so the messages match. */
  function parseProxyLine(raw) {
    const text = String(raw || '').trim();
    if (!text) return { error: 'the line is empty' };
    let scheme = 'http';
    let rest = text;
    const marker = text.indexOf('://');
    if (marker >= 0) {
      scheme = text.slice(0, marker).toLowerCase();
      rest = text.slice(marker + 3);
    }
    if (!['http', 'https', 'socks5', 'socks'].includes(scheme)) {
      return { error: `'${scheme}' is not a supported scheme (use http or socks5)` };
    }
    // Credentials end at the LAST '@', so a password containing one survives.
    let auth = null;
    let hostPort = rest;
    const at = rest.lastIndexOf('@');
    if (at >= 0) {
      auth = rest.slice(0, at);
      hostPort = rest.slice(at + 1);
    }
    let username = null;
    let password = null;
    if (auth !== null) {
      const colon = auth.indexOf(':');
      if (colon >= 0) {
        username = auth.slice(0, colon);
        password = auth.slice(colon + 1);
      } else {
        username = auth;
      }
    }
    // A pasted path or query is not part of the socket address.
    hostPort = String(hostPort.split(/[/?#]/)[0] || '').trim();
    const colon = hostPort.lastIndexOf(':');
    if (colon < 0) return { error: `'${hostPort}' has no port (use host:port)` };
    const host = hostPort.slice(0, colon).trim();
    const portText = hostPort.slice(colon + 1).trim();
    if (!host) return { error: 'the address has no host' };
    const port = Number(portText);
    if (!Number.isInteger(port) || port < 1 || port > 65535) {
      return { error: `'${portText}' is not a valid port (1-65535)` };
    }
    return { proxy: proxyObject({ scheme, host, port, username, password }) };
  }

  const providerName = (p) => (p && p.name) || 'Untitled provider';
  const poolName = (p) => (p && p.name) || 'Untitled pool';
  const usableCount = (pool) => (pool.proxies || []).filter(p => p.host && p.port).length;

  // ---- theme ---------------------------------------------------------------

  /* `system` follows the OS and keeps following it, so a user who flips their OS
     theme does not have to come back here. The remembered value is applied before
     the first fetch, which is what stops a light-theme launch from flashing
     dark. */
  const THEME_KEY = 'rustbox.theme';
  let mediaBound = false;

  function applyTheme(theme) {
    const root = document.documentElement;
    const chosen = theme || 'dark';
    root.dataset.themeSource = chosen;
    const media = window.matchMedia('(prefers-color-scheme: light)');
    const resolve = () => {
      const light = chosen === 'light' || (chosen === 'system' && media.matches);
      root.dataset.theme = light ? 'light' : 'dark';
    };
    resolve();
    if (!mediaBound) {
      mediaBound = true;
      media.addEventListener('change', () => {
        if (root.dataset.themeSource === 'system') resolve();
      });
    }
    try { localStorage.setItem(THEME_KEY, chosen); } catch (_) {}
  }

  window.RustBoxTheme = {
    apply: applyTheme,
    applyRemembered() {
      let remembered = 'dark';
      try { remembered = localStorage.getItem(THEME_KEY) || 'dark'; } catch (_) {}
      applyTheme(remembered);
    },
  };

  // ---- load / save ---------------------------------------------------------

  function adoptView(view) {
    state.view = view;
    state.facts = view.env;
    state.draft = clone({
      global_proxy: view.global_proxy,
      pools: view.pools,
      providers: view.providers,
      mcp: view.mcp,
      skills: view.skills,
      theme: view.theme,
      chat: view.chat,
      startup: view.startup,
      router: view.router,
      combos: view.combos || [],
    });
    state.dirty = false;
    applyTheme(view.theme);
  }

  async function reload() {
    const [view, mcp, logs] = await Promise.all([
      invoke('get_settings'),
      invoke('mcp_state').catch(() => null),
      invoke('router_logs').catch(() => null),
    ]);
    adoptView(view);
    state.mcp = mcp;
    state.logs = logs;
    state.logLiveCount = (logs && logs.live ? logs.live : []).length;
    render();
  }

  let saving = false;

  async function save() {
    if (!state.draft || saving) return;
    saving = true;
    const btn = $('#sp-save');
    if (btn) { btn.disabled = true; btn.textContent = 'Saving…'; }
    try {
      const view = await invoke('save_settings', { incoming: state.draft });
      adoptView(view);
      // The MCP listener may have moved to a new port, so the live state has to
      // be re-read rather than assumed.
      state.mcp = await invoke('mcp_state').catch(() => state.mcp);
      flash('Saved');
      render();
      return true;
    } catch (e) {
      flash(String(e?.message || e), true);
      return false;
    } finally {
      saving = false;
      const b = $('#sp-save');
      if (b) { b.disabled = false; b.textContent = 'Save changes'; }
    }
  }

  let flashTimer = null;
  function flash(text, isError) {
    const el = $('#sp-flash');
    if (!el) return;
    el.textContent = text;
    el.style.color = isError ? 'var(--danger)' : 'var(--accent)';
    el.classList.add('on');
    clearTimeout(flashTimer);
    flashTimer = setTimeout(() => el.classList.remove('on'), isError ? 7000 : 1800);
  }

  // ---- open / close --------------------------------------------------------

  async function open(tab) {
    if (tab) state.tab = tab;
    if (!state.open) {
      state.open = true;
      // Embedded tabs are separate OS windows stacked above the page, so the page
      // needs them hidden to be visible at all. Same rule the catalog follows.
      if (window.RustBoxUI) await window.RustBoxUI.hideTabs();
      $('#settings-page').classList.remove('hidden');
      announce(true);
    }
    try {
      await reload();
    } catch (e) {
      flash(String(e?.message || e), true);
    }
    syncLogPoll();
  }

  async function close() {
    if (!state.open) return;
    if (state.dirty) {
      const ok = await window.RustBoxUI.askConfirm({
        title: 'Discard changes?',
        text: 'The settings you edited have not been saved.',
        okLabel: 'Discard',
      });
      if (!ok) return;
      // Re-read so a discarded edit cannot come back on the next open.
      state.dirty = false;
    }
    state.open = false;
    state.editing = null;
    state.logDetail = null;
    stopLogPoll();
    $('#settings-page').classList.add('hidden');
    announce(false);
    if (window.RustBoxUI) window.RustBoxUI.restoreAfterOverlay();
  }

  /** Tell the shell whether this surface is up, so the sidebar button that opens
   *  it can show its own state without the shell having to poll. */
  function announce(open) {
    document.dispatchEvent(new CustomEvent('rustbox:overlay', {
      detail: { name: 'settings', open },
    }));
  }

  function setTab(tab) {
    state.tab = tab;
    state.editing = null;
    state.logDetail = null;
    render();
    syncLogPoll();
  }

  // ---- live request poll ---------------------------------------------------

  /** While the Request logs pane is open, ask the backend what is running.
   *
   *  Polling rather than an event: the backend has no reason to push on every
   *  token of every stream, and a two-second tick is enough for a display whose
   *  fastest-changing number is tokens per second. The poll only runs while that
   *  pane is visible and the settings page is open, so an idle app does no work.
   *
   *  It replaces the live block in place instead of re-rendering the pane, so an
   *  open detail panel and the scroll position survive every tick. */
  let logPoll = null;
  let logPollBusy = false;

  function stopLogPoll() {
    if (logPoll) clearInterval(logPoll);
    logPoll = null;
  }

  function syncLogPoll() {
    const wanted = state.open && state.tab === 'logs';
    if (!wanted) {
      stopLogPoll();
      return;
    }
    if (logPoll) return;
    logPoll = setInterval(pollLive, 2000);
  }

  async function pollLive() {
    if (logPollBusy || !state.open || state.tab !== 'logs') return;
    logPollBusy = true;
    try {
      const live = await invoke('router_live');
      const host = document.querySelector('#logs-live');
      // A pane re-rendered while the request was in flight has a fresh empty
      // host, so writing into it is still correct — but a pane that has gone
      // away must not be written into at all.
      if (host) {
        state.logs = { ...(state.logs || {}), live };
        host.innerHTML = liveRowsHtml();
        // A stream that just ended has to appear in the list above it. Detecting
        // that here — rather than polling the whole log on a timer — is what
        // keeps the pane quiet while nothing is happening.
        if (state.logLiveCount > live.length) refreshLogs();
        else syncLivePill(live.length);
        state.logLiveCount = live.length;
      }
    } catch (_) {
      // A poll that fails is not worth a flash: the next tick tries again, and a
      // stopped router is already visible in the rail.
    } finally {
      logPollBusy = false;
    }
  }

  /** The "N running" pill in the Requests header.
   *
   *  Outside the block the poll replaces, so it is updated on its own rather than
   *  by re-rendering the pane and losing the scroll position. */
  function syncLivePill(count) {
    const head = document.querySelector('#logs-clear');
    const row = head && head.closest('.sp-headrow');
    if (!row) return;
    let pill = row.querySelector('#logs-live-pill');
    if (!count) {
      if (pill) pill.remove();
      return;
    }
    if (!pill) {
      pill = document.createElement('span');
      pill.id = 'logs-live-pill';
      pill.className = 'sp-pill live';
      row.insertBefore(pill, row.querySelector('#logs-clear'));
    }
    pill.innerHTML = `<span class="lr-spin"></span>${count} running`;
  }

  /** Re-read the whole log, keeping the open detail row where it was. */
  async function refreshLogs() {
    try {
      state.logs = await invoke('router_logs');
      state.logLiveCount = (state.logs.live || []).length;
      render();
      syncLogPoll();
    } catch (e) {
      flash(String(e?.message || e), true);
    }
  }

  // ---- render --------------------------------------------------------------

  function render() {
    if (!state.open || !state.draft) return;
    renderRail();
    const pane = $('#sp-pane');
    const builders = {
      general: paneGeneral,
      proxy: paneProxy,
      pools: panePools,
      ai: paneProviders,
      router: paneRouter,
      combo: paneCombo,
      logs: paneLogs,
      mcp: paneMcp,
      skills: paneSkills,
    };
    const scroll = pane.scrollTop;
    pane.innerHTML = `<div class="sp-wrap">${(builders[state.tab] || paneGeneral)()}</div>`;
    pane.scrollTop = scroll;
    wirePane();
  }

  function railStat(id) {
    const d = state.draft;
    switch (id) {
      case 'general': return d.theme;
      case 'proxy': {
        const g = d.global_proxy;
        if (g.mode === 'none') return 'direct';
        if (g.mode === 'custom') {
          return g.custom && g.custom.host ? proxyUri(g.custom) : 'not set';
        }
        const pool = d.pools.find(p => p.id === g.pool_id);
        return pool ? `${usableCount(pool)} members` : 'no pool';
      }
      case 'pools':
        return d.pools.length ? String(d.pools.length) : 'none';
      case 'ai': {
        const ready = d.providers.filter(p => p.base_url && p.model).length;
        return ready ? `${ready} ready` : 'none';
      }
      case 'router': {
        const live = routerLive();
        if (!live.running) return 'off';
        return `:${live.port}`;
      }
      case 'combo':
        return (d.combos || []).length ? String((d.combos || []).length) : 'none';
      case 'logs': {
        const data = state.logs;
        if (!data || !data.stats) return '—';
        return `${data.stats.requests} req`;
      }
      case 'mcp': {
        if (!(state.view && state.view.mcp_enabled)) return 'off';
        const port = state.mcp && state.mcp.port;
        return port ? `:${port}` : 'on';
      }
      case 'skills': {
        if (!d.skills.length) return 'none';
        return `${d.skills.filter(s => s.enabled !== false).length}/${d.skills.length}`;
      }
      default: return '';
    }
  }

  function railTone(id) {
    const text = railStat(id);
    if (id === 'mcp') return state.view && state.view.mcp_enabled ? 'ok' : '';
    if (id === 'ai') return /ready/.test(text) ? 'ok' : 'warn';
    if (id === 'router') return routerLive().running ? 'ok' : '';
    if (id === 'combo') return text === 'none' ? '' : 'ok';
    if (id === 'logs') {
      const stats = state.logs && state.logs.stats;
      if (!stats || !stats.requests) return '';
      return stats.failed ? 'warn' : 'ok';
    }
    if (id === 'proxy') return /^(direct|not set|no pool)$/.test(text) ? '' : 'ok';
    return '';
  }

  function renderRail() {
    const rail = $('#sp-rail');
    rail.innerHTML = TABS.map(t => `
      <button class="sp-rail-item ${t.id === state.tab ? 'active' : ''}" data-tab="${t.id}">
        <span class="lbl">${esc(t.label)}</span>
        <span class="st ${railTone(t.id)}">${esc(railStat(t.id))}</span>
      </button>`).join('')
      + `<div class="sp-rail-sep"></div>
         <div class="sp-rail-note">Stored in config.json in the RustBox data folder. Proxy and provider changes apply to the next tab that opens; the MCP port and bind apply as soon as you save.</div>`;
    rail.querySelectorAll('[data-tab]').forEach(btn => {
      btn.addEventListener('click', () => setTab(btn.dataset.tab));
    });
  }

  // ---- shared pieces -------------------------------------------------------

  function probeBox(key) {
    const p = state.probes[key];
    if (!p) return `<div class="sp-probe" id="probe-${key}"></div>`;
    return probeHtml(key, p);
  }

  function probeHtml(key, p) {
    const cls = p.status === 'ok' ? 'ok' : p.status === 'fail' ? 'bad' : 'busy';
    const ms = p.ms != null ? `<span class="p-ms">${p.ms} ms</span>` : '';
    return `<div class="sp-probe ${cls}" id="probe-${key}"><span>${esc(p.text)}</span>${ms}</div>`;
  }

  /** Update one probe block in place.
   *
   *  Re-rendering the whole pane here would drop whatever the user was typing,
   *  so only the block changes. */
  function setProbe(key, value) {
    state.probes[key] = value;
    const host = $(`#probe-${key}`);
    if (!host) return;
    host.outerHTML = probeHtml(key, value);
  }

  function choice(name, value, current, title, desc, extraClass) {
    return `<label class="sp-choice ${extraClass || ''} ${value === current ? 'on' : ''}">
      <input type="radio" name="${name}" value="${value}" ${value === current ? 'checked' : ''}>
      <span class="cbody">
        <span class="cname">${esc(title)}</span>
        <span class="cdesc">${esc(desc)}</span>
      </span>
    </label>`;
  }

  // ---- pane: general -------------------------------------------------------

  function paneGeneral() {
    const d = state.draft;
    return `
      <h2 class="sp-title">General</h2>
      <p class="sp-lede">Appearance and where this app keeps its data.</p>

      <div class="sp-card">
        <h3>Theme</h3>
        <div class="sp-choices">
          ${choice('theme', 'dark', d.theme, 'Dark', 'The default. Low-glare, matched to the tab chrome.')}
          ${choice('theme', 'light', d.theme, 'Light', 'For daylight and projectors.')}
          ${choice('theme', 'system', d.theme, 'Follow the system', 'Switches with Windows, and keeps following it.')}
        </div>
      </div>

      <div class="sp-card">
        <h3>Data</h3>
        <p class="sp-note" id="sp-paths">Loading…</p>
      </div>

      <div class="sp-card">
        <h3>Startup</h3>
        <p class="sp-note">RustBox reopens the tab you had open last, and falls back to the first enabled app. Apps that launch an external browser are not opened automatically, because that starts a separate program you did not ask for yet.</p>
        <label class="sp-choice inline ${d.startup && d.startup.warm_start !== false ? 'on' : ''}">
          <input type="checkbox" data-path="startup.warm_start" ${!d.startup || d.startup.warm_start !== false ? 'checked' : ''}>
          <span class="cbody">
            <span class="cname">Load every app at startup</span>
            <span class="cdesc">Each enabled app opens in the background so it is already connected when you click it — new mail and messages arrive without a cold reload. Costs one webview per app. Takes effect on the next launch.</span>
          </span>
        </label>
      </div>`;
  }

  // ---- pane: global proxy --------------------------------------------------

  function paneProxy() {
    const g = state.draft.global_proxy;
    const custom = proxyObject(g.custom || {});
    const pools = state.draft.pools;

    const poolOptions = pools.length
      ? pools.map(p => `<option value="${esc(p.id)}" ${g.pool_id === p.id ? 'selected' : ''}>${esc(poolName(p))} — ${usableCount(p)} usable</option>`).join('')
      : '<option value="">No pools yet — add one on the Proxy pool tab</option>';

    return `
      <h2 class="sp-title">Global proxy</h2>
      <p class="sp-lede">What every app inherits unless it sets its own. An app's own setting always wins, and an app can opt out of the global proxy entirely.</p>

      <div class="sp-card">
        <div class="sp-choices">
          ${choice('gp-mode', 'none', g.mode, 'Direct connection', 'No proxy anywhere. Every tab leaves from this machine.')}
          ${choice('gp-mode', 'custom', g.mode, 'One proxy for everything', 'Every inheriting app goes through the same endpoint.')}
          ${choice('gp-mode', 'pool', g.mode, 'Proxy pool', 'Each app gets one member of a pool, so apps do not share an exit address.')}
        </div>
      </div>

      <div class="sp-card ${g.mode === 'custom' ? '' : 'hidden'}">
        <h3>Endpoint</h3>
        <div class="sp-row">
          <div class="sp-field" style="flex:0 0 128px">
            <label>Scheme</label>
            <select data-path="global_proxy.custom.scheme">
              <option value="http" ${custom.scheme === 'http' ? 'selected' : ''}>HTTP</option>
              <option value="socks5" ${custom.scheme === 'socks5' ? 'selected' : ''}>SOCKS5</option>
            </select>
          </div>
          <div class="sp-field">
            <label>Host</label>
            <input data-path="global_proxy.custom.host" value="${esc(custom.host)}" placeholder="127.0.0.1" spellcheck="false">
          </div>
          <div class="sp-field" style="flex:0 0 104px">
            <label>Port</label>
            <input data-path="global_proxy.custom.port" data-num type="number" value="${custom.port || ''}" placeholder="8080">
          </div>
        </div>
        <div class="sp-row">
          <div class="sp-field">
            <label>Username (optional)</label>
            <input data-path="global_proxy.custom.username" value="${esc(custom.username || '')}" autocomplete="off">
          </div>
          <div class="sp-field">
            <label>Password (optional)</label>
            <input data-path="global_proxy.custom.password" type="password" value="${esc(custom.password || '')}" autocomplete="off">
          </div>
        </div>
        <div class="sp-actions">
          <button class="sp-btn" id="test-global">Test connection</button>
          <span class="sp-spacer"></span>
          <span class="sp-note" style="margin:0">${custom.host ? esc(proxyUri(custom)) : 'No endpoint set yet.'}</span>
        </div>
        ${probeBox('global')}
        <p class="sp-note">The test relays a request through the proxy and reports the address it came out of, so a proxy that answers but forwards nothing is not mistaken for a working one.</p>
      </div>

      <div class="sp-card ${g.mode === 'pool' ? '' : 'hidden'}">
        <h3>Pool</h3>
        <div class="sp-field">
          <label>Which pool</label>
          <select data-path="global_proxy.pool_id">${poolOptions}</select>
        </div>
        <p class="sp-note" id="gp-pool-hint">${poolHint()}</p>
      </div>`;
  }

  function poolHint() {
    const g = state.draft.global_proxy;
    const pool = state.draft.pools.find(p => p.id === g.pool_id);
    if (!pool) return 'Pick a pool. Each inheriting app gets one member of it.';
    const usable = usableCount(pool);
    if (!usable) {
      return `"${poolName(pool)}" has no usable proxies, so every app inheriting it reports that the pool is unavailable instead of silently connecting directly.`;
    }
    return pool.strategy === 'round-robin'
      ? `${usable} usable members, handed out in sidebar order. Reordering the sidebar changes which app gets which.`
      : `${usable} usable members. Each app is hashed to one member, so reordering the sidebar never moves an app to a different exit.`;
  }

  // ---- pane: pools ---------------------------------------------------------

  function panePools() {
    const pools = state.draft.pools;
    const editing = state.editing && state.editing.kind === 'pool';

    const list = pools.length ? pools.map((p, i) => `
      <div class="sp-item ${editing && state.editing.index === i ? 'sel' : ''}">
        <div class="i-main">
          <div class="i-name">${esc(poolName(p))}</div>
          <div class="i-meta">${esc(p.strategy === 'round-robin' ? 'round robin' : 'sticky')} · ${usableCount(p)} usable of ${(p.proxies || []).length}</div>
        </div>
        <span class="sp-pill ${usableCount(p) ? 'ok' : 'warn'}">${usableCount(p) ? 'ready' : 'empty'}</span>
        <div class="i-acts">
          <button class="sp-btn sm ghost" data-pool-edit="${i}">Edit</button>
          <button class="sp-btn sm danger" data-pool-del="${i}">Delete</button>
        </div>
      </div>`).join('') : `<div class="sp-empty">No pools yet.<br>A pool is a named list of proxies; the global proxy can hand one member to each app.</div>`;

    const pool = editing
      ? (state.editing.index >= 0
        ? state.draft.pools[state.editing.index]
        : (state.editing.draft || (state.editing.draft = {
          id: uid('pool'), name: '', proxies: [], strategy: 'sticky',
        })))
      : null;

    const editor = pool ? `
      <div class="sp-card">
        <h3>${state.editing.index >= 0 ? 'Edit pool' : 'New pool'}</h3>
        <div class="sp-row">
          <div class="sp-field">
            <label>Name</label>
            <input id="pool-name" value="${esc(pool.name)}" placeholder="Residential US">
          </div>
          <div class="sp-field" style="flex:0 0 218px">
            <label>Assignment</label>
            <select id="pool-strategy">
              <option value="sticky" ${pool.strategy !== 'round-robin' ? 'selected' : ''}>Sticky — same app, same exit</option>
              <option value="round-robin" ${pool.strategy === 'round-robin' ? 'selected' : ''}>Round robin — by order</option>
            </select>
          </div>
        </div>
        <div class="sp-field">
          <label>Members — one per line</label>
          <textarea id="pool-members" spellcheck="false" placeholder="http://user:pass@1.2.3.4:8080&#10;socks5://5.6.7.8:1080">${esc((pool.proxies || []).map(proxyUri).join('\n'))}</textarea>
          <p class="sp-note">Format: <code>scheme://user:pass@host:port</code>. The credentials part is optional, a password may contain <code>@</code>, and a bare <code>host:port</code> is read as HTTP.</p>
        </div>
        <p class="sp-note" id="pool-error"></p>
        <div class="sp-actions">
          <button class="sp-btn primary" id="pool-save">${state.editing.index >= 0 ? 'Save pool' : 'Add pool'}</button>
          <button class="sp-btn ghost" id="pool-cancel">Cancel</button>
          <span class="sp-spacer"></span>
          <button class="sp-btn" id="pool-test">Test every member</button>
        </div>
        ${probeBox('pool')}
      </div>` : '';

    return `
      <h2 class="sp-title">Proxy pools</h2>
      <p class="sp-lede">A named group of proxies. Used by the global setting, or by one app that asks for a pool explicitly.</p>
      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Pools</h3>
          <button class="sp-btn primary" id="pool-new">New pool</button>
        </div>
        <div class="sp-list">${list}</div>
      </div>
      ${editor}`;
  }

  // ---- pane: providers -----------------------------------------------------

  function newProvider() {
    return {
      id: uid('prov'),
      name: '',
      prefix: '',
      kind: 'chat-completions',
      base_url: '',
      api_key: '',
      model: '',
      models: [],
      agent: 'plain',
      headers: [],
      input_price: 0,
      output_price: 0,
    };
  }

  /** The provider object the editor is editing.
   *
   *  A provider that has not been saved yet lives on the editor state, so that
   *  "add header" cannot create a half-filled entry in the list. */
  function editingProvider() {
    if (!state.editing || state.editing.kind !== 'provider') return null;
    if (state.editing.index >= 0) return state.draft.providers[state.editing.index];
    if (!state.editing.draft) state.editing.draft = newProvider();
    return state.editing.draft;
  }

  function paneProviders() {
    const providers = state.draft.providers;
    const editing = state.editing && state.editing.kind === 'provider';

    const list = providers.length ? providers.map((p, i) => `
      <div class="sp-item ${editing && state.editing.index === i ? 'sel' : ''}">
        <div class="i-main">
          <div class="i-name">${esc(providerName(p))}</div>
          <div class="i-meta">${esc(modelSummary(p))} · ${esc(p.base_url || 'no base URL')}${p.api_key ? ` · ${esc(p.api_key)}` : ''}</div>
        </div>
        <span class="sp-pill ${p.base_url && modelCount(p) ? 'ok' : 'warn'}">${p.base_url && modelCount(p) ? 'ready' : 'incomplete'}</span>
        <div class="i-acts">
          <button class="sp-btn sm ghost" data-prov-edit="${i}">Edit</button>
          <button class="sp-btn sm danger" data-prov-del="${i}">Delete</button>
        </div>
      </div>`).join('') : `<div class="sp-empty">No provider yet.<br>Add the endpoint the chat panel should talk to — any Chat Completions-, Anthropic- or Responses-compatible API.</div>`;

    const provider = editing ? editingProvider() : null;
    const editor = provider ? providerEditor(provider) : '';
    const chat = state.draft.chat;

    return `
      <h2 class="sp-title">AI provider</h2>
      <p class="sp-lede">The model endpoint the chat panel talks to. Keys live in config.json and are never sent back to this page in the clear.</p>
      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Providers</h3>
          <button class="sp-btn primary" id="prov-new">Add provider</button>
        </div>
        <div class="sp-list">${list}</div>
      </div>
      ${editor}

      <div class="sp-card">
        <h3>Chat defaults</h3>
        <div class="sp-field">
          <label>Provider for new conversations</label>
          <select data-path="chat.provider_id">
            <option value="">— none —</option>
            ${providers.map(p => `<option value="${esc(p.id)}" ${chat.provider_id === p.id ? 'selected' : ''}>${esc(providerName(p))}${p.model ? ` — ${esc(p.model)}` : ''}</option>`).join('')}
          </select>
        </div>
        <div class="sp-field">
          <label>Model override (optional)</label>
          <input data-path="chat.model" list="chat-model-options" value="${esc(chat.model || '')}" placeholder="Leave empty to use the provider's own default" spellcheck="false">
          <datalist id="chat-model-options">
            ${providers.flatMap(p => providerModels(p)).filter((m, i, all) => all.indexOf(m) === i)
              .map(m => `<option value="${esc(m)}"></option>`).join('')}
          </datalist>
          <p class="sp-note">Any model id the selected provider serves. The suggestions are every
            model you have configured; typing one that is not listed is allowed, because a router
            may accept ids it does not advertise.</p>
        </div>
        <div class="sp-row">
          <div class="sp-field">
            <label class="sp-choice inline ${chat.tools_enabled !== false ? 'on' : ''}">
              <input type="checkbox" data-path="chat.tools_enabled" ${chat.tools_enabled !== false ? 'checked' : ''}>
              <span class="cbody">
                <span class="cname">Let the agent use tools</span>
                <span class="cdesc">Read pages, run scripts, take screenshots.</span>
              </span>
            </label>
          </div>
          <div class="sp-field">
            <label class="sp-choice inline ${chat.use_skills !== false ? 'on' : ''}">
              <input type="checkbox" data-path="chat.use_skills" ${chat.use_skills !== false ? 'checked' : ''}>
              <span class="cbody">
                <span class="cname">Include enabled skills</span>
                <span class="cdesc">Adds them to the system prompt.</span>
              </span>
            </label>
          </div>
        </div>
        <div class="sp-field" style="max-width:230px">
          <label>Prompt history limit</label>
          <input data-path="chat.history_limit" data-num type="number" value="${chat.history_limit || 24}">
          <p class="sp-note">How many recent turns are sent. The first message is always kept, because it usually states what the conversation is about.</p>
        </div>
      </div>`;
  }

  function providerEditor(p) {
    const kind = KINDS.find(k => k.id === p.kind) || KINDS[0];
    const isNew = state.editing.index < 0;
    const masked = (p.api_key || '').startsWith('****');

    const headers = (p.headers || []).map((h, i) => `
      <div class="sp-row tight" data-header="${i}" style="margin-bottom:6px">
        <input class="hdr-name" value="${esc(h.name)}" placeholder="header-name" spellcheck="false">
        <input class="hdr-value" value="${esc(h.value)}" placeholder="value" spellcheck="false">
        <button class="sp-btn sm danger" data-hdr-del="${i}" style="flex:0 0 auto">Remove</button>
      </div>`).join('');

    return `
      <div class="sp-card">
        <h3>${isNew ? 'New provider' : 'Edit provider'}</h3>
        <div class="sp-row">
          <div class="sp-field">
            <label>Name</label>
            <input id="prov-name" value="${esc(p.name)}" placeholder="My provider">
          </div>
          <div class="sp-field" style="flex:0 0 218px">
            <label>Protocol</label>
            <select id="prov-kind">
              ${KINDS.map(k => `<option value="${k.id}" ${k.id === kind.id ? 'selected' : ''}>${esc(k.label)}</option>`).join('')}
            </select>
          </div>
        </div>
        <div class="sp-field">
          <label>Base URL</label>
          <input id="prov-base" value="${esc(p.base_url)}" placeholder="https://api.example.com/v1" spellcheck="false">
          <p class="sp-note" id="prov-endpoint">Requests go to <code>${esc(endpointPreview(p))}</code></p>
        </div>
        <div class="sp-row">
          <div class="sp-field" style="flex:0 0 190px">
            <label>Router prefix</label>
            <input id="prov-prefix" value="${esc(p.prefix || '')}" placeholder="${esc(defaultPrefix(p))}" spellcheck="false" style="font-family:ui-monospace,'Cascadia Mono',Consolas,monospace">
          </div>
          <div class="sp-field">
            <label>Published as</label>
            <div class="sp-code" id="prov-prefix-preview" style="padding:7px 10px">${esc(prefixPreview(p))}<button class="sp-copy" data-copy-prefix="1">Copy</button></div>
          </div>
        </div>
        <p class="sp-note" id="prov-prefix-note">${esc(prefixNote(p))}</p>
        <div class="sp-row">
          <div class="sp-field">
            <label>Input price per 1M tokens</label>
            <input id="prov-input-price" data-num type="number" min="0" step="0.01" value="${Number(p.input_price) || 0}">
          </div>
          <div class="sp-field">
            <label>Output price per 1M tokens</label>
            <input id="prov-output-price" data-num type="number" min="0" step="0.01" value="${Number(p.output_price) || 0}">
          </div>
        </div>
        <p class="sp-note">Optional. When set, the request log uses these rates instead of the built-in list prices, so the cost estimate matches what you actually pay.</p>
        <div class="sp-row">
          <div class="sp-field">
            <label>API key</label>
            <input id="prov-key" type="password" value="${esc(p.api_key)}"
                   placeholder="${masked ? 'Leave as is to keep the stored key' : 'sk-…'}" autocomplete="off">
          </div>
        </div>
        <p class="sp-note">${masked
          ? 'The stored key is shown masked. Leave it untouched to keep it, or clear it and type a new one to replace it.'
          : 'Stored in the config file. It is only ever shown masked afterwards.'}</p>

        <div class="sp-field">
          <label>Models</label>
          ${modelRows(p)}
          ${probeBox('model')}
          <div class="sp-actions">
            <input id="prov-model-add" placeholder="Add a model id, e.g. gpt-4o" spellcheck="false">
            <button class="sp-btn sm" id="prov-model-add-btn">Add model</button>
          </div>
          <p class="sp-note">One provider can serve several models — a family, or a router fronting
            many vendors. Each conversation picks one from this list; the first is the default for
            new conversations. Test a single model to check that the endpoint still serves it.</p>
        </div>

        <div class="sp-field">
          <label>Agent headers</label>
          <select id="prov-agent">
            ${agentPresets().map(a => `<option value="${esc(a.id)}" ${a.id === p.agent ? 'selected' : ''}>${esc(a.label)}</option>`).join('')}
          </select>
          <p class="sp-note" id="prov-agent-hint">${esc(agentHint(p.agent))}</p>
        </div>

        <div class="sp-field">
          <label>Extra headers</label>
          ${headers || '<p class="sp-note tight">None. These are added after the preset, so a custom header can override one.</p>'}
          <div class="sp-actions">
            <button class="sp-btn sm" id="hdr-add">Add header</button>
          </div>
        </div>

        <p class="sp-note" id="prov-error"></p>
        <div class="sp-actions">
          <button class="sp-btn primary" id="prov-save">${isNew ? 'Add provider' : 'Save provider'}</button>
          <button class="sp-btn ghost" id="prov-cancel">Cancel</button>
          <span class="sp-spacer"></span>
          <button class="sp-btn" id="prov-test" title="Send one request using the default model">Test default model</button>
        </div>
        ${probeBox('provider')}
      </div>`;
  }

  /** The models a provider offers, from whichever field is populated.
   *
   *  `models` is the list; `model` is the single value older configs carry. Both
   *  are read here so a provider saved before the list existed still counts as
   *  ready and still shows its model in the list row. */
  function providerModels(p) {
    const list = Array.isArray(p.models) ? p.models.filter(m => String(m).trim()) : [];
    if (list.length) return list;
    const single = String(p.model || '').trim();
    return single ? [single] : [];
  }

  function modelCount(p) {
    return providerModels(p).length;
  }

  function modelSummary(p) {
    const models = providerModels(p);
    if (!models.length) return 'no model';
    if (models.length === 1) return models[0];
    return `${models[0]} +${models.length - 1} more`;
  }

  /** The model list rows in the provider editor.
   *
   *  The first entry is marked as the default, because `provider.model` is what a
   *  new conversation starts on and a list with no clear default leaves the
   *  choice to whichever row happens to be first.
   *
   *  Every row carries its own Test button. A provider can front several vendors,
   *  and "the provider works" says nothing about whether *this* model id is one
   *  the endpoint still serves — a router that dropped a vendor keeps answering
   *  for the others while returning `model_not_found` for that one. Testing the
   *  whole provider at once cannot tell those two apart. */
  function modelRows(p) {
    const models = providerModels(p);
    if (!models.length) {
      return `<p class="sp-note tight">No model yet. Add at least one — the chat panel cannot send without one.</p>`;
    }
    return `<div class="sp-list tight">${models.map((m, i) => `
      <div class="sp-item" data-model-row="${i}">
        <div class="i-main">
          <div class="i-name">${esc(m)}</div>
          <div class="i-meta">${i === 0 ? 'default for new conversations' : 'available'}</div>
        </div>
        ${i === 0
          ? '<span class="sp-pill ok">default</span>'
          : `<button class="sp-btn sm ghost" data-model-default="${i}">Make default</button>`}
        <div class="i-acts">
          <button class="sp-btn sm ghost" data-model-test="${i}" title="Send one request to this model">Test</button>
          <button class="sp-btn sm danger" data-model-del="${i}">Remove</button>
        </div>
      </div>`).join('')}</div>`;
  }

  function endpointPreview(p) {    const base = String(p.base_url || '').trim().replace(/\/+$/, '');
    const kind = KINDS.find(k => k.id === p.kind) || KINDS[0];
    if (!base) return '…';
    return base.endsWith(kind.path) ? base : `${base}${kind.path}`;
  }

  /** The prefix a provider publishes under, or the one it would get by default.
   *
   *  Mirrors `settings::slugify` + `Provider::effective_prefix` so the preview is
   *  the id a client will really have to send. Two implementations of the same
   *  rule is a smell, but the alternative is a round trip per keystroke; the
   *  backend still validates on save, so a mismatch is caught rather than
   *  silently published. */
  function slugify(text) {
    let out = '';
    let pending = false;
    for (const ch of String(text || '').trim()) {
      if (/[A-Za-z0-9_.]/.test(ch)) {
        if (pending && out) out += '-';
        pending = false;
        out += ch.toLowerCase();
      } else {
        pending = true;
      }
    }
    return out.replace(/^[-.]+|[-.]+$/g, '');
  }

  function defaultPrefix(p) {
    const slug = slugify(p && p.name);
    if (!slug) return 'provider';
    if (slug === 'combo') return 'combo-2';
    return slug;
  }

  function effectivePrefix(p) {
    const own = slugify(p && p.prefix);
    if (own && own !== 'combo') return own;
    return defaultPrefix(p);
  }

  /** The full model id a client would pass for this provider's first model. */
  function prefixPreview(p) {
    const model = providerModels(p)[0] || 'model-id';
    return `${effectivePrefix(p)}/${model}`;
  }

  /** The sentence under the prefix field: what it means, or why it is refused. */
  function prefixNote(p) {
    const raw = String((p && p.prefix) || '').trim();
    if (raw && slugify(raw) === 'combo') {
      return '“combo” is reserved: combo/… always means a combo, so a provider cannot use it.';
    }
    const clash = (state.draft.providers || []).some(other =>
      other.id !== p.id && effectivePrefix(other) === effectivePrefix(p));
    if (clash) {
      return 'Another provider already publishes under this prefix, so their models would collide. Change one of them.';
    }
    if (!raw) {
      return `Empty means the prefix follows the name: “${defaultPrefix(p)}”. Every model on this provider is published as ${defaultPrefix(p)}/<model-id>.`;
    }
    return `Published as ${effectivePrefix(p)}/<model-id>. A model id with slashes keeps them, so bansos/deepseek-v4.1 becomes ${effectivePrefix(p)}/bansos/deepseek-v4.1.`;
  }

  // ---- pane: AI Router -----------------------------------------------------

  /** Live state from the backend, with a fallback for the first paint. */
  function routerLive() {
    const live = (state.view && state.view.router_live) || {};
    return {
      running: !!live.running,
      port: live.port || (state.draft.router && state.draft.router.port) || 11930,
      localEndpoint: live.localEndpoint || `http://127.0.0.1:${(state.draft.router && state.draft.router.port) || 11930}/v1`,
      networkEndpoint: live.networkEndpoint || null,
      tailnetEndpoint: live.tailnetEndpoint || null,
      tunnel: live.tunnel || { running: false, url: '', endpoint: '' },
      tailscaleState: live.tailscaleState || { installed: false, address: '', enabled: false },
      cloudflaredAvailable: !!live.cloudflaredAvailable,
      logFile: live.logFile || '',
      logBytes: live.logBytes || 0,
      models: live.models || 0,
      combos: live.combos || 0,
      enabledKeys: live.enabledKeys || 0,
    };
  }

  /** Re-read only the router's live state.
   *
   *  A full `get_settings` would also re-send the draft, and adopting it would
   *  throw away unsaved edits on the other tabs — so a start, a stop or a tunnel
   *  refresh asks the narrow question instead. */
  async function refreshRouterLive() {
    try {
      const live = await invoke('router_state');
      if (state.view) state.view.router_live = live;
    } catch (e) {
      flash(String(e?.message || e), true);
    }
  }

  function paneRouter() {
    const r = state.draft.router;
    const live = routerLive();
    const published = (state.view && state.view.published) || [];
    const providers = published.filter(p => p.source === 'provider');
    // Combos are published models too, but they are not providers: they have no
    // prefix, no upstream id and no price. Listing them under a provider's
    // prefix would be wrong, and leaving them out of the copy list would mean a
    // client never learns the combo ids exist.
    const combos = published.filter(p => p.source === 'combo');
    const prefixes = [...new Set(providers.map(p => p.id.split('/')[0]))];

    const keyRows = (r.keys || []).length ? (r.keys || []).map((k, i) => `
      <div class="sp-item">
        <div class="i-main">
          <div class="i-name">${esc(k.name || 'Untitled key')}</div>
          <div class="i-meta">${esc(k.key || '(new)')}${(k.models || []).length ? ` · ${k.models.length} rule${k.models.length === 1 ? '' : 's'}` : ' · every model'}</div>
        </div>
        <button class="sp-switch ${k.enabled !== false ? 'on' : ''}" data-key-toggle="${i}" title="${k.enabled !== false ? 'Disable this key' : 'Enable this key'}" aria-pressed="${k.enabled !== false}"></button>
        <div class="i-acts">
          <button class="sp-btn sm ghost" data-key-edit="${i}">Edit</button>
          <button class="sp-btn sm danger" data-key-del="${i}">Delete</button>
        </div>
      </div>`).join('') : `<div class="sp-empty">No API key yet.<br>Without one, only this machine can call the router. Add a key before publishing.</div>`;

    const keyEditor = state.editing && state.editing.kind === 'key' ? keyEditorCard() : '';

    // The endpoint strip is the one thing this page exists to answer: what URL
    // does a client put in its config? It stays at the top, monospace, with the
    // live port — not a saved value that may already be wrong.
    const endpoints = [
      { name: 'This machine', where: 'loopback', url: live.localEndpoint },
    ];
    if (live.networkEndpoint) endpoints.push({ name: 'Same network', where: 'LAN', url: live.networkEndpoint });
    if (live.tailnetEndpoint) endpoints.push({ name: 'Tailnet', where: 'Tailscale', url: live.tailnetEndpoint });
    if (live.tunnel.running && live.tunnel.endpoint) {
      endpoints.push({ name: 'Public', where: 'Cloudflare tunnel', url: live.tunnel.endpoint });
    }

    return `
      <h2 class="sp-title">AI Router</h2>
      <p class="sp-lede">One local endpoint that republishes every model from your AI providers, so any OpenAI- or Anthropic-compatible client can use them without knowing which vendor answers. A model is addressed as <code>prefix/model-id</code>, and a combo as <code>combo/name</code>.</p>

      <div class="sp-endpoint ${live.running ? 'live' : ''}">
        <div class="ep-head">
          <span class="ep-dot" aria-hidden="true"></span>
          <span class="ep-state">${live.running ? 'Listening' : 'Stopped'}</span>
          <span class="ep-port">:${live.port}</span>
          <span class="spacer"></span>
          <button class="sp-btn sm ${live.running ? '' : 'primary'}" id="router-toggle">${live.running ? 'Stop router' : 'Start router'}</button>
        </div>
        ${endpoints.map(e => `
          <div class="ep-row">
            <span class="ep-name">${esc(e.name)}</span>
            <code class="ep-url">${esc(e.url)}</code>
            <button class="sp-btn sm ghost" data-copy="${esc(e.url)}">Copy</button>
          </div>`).join('')}
        <p class="ep-note">${live.models} model${live.models === 1 ? '' : 's'} and ${live.combos} combo${live.combos === 1 ? '' : 's'} published${live.enabledKeys ? ` · ${live.enabledKeys} active key${live.enabledKeys === 1 ? '' : 's'}` : ' · no API key'}</p>
      </div>

      <div class="sp-card">
        <h3>Listener</h3>
        <div class="sp-row">
          <div class="sp-field" style="flex:0 0 130px">
            <label>Port</label>
            <input data-path="router.port" data-num type="number" min="1" max="65535" value="${r.port || 11930}">
          </div>
          <div class="sp-field">
            <label>Agent headers sent upstream</label>
            <select data-path="router.agent">
              ${agentPresets().map(a => `<option value="${esc(a.id)}" ${a.id === r.agent ? 'selected' : ''}>${esc(a.label)}</option>`).join('')}
            </select>
            <p class="sp-note" id="router-agent-hint">${esc(agentHint(r.agent))}</p>
          </div>
        </div>
        <label class="sp-choice inline ${r.publish ? 'on' : ''}">
          <input type="checkbox" data-path="router.publish" ${r.publish ? 'checked' : ''}>
          <span class="cbody">
            <span class="cname">Publish to the local network</span>
            <span class="cdesc">Off means the router only answers on 127.0.0.1. On binds every interface, so another device on the same network can reach it — and an API key becomes mandatory.</span>
          </span>
        </label>
        <p class="sp-note">Changing the port, the publish switch or Tailscale restarts the listener as soon as you save. The port is what keeps a client config valid across launches, so it is fixed rather than ephemeral.</p>
      </div>

      <div class="sp-card">
        <h3>Public access</h3>
        <div class="sp-tunnel">
          <div class="tun-main">
            <div class="tun-title">Cloudflare tunnel</div>
            <div class="tun-meta">${live.tunnel.running
              ? `Public at <code>${esc(live.tunnel.endpoint)}</code>`
              : (live.cloudflaredAvailable
                ? 'Not running. Starts a quick tunnel with a random trycloudflare.com address.'
                : 'cloudflared is not installed. Install it, then start the tunnel here.')}</div>
          </div>
          <button class="sp-btn sm ${live.tunnel.running ? '' : 'primary'}" id="tunnel-toggle" ${live.cloudflaredAvailable || live.tunnel.running ? '' : 'disabled'}>
            ${live.tunnel.running ? 'Stop tunnel' : 'Start tunnel'}
          </button>
        </div>
        ${live.tunnel.running ? '' : `<div class="sp-code" style="margin-top:10px">winget install Cloudflare.cloudflared<button class="sp-copy" data-copy="winget install Cloudflare.cloudflared">Copy</button></div>`}
        <div class="sp-tunnel" style="margin-top:12px">
          <div class="tun-main">
            <div class="tun-title">Tailscale</div>
            <div class="tun-meta">${live.tailscaleState.installed
              ? (live.tailscaleState.address
                ? `Tailnet address <code>${esc(live.tailscaleState.address)}</code>${live.tailscaleState.hostname ? ` · ${esc(live.tailscaleState.hostname)}` : ''}`
                : 'Installed, but not connected. Sign in to Tailscale first.')
              : 'Tailscale is not installed on this machine.'}</div>
          </div>
          <button class="sp-switch ${r.tailscale ? 'on' : ''}" id="tailscale-toggle" aria-pressed="${!!r.tailscale}" ${live.tailscaleState.installed ? '' : 'disabled'}></button>
        </div>
        <p class="sp-note">A tunnel and Tailscale both widen who can reach the router, so both require an API key. A tunnel forwards to 127.0.0.1 on this machine; Tailscale makes the machine reachable on your tailnet, where Tailscale's own ACLs decide who gets in.</p>
      </div>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>API keys</h3>
          <button class="sp-btn primary" id="key-new">New key</button>
        </div>
        <div class="sp-list">${keyRows}</div>
        ${keyEditor}
        <p class="sp-note">A key is required whenever the router is reachable from anywhere but this machine. <b>Restricted access</b> limits a key to a set of models, matched against the full id — <code>risun/*</code> for one provider, <code>!combo/*</code> to exclude every combo.</p>
      </div>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Published models</h3>
          <span class="sp-pill ${prefixes.length ? 'ok' : 'warn'}">${providers.length} model${providers.length === 1 ? '' : 's'}${combos.length ? ` · ${combos.length} combo${combos.length === 1 ? '' : 's'}` : ''}</span>
          <button class="sp-btn sm ghost" data-copy-all="1" title="Copy every id a client can send, one per line">Copy all</button>
        </div>
        ${prefixes.length ? prefixes.map(prefix => `
          <div class="sp-prefix-group">
            <div class="pg-head"><code>${esc(prefix)}/</code><span>${providers.filter(p => p.id.split('/')[0] === prefix).length}</span></div>
            <div class="pg-ids">${providers.filter(p => p.id.split('/')[0] === prefix).map(p => `<code>${esc(p.id)}</code>`).join('')}</div>
            <button class="sp-copy pg-copy" data-copy-group="${esc(prefix)}" title="Copy these ${providers.filter(p => p.id.split('/')[0] === prefix).length} ids, one per line">Copy</button>
          </div>`).join('') : `<div class="sp-empty">Nothing published yet.<br>Add a provider with at least one model on the AI provider tab.</div>`}
        ${combos.length ? `
          <div class="sp-prefix-group combo-group">
            <div class="pg-head"><code>combo/</code><span>${combos.length}</span></div>
            <div class="pg-ids">${combos.map(c => `<code>${esc(c.id)}</code>`).join('')}</div>
            <button class="sp-copy pg-copy" data-copy-group="combo" title="Copy these ${combos.length} combo ids, one per line">Copy</button>
          </div>` : ''}
        <p class="sp-note">Every model on every provider is published automatically, and each id shown is exactly what a client sends — the prefix plus the model id as configured. Combos are published as <code>combo/name</code> and listed separately, because a combo is not a provider. <b>Copy</b> puts the ids on the clipboard one per line.</p>
      </div>

      <div class="sp-card">
        <h3>Request log</h3>
        <label class="sp-choice inline ${r.log && r.log.enabled !== false ? 'on' : ''}">
          <input type="checkbox" data-path="router.log.enabled" ${!r.log || r.log.enabled !== false ? 'checked' : ''}>
          <span class="cbody">
            <span class="cname">Record every request</span>
            <span class="cdesc">Model, tokens, latency and estimated cost. Kept in a file, never sent anywhere.</span>
          </span>
        </label>
        <div class="sp-field" style="margin-top:11px">
          <label>Keep at most</label>
          <div class="sp-choices">
            ${choice('log-limit', '50', String((r.log && r.log.limit) || 100), '50 requests', 'Enough for a busy hour, and the file stays tiny.')}
            ${choice('log-limit', '100', String((r.log && r.log.limit) || 100), '100 requests', 'The default. About a day of normal use.')}
          </div>
          <p class="sp-note">Older entries are pruned as new ones arrive, so the log has a fixed ceiling instead of growing until the disk fills.</p>
        </div>
        <p class="sp-note" id="router-logfile">${live.logFile ? `File: <code>${esc(live.logFile)}</code>` : ''}</p>
        <p class="sp-note">The chart, the top-model ranking and the totals on the Request logs tab are kept in SQLite next to this file, so clearing the list does not reset them.</p>
      </div>`;
  }

  // ---- pane: AI Combo ------------------------------------------------------

  function newCombo() {
    return {
      id: uid('combo'),
      name: '',
      label: '',
      members: [],
      strategy: 'fallback',
      judge: '',
      enabled: true,
    };
  }

  function editingCombo() {
    if (!state.editing || state.editing.kind !== 'combo') return null;
    if (state.editing.index >= 0) return state.draft.combos[state.editing.index];
    if (!state.editing.draft) state.editing.draft = newCombo();
    return state.editing.draft;
  }

  function paneCombo() {
    const combos = state.draft.combos || [];
    const editing = state.editing && state.editing.kind === 'combo';
    const published = (state.view && state.view.published) || [];
    const strategies = (state.view && state.view.combo_strategies) || [
      { id: 'fallback', label: 'Fallback', hint: 'Try each model in order and return the first usable answer.' },
      { id: 'round-robin', label: 'Round-robin', hint: 'Rotate through the models one request at a time.' },
      { id: 'fusion', label: 'Fusion', hint: 'Ask every model at once, then have the judge combine the answers.' },
    ];

    const list = combos.length ? combos.map((c, i) => `
      <div class="sp-item ${editing && state.editing.index === i ? 'sel' : ''}">
        <div class="i-main">
          <div class="i-name">${esc(c.label || c.name || 'Untitled combo')}</div>
          <div class="i-meta">combo/${esc(c.name || '…')} · ${(c.members || []).length} model${(c.members || []).length === 1 ? '' : 's'} · ${esc(c.strategy)}</div>
        </div>
        <span class="sp-pill ${c.enabled === false ? 'warn' : 'ok'}">${c.enabled === false ? 'off' : 'on'}</span>
        <div class="i-acts">
          <button class="sp-btn sm ghost" data-copy-combo="${i}" title="Copy combo/${esc(c.name || '')} — the id a client sends">Copy</button>
          <button class="sp-btn sm ghost" data-combo-toggle="${i}">${c.enabled === false ? 'Enable' : 'Disable'}</button>
          <button class="sp-btn sm ghost" data-combo-edit="${i}">Edit</button>
          <button class="sp-btn sm danger" data-combo-del="${i}">Delete</button>
        </div>
      </div>`).join('') : `<div class="sp-empty">No combo yet.<br>A combo answers as <code>combo/name</code> and hides which model served the request — so a client config survives swapping vendors.</div>`;

    const combo = editing ? editingCombo() : null;
    const editor = combo ? comboEditor(combo, published, strategies) : '';

    return `
      <h2 class="sp-title">AI Combo</h2>
      <p class="sp-lede">A combo is a name for a group of published models. A client asks for <code>combo/name</code> and never learns which member answered, so the models behind it can be reordered, replaced or retired without touching the client.</p>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Combos</h3>
          <button class="sp-btn primary" id="combo-new">New combo</button>
        </div>
        <div class="sp-list">${list}</div>
      </div>
      ${editor}`;
  }

  function comboEditor(combo, published, strategies) {
    const isNew = state.editing.index < 0;
    const strategy = strategies.find(s => s.id === combo.strategy) || strategies[0];
    const slug = slugify(combo.name || combo.label || '');
    const members = published.map(p => ({
      id: p.id,
      on: (combo.members || []).includes(p.id),
      source: p.source,
    }));

    return `
      <div class="sp-card">
        <h3>${isNew ? 'New combo' : 'Edit combo'}</h3>
        <div class="sp-row">
          <div class="sp-field">
            <label>Name</label>
            <input id="combo-label" value="${esc(combo.label || '')}" placeholder="Fast fallback">
          </div>
          <div class="sp-field" style="flex:0 0 250px">
            <label>Answers as</label>
            <div class="sp-prefix-preview" id="combo-slug">combo/${esc(slug || 'name')}</div>
          </div>
        </div>
        <div class="sp-field">
          <label>Strategy</label>
          <div class="sp-choices">
            ${strategies.map(s => `
              <label class="sp-choice ${s.id === strategy.id ? 'on' : ''}">
                <input type="radio" name="combo-strategy" value="${esc(s.id)}" ${s.id === strategy.id ? 'checked' : ''}>
                <span class="cbody">
                  <span class="cname">${esc(s.label)}</span>
                  <span class="cdesc">${esc(s.hint)}</span>
                </span>
              </label>`).join('')}
          </div>
        </div>
        <div class="sp-field">
          <label>Models — in priority order</label>
          ${members.length ? `<div class="sp-list tight">${members.map((m, i) => `
            <label class="sp-item sp-check-item ${m.on ? 'sel' : ''}">
              <input type="checkbox" data-combo-member="${i}" ${m.on ? 'checked' : ''}>
              <span class="i-main">
                <span class="i-name"><code>${esc(m.id)}</code></span>
                <span class="i-meta">${esc(m.source === 'combo' ? 'combo' : 'provider model')}</span>
              </span>
              <span class="sp-pill">${m.on ? `#${(combo.members || []).indexOf(m.id) + 1}` : '—'}</span>
            </label>`).join('')}</div>`
            : `<div class="sp-empty">No published model to pick from.<br>Add a provider model first — a combo is made of models that already exist.</div>`}
          <p class="sp-note">Order matters for <b>Fallback</b>: the first model that answers wins. For <b>Round-robin</b> the order is the rotation, and for <b>Fusion</b> every member is asked at once.</p>
        </div>
        ${combo.strategy === 'fusion' ? `
        <div class="sp-field">
          <label>Judge model</label>
          <select id="combo-judge">
            <option value="">— the first member that answered —</option>
            ${(combo.members || []).map(id => `<option value="${esc(id)}" ${combo.judge === id ? 'selected' : ''}>${esc(id)}</option>`).join('')}
          </select>
          <p class="sp-note">Fusion asks every member, then has the judge write one answer from theirs. The judge's own answer is not part of the vote, so pick a model you trust to summarise.</p>
        </div>` : ''}
        <p class="sp-note" id="combo-error"></p>
        <div class="sp-actions">
          <button class="sp-btn primary" id="combo-save">${isNew ? 'Add combo' : 'Save combo'}</button>
          <button class="sp-btn ghost" id="combo-cancel">Cancel</button>
        </div>
      </div>`;
  }

  function keyEditorCard() {
    const key = state.editing.index >= 0
      ? state.draft.router.keys[state.editing.index]
      : (state.editing.draft || (state.editing.draft = {
        id: uid('key'), name: '', key: '', enabled: true, models: [], created_at: Date.now(),
      }));
    const published = ((state.view && state.view.published) || []);
    const rules = (key.models || []).join('\n');
    return `
      <div class="sp-card" style="margin-top:12px">
        <h3>${state.editing.index >= 0 ? 'Edit key' : 'New key'}</h3>
        <div class="sp-field">
          <label>Name</label>
          <input id="key-name" value="${esc(key.name)}" placeholder="Laptop, CI, Phone">
        </div>
        <div class="sp-field">
          <label>Secret</label>
          <div class="sp-row tight">
            <input id="key-secret" value="${esc(key.key)}" spellcheck="false" style="font-family:ui-monospace,'Cascadia Mono',Consolas,monospace">
            <button class="sp-btn sm" id="key-generate" style="flex:0 0 auto">Generate</button>
          </div>
          <p class="sp-note">Leave it empty and Save generates one. It is shown in full only until you save; afterwards it is masked, like a provider key.</p>
        </div>
        <div class="sp-field">
          <label>Restricted access</label>
          <textarea id="key-models" spellcheck="false" placeholder="Leave empty for every model&#10;risun/*&#10;!combo/*">${esc(rules)}</textarea>
          <p class="sp-note">One rule per line, matched against the full model id. <code>*</code> and <code>?</code> are wildcards, and a rule starting with <code>!</code> removes a match. A list with only negations starts from “everything”.</p>
          <div class="sp-rule-hint">${published.length
            ? `Currently published: ${published.slice(0, 6).map(p => `<code>${esc(p.id)}</code>`).join(' ')}${published.length > 6 ? ` and ${published.length - 6} more` : ''}`
            : 'No models published yet, so a restriction would match nothing.'}</div>
        </div>
        <p class="sp-note" id="key-error"></p>
        <div class="sp-actions">
          <button class="sp-btn primary" id="key-save">${state.editing.index >= 0 ? 'Save key' : 'Add key'}</button>
          <button class="sp-btn ghost" id="key-cancel">Cancel</button>
        </div>
      </div>`;
  }

  // ---- pane: request logs --------------------------------------------------

  /** The last loaded log payload, so a re-render does not refetch. */
  function logData() {
    return state.logs || { entries: [], stats: null, limit: 100, enabled: true, bytes: 0, file: '' };
  }

  function bytesLabel(bytes) {
    if (!bytes) return '0 KB';
    if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    return `${Math.max(1, Math.round(bytes / 1024))} KB`;
  }

  function tokensLabel(n) {
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
    return String(n);
  }

  function moneyLabel(usd) {
    if (!usd) return '$0.00';
    if (usd < 0.01) return `$${usd.toFixed(4)}`;
    return `$${usd.toFixed(2)}`;
  }

  /** How long ago a timestamp was, in words. */
  /** Milliseconds as a compact, honest duration.
   *
   *  Under a second reads in ms, over it in seconds: "8.4s" is the number a user
   *  is actually looking for when a model feels slow, and "8400 ms" makes them
   *  do the division. */
  function msLabel(ms) {
    if (!ms) return '—';
    if (ms < 1000) return `${ms} ms`;
    if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
    const minutes = Math.floor(ms / 60_000);
    return `${minutes}m ${Math.round((ms % 60_000) / 1000)}s`;
  }

  /** Tokens per second, one decimal, or a dash when there is nothing to divide. */
  function tpsLabel(tps) {
    if (!tps || !Number.isFinite(tps)) return '—';
    return `${tps.toFixed(1)} t/s`;
  }

  function agoLabel(at) {
    const delta = Date.now() - at;
    if (delta < 60_000) return 'just now';
    const minutes = Math.floor(delta / 60_000);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    return `${Math.floor(hours / 24)}d ago`;
  }

  /** The 24-hour chart, drawn as inline SVG.
   *
   *  A bar per hour rather than a line: requests arrive in bursts, and a line
   *  between two quiet hours implies traffic that did not happen. The bars are
   *  the only place in this page where colour carries meaning, so the accent is
   *  spent here and nowhere else. */
  function activityChart(series) {
    const buckets = series && series.length ? series : [];
    if (!buckets.length) return `<div class="sp-empty">No requests recorded yet.</div>`;
    const peak = Math.max(1, ...buckets.map(b => b.requests));
    const width = 100;
    const height = 34;
    const gap = 0.8;
    const barWidth = (width - gap * (buckets.length - 1)) / buckets.length;
    const bars = buckets.map((b, i) => {
      const h = b.requests === 0 ? 0.7 : Math.max(1.5, (b.requests / peak) * height);
      const x = i * (barWidth + gap);
      const y = height - h;
      const cls = b.requests === 0 ? 'zero' : 'on';
      return `<rect class="${cls}" x="${x.toFixed(2)}" y="${y.toFixed(2)}" width="${barWidth.toFixed(2)}" height="${h.toFixed(2)}" rx="0.6"><title>${b.hours_ago}h ago · ${b.requests} request${b.requests === 1 ? '' : 's'} · ${tokensLabel(b.tokens)} tokens</title></rect>`;
    }).join('');
    return `
      <svg class="sp-chart" viewBox="0 0 ${width} ${height}" preserveAspectRatio="none" role="img" aria-label="Requests per hour over the last 24 hours">${bars}</svg>
      <div class="sp-chart-axis"><span>24h ago</span><span>12h</span><span>now</span></div>`;
  }

  function paneLogs() {
    const data = logData();
    const stats = data.stats || {
      requests: 0, failed: 0, input_tokens: 0, output_tokens: 0,
      cost: 0, avg_ms: 0, avg_ttft_ms: 0, avg_tps: 0, window_hours: 24,
      models: [], series: [], endpoints: [],
    };
    const top = (stats.models || []).slice(0, 5);
    const peak = top.length ? Math.max(1, ...top.map(m => m.input_tokens + m.output_tokens)) : 1;
    const entries = data.entries || [];
    const hours = stats.window_hours || 24;

    // A row's identity is its start time, not its position: a new entry pushes
    // every index down, and an open detail panel must stay on the request it was
    // opened for.
    const key = (e) => `done:${e.at}`;
    const detailRow = (e) => `
      <div class="sp-logdetail">
        <div class="ld-grid">
          ${logFact('Status', e.status < 400 ? `${e.status} ok` : `${e.status} failed`)}
          ${logFact('Model', e.model)}
          ${logFact('Provider', e.provider || '—')}
          ${logFact('Upstream id', e.upstream || '—')}
          ${logFact('Endpoint', e.endpoint)}
          ${logFact('Key', e.key || '—')}
          ${logFact('Started', new Date(e.at).toLocaleString())}
          ${logFact('Duration', msLabel(e.ms))}
          ${logFact('First token', e.stream ? (e.ttft_ms ? msLabel(e.ttft_ms) : 'not reported') : 'buffered reply')}
          ${logFact('Generation', e.stream && e.ttft_ms ? msLabel(Math.max(0, e.ms - e.ttft_ms)) : msLabel(e.ms))}
          ${logFact('Throughput', e.tps ? tpsLabel(e.tps) : '—')}
          ${logFact('Tokens', `${e.input_tokens} in · ${e.output_tokens} out${e.estimated ? ' (counted)' : ''}`)}
          ${logFact('Est. cost', e.cost ? moneyLabel(e.cost) : 'no rate for this model')}
          ${logFact('Streamed', e.stream ? 'yes' : 'no')}
        </div>
        ${e.estimated ? `<p class="ld-note">The token count is a count of stream events, because this reply carried no usage block. Close enough for a dashboard; not the provider's own number.</p>` : ''}
        ${e.error ? `<div class="ld-error">${esc(e.error)}</div>` : ''}
        <p class="ld-note">The log records what the router knew, not the conversation: request and reply bodies are never stored, so there is no prompt text here to show.</p>
      </div>`;

    const rows = entries.length ? entries.map((e) => {
      const ok = e.status < 400;
      const open = state.logDetail === key(e);
      return `
        <div class="sp-logrow ${ok ? '' : 'bad'} ${open ? 'open' : ''}" data-log-row="${esc(key(e))}" role="button" tabindex="0" aria-expanded="${open}">
          <span class="lr-status ${ok ? 'ok' : 'bad'}">${e.status}</span>
          <span class="lr-model" title="${esc(e.model)}">${esc(e.model)}</span>
          <span class="lr-endpoint">${esc(e.endpoint)}</span>
          <span class="lr-tokens">${tokensLabel(e.input_tokens + e.output_tokens)}</span>
          <span class="lr-ttft">${e.stream ? (e.ttft_ms ? msLabel(e.ttft_ms) : '—') : '·'}</span>
          <span class="lr-tps">${e.tps ? e.tps.toFixed(1) : '—'}</span>
          <span class="lr-ms">${msLabel(e.ms)}</span>
          <span class="lr-cost">${e.cost ? moneyLabel(e.cost) : '—'}</span>
          <span class="lr-when" title="${esc(new Date(e.at).toLocaleString())}">${agoLabel(e.at)}</span>
        </div>
        ${open ? detailRow(e) : ''}
        ${e.error && !open ? `<div class="sp-logerror">${esc(e.error)}</div>` : ''}`;
    }).join('') : `<div class="sp-empty">Nothing recorded yet.<br>Requests appear here as soon as a client calls the router.</div>`;

    return `
      <h2 class="sp-title">Request logs</h2>
      <p class="sp-lede">Every request the AI Router served, with what it cost and how fast it answered. Costs are <b>estimates</b> from list prices or the per-million rates you set on a provider — never a bill.</p>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Last ${hours} hours</h3>
          <span class="sp-pill ${stats.failed ? 'warn' : (stats.requests ? 'ok' : '')}">${stats.requests} request${stats.requests === 1 ? '' : 's'}${stats.failed ? ` · ${stats.failed} failed` : ''}</span>
          <button class="sp-btn sm ghost" id="logs-refresh">Refresh</button>
        </div>
        ${activityChart(stats.series)}
        <div class="sp-stats">
          <div class="sp-stat"><span class="s-val">${tokensLabel(stats.input_tokens + stats.output_tokens)}</span><span class="s-lbl">tokens</span></div>
          <div class="sp-stat"><span class="s-val">${stats.avg_ttft_ms ? msLabel(stats.avg_ttft_ms) : '—'}</span><span class="s-lbl">avg first token</span></div>
          <div class="sp-stat"><span class="s-val">${stats.avg_tps ? stats.avg_tps.toFixed(1) : '—'}</span><span class="s-lbl">tokens/sec</span></div>
          <div class="sp-stat"><span class="s-val">${moneyLabel(stats.cost)}</span><span class="s-lbl">est. cost</span></div>
          <div class="sp-stat"><span class="s-val">${bytesLabel(data.bytes)}</span><span class="s-lbl">log size</span></div>
        </div>
        <p class="sp-note">These totals come from a SQLite file beside the log, so <b>Clear log</b> empties the list below without resetting the chart, the ranking or these figures.</p>
      </div>

      <div class="sp-card">
        <h3>Top models by tokens</h3>
        ${top.length ? `<div class="sp-bars">${top.map(m => {
          const total = m.input_tokens + m.output_tokens;
          const pct = Math.max(2, Math.round((total / peak) * 100));
          return `<div class="sp-bar">
            <div class="b-head"><code>${esc(m.name)}</code><span>${tokensLabel(total)} · ${m.requests} req</span></div>
            <div class="b-track"><div class="b-fill" style="width:${pct}%"></div></div>
            <div class="b-foot"><span>${tokensLabel(m.input_tokens)} in</span><span>${tokensLabel(m.output_tokens)} out</span><span>${m.cost ? moneyLabel(m.cost) : 'no rate'}</span></div>
          </div>`;
        }).join('')}</div>` : `<div class="sp-empty">No model has answered yet.</div>`}
      </div>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Requests</h3>
          ${(data.live || []).length ? `<span class="sp-pill live" id="logs-live-pill"><span class="lr-spin"></span>${data.live.length} running</span>` : ''}
          <span class="sp-pill">${entries.length}/${data.limit}</span>
          <button class="sp-btn sm danger" id="logs-clear">Clear log</button>
        </div>
        <div class="sp-logtable">
          <div class="sp-loghead">
            <span class="lr-status">Status</span>
            <span class="lr-model">Model</span>
            <span class="lr-endpoint">API</span>
            <span class="lr-tokens">Tokens</span>
            <span class="lr-ttft" title="Time to first token">TTFT</span>
            <span class="lr-tps" title="Output tokens per second, after the first token">t/s</span>
            <span class="lr-ms">Total</span>
            <span class="lr-cost">Est.</span>
            <span class="lr-when">When</span>
          </div>
          <div class="sp-livelist" id="logs-live">${liveRowsHtml()}</div>
          ${rows}
        </div>
        <p class="sp-note">Pruned to the newest ${data.limit} entries. Click a row for its detail. ${data.file ? `<code>${esc(data.file)}</code>` : ''}</p>
      </div>`;
  }

  /** One labelled fact in the detail panel. */
  function logFact(label, value) {
    return `<div class="ld-fact"><span class="ld-lbl">${esc(label)}</span><span class="ld-val">${esc(String(value))}</span></div>`;
  }

  /** The requests being served right now, as table rows.
   *
   *  Kept in its own function and its own container so the live poll can replace
   *  this block alone: re-rendering the whole pane every couple of seconds would
   *  throw away the scroll position and any open detail panel. */
  function liveRowsHtml() {
    const rows = (state.logs && state.logs.live) || [];
    if (!rows.length) return '';
    return rows.map(r => {
      const open = state.logDetail === `live:${r.id}`;
      const tps = r.tps ? r.tps.toFixed(1) : '—';
      const ttft = r.ttft_ms != null ? msLabel(r.ttft_ms) : 'waiting';
      return `
        <div class="sp-logrow live ${open ? 'open' : ''}" data-log-row="live:${r.id}" role="button" tabindex="0" aria-expanded="${open}">
          <span class="lr-status run"><span class="lr-spin"></span></span>
          <span class="lr-model" title="${esc(r.model)}">${esc(r.model)}</span>
          <span class="lr-endpoint">${esc(r.endpoint)}</span>
          <span class="lr-tokens">${r.output_tokens ? `~${tokensLabel(r.output_tokens)}` : '—'}</span>
          <span class="lr-ttft">${esc(ttft)}</span>
          <span class="lr-tps">${tps}</span>
          <span class="lr-ms">${msLabel(r.elapsed_ms)}</span>
          <span class="lr-cost">—</span>
          <span class="lr-when">now</span>
        </div>
        ${open ? `
        <div class="sp-logdetail live">
          <div class="ld-grid">
            ${logFact('Status', 'streaming')}
            ${logFact('Model', r.model)}
            ${logFact('Provider', r.provider || 'not routed yet')}
            ${logFact('Endpoint', r.endpoint)}
            ${logFact('Key', r.key || '—')}
            ${logFact('Started', new Date(r.at).toLocaleString())}
            ${logFact('Elapsed', msLabel(r.elapsed_ms))}
            ${logFact('First token', r.ttft_ms != null ? msLabel(r.ttft_ms) : 'not yet')}
            ${logFact('Tokens so far', r.output_tokens ? `~${r.output_tokens}` : '—')}
            ${logFact('Throughput', r.tps ? tpsLabel(r.tps) : '—')}
            ${logFact('Reply', r.stream ? 'streamed' : 'buffered')}
          </div>
          <p class="ld-note">This request is still running. Token counts are counted from the stream as it arrives, so they are an estimate until it finishes.</p>
        </div>` : ''}`;
    }).join('');
  }

  function paneMcp() {
    const m = state.draft.mcp;
    const live = state.mcp || {};
    const enabled = !!(state.view && state.view.mcp_enabled);
    const facts = state.facts || { lan_addresses: [], public_address: null };
    const port = live.port || m.port || 0;
    const shim = live.shimPath || 'rustbox-mcp.exe';

    // One guide for a loopback bridge; three when it is published, because the
    // address a client must use then depends on where that client runs.
    const guides = [];
    if (m.bind === 'network') {
      guides.push({ name: 'This machine', where: 'loopback', host: '127.0.0.1' });
      guides.push({
        name: 'Same network',
        where: facts.lan_addresses[0] || 'no LAN address found',
        host: facts.lan_addresses[0] || '<this machine LAN address>',
      });
      guides.push({
        name: 'Anywhere',
        where: facts.public_address || 'not looked up yet',
        host: facts.public_address || '<public address>',
      });
    } else {
      guides.push({ name: 'This machine', where: 'loopback only', host: '127.0.0.1' });
    }

    const guideHtml = guides.map(g => `
      <div class="sp-guide">
        <div class="sp-guide-head">
          <span class="g-name">${esc(g.name)}</span>
          <span class="g-where">${esc(g.where)}</span>
        </div>
        <div class="sp-code"><button class="sp-copy" data-copy="${esc(guideJson(shim, g.host, port))}">Copy</button>${esc(guideJson(shim, g.host, port))}</div>
      </div>`).join('');

    return `
      <h2 class="sp-title">MCP bridge</h2>
      <p class="sp-lede">Lets an external agent — Claude Desktop, Cursor, Droid — read the tabs you already have open, run scripts in them, and follow chat conversations. Off until you turn it on, and every request needs a token that is regenerated when the bridge restarts.</p>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Agent access</h3>
          <button class="sp-switch ${enabled ? 'on' : ''}" id="mcp-toggle" title="${enabled ? 'Turn off' : 'Turn on'}"></button>
        </div>
        <p class="sp-note">${enabled
          ? `Listening${live.port ? ` on port ${live.port}` : ''}${m.bind === 'network' ? ' on every interface' : ' on 127.0.0.1'}. Endpoint file: <code>${esc(live.endpointFile || 'unknown')}</code>`
          : 'Off. No socket is open, and no client can connect.'}</p>
      </div>

      <div class="sp-card">
        <h3>Where it listens</h3>
        <div class="sp-row">
          <div class="sp-field" style="flex:0 0 150px">
            <label>Port</label>
            <input data-path="mcp.port" data-num type="number" value="${m.port || ''}" placeholder="random">
          </div>
          <div class="sp-field" style="display:flex;align-items:flex-end">
            <p class="sp-note" style="margin:0 0 9px">A fixed port keeps a client config valid across launches. Empty means a fresh random port each start, which no client config can follow.</p>
          </div>
        </div>
        <div class="sp-field">
          <label>Reachable from</label>
          <div class="sp-choices">
            ${choice('mcp-bind', 'loopback', m.bind, 'This machine only', '127.0.0.1. Nothing on the network can reach the port.')}
            ${choice('mcp-bind', 'network', m.bind, 'Any interface', 'The port becomes reachable from your network. The token is still required, but the surface is no longer just this machine.')}
          </div>
        </div>
        ${m.bind === 'network' ? `<p class="sp-note warn">Only do this on a network you trust. A NAT router still has to forward the port before anything outside can reach it, and a LAN address will not work from outside.</p>` : ''}
        <div class="sp-field">
          <label>What an agent may act on</label>
          <div class="sp-choices">
            ${choice('mcp-scope', 'all-tabs', m.scope, 'Every open tab', 'Hidden tabs included: they are still open and signed in.')}
            ${choice('mcp-scope', 'active-tab', m.scope, 'The tab on screen only', 'Enforced in the backend, so a request naming any other tab is refused rather than quietly ignored.')}
          </div>
        </div>
        <div class="sp-field">
          <label class="sp-choice inline ${m.allow_app_edits ? 'on' : ''}">
            <input type="checkbox" id="mcp-edits" ${m.allow_app_edits ? 'checked' : ''}>
            <span class="cbody">
              <span class="cname">Let an agent add, edit and delete apps</span>
              <span class="cdesc">Off by default. Reading a page and rewriting your workspace are different levels of trust.</span>
            </span>
          </label>
        </div>
      </div>

      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Client configuration</h3>
          ${m.bind === 'network' ? '<button class="sp-btn sm" id="mcp-public">Look up public address</button>' : ''}
        </div>
        ${guideHtml}
        <p class="sp-note">${m.bind === 'network'
          ? 'Three guides, because the address a client must use depends on where that client runs. The shim reads the port and token from the endpoint file, so no secret is pasted here.'
          : 'One guide, because every client on this machine reaches the bridge the same way. The shim reads the port and token from the endpoint file, so no secret is pasted here.'}</p>
      </div>`;
  }

  function guideJson(shim, host, port) {
    const env = {};
    // The shim dials 127.0.0.1 by default, so only a published bridge needs to
    // be told a different address.
    if (host !== '127.0.0.1') env.RUSTBOX_MCP_HOST = host;
    if (port) env.RUSTBOX_MCP_PORT = String(port);
    const entry = { command: String(shim || 'rustbox-mcp.exe') };
    if (Object.keys(env).length) entry.env = env;
    return JSON.stringify({ mcpServers: { rustbox: entry } }, null, 2);
  }

  // ---- pane: skills --------------------------------------------------------

  function paneSkills() {
    const skills = state.draft.skills;
    const editing = state.editing && state.editing.kind === 'skill';

    const list = skills.length ? skills.map((s, i) => `
      <div class="sp-item ${editing && state.editing.index === i ? 'sel' : ''}">
        <div class="i-main">
          <div class="i-name">${esc(s.name || 'Untitled skill')}</div>
          <div class="i-meta">${esc(s.description || (s.url ? s.url : 'no description'))}</div>
        </div>
        <button class="sp-switch ${s.enabled !== false ? 'on' : ''}" data-skill-toggle="${i}" title="${s.enabled !== false ? 'Disable' : 'Enable'}"></button>
        <div class="i-acts">
          <button class="sp-btn sm ghost" data-skill-edit="${i}">Edit</button>
          <button class="sp-btn sm danger" data-skill-del="${i}">Delete</button>
        </div>
      </div>`).join('') : `<div class="sp-empty">No skills yet.<br>A skill is a block of instructions the chat agent is told to follow.</div>`;

    const skill = editing
      ? (state.editing.index >= 0
        ? state.draft.skills[state.editing.index]
        : (state.editing.draft || (state.editing.draft = {
          id: uid('skill'), name: '', description: '', instructions: '',
          enabled: true, source: 'manual', url: null,
        })))
      : null;

    const editor = skill ? `
      <div class="sp-card">
        <h3>${state.editing.index >= 0 ? 'Edit skill' : 'New skill'}</h3>
        <div class="sp-field">
          <label>Install from a URL or owner/repo</label>
          <div class="sp-row">
            <input id="skill-url" placeholder="https://…/SKILL.md   or   owner/repo" spellcheck="false">
            <button class="sp-btn" id="skill-fetch" style="flex:0 0 auto">Fetch</button>
          </div>
          <p class="sp-note" id="skill-fetch-note">Reads a <code>SKILL.md</code> and fills in the fields below. Nothing is installed until you save. Skills are shared on <code>skills.sh</code>.</p>
        </div>
        <div class="sp-field">
          <label>Name</label>
          <input id="skill-name" value="${esc(skill.name)}" placeholder="Review a diff">
        </div>
        <div class="sp-field">
          <label>Description</label>
          <input id="skill-desc" value="${esc(skill.description)}" placeholder="One line, shown in the list">
        </div>
        <div class="sp-field">
          <label>Instructions</label>
          <textarea id="skill-text" spellcheck="false" placeholder="What to do, what not to do, and what done looks like.">${esc(skill.instructions)}</textarea>
        </div>
        <p class="sp-note" id="skill-error"></p>
        <div class="sp-actions">
          <button class="sp-btn primary" id="skill-save">${state.editing.index >= 0 ? 'Save skill' : 'Add skill'}</button>
          <button class="sp-btn ghost" id="skill-cancel">Cancel</button>
        </div>
      </div>` : '';

    return `
      <h2 class="sp-title">Skills</h2>
      <p class="sp-lede">Instructions the chat agent loads before answering. Enabled skills are added to the system prompt of every conversation, so they shape how the agent works in this workspace.</p>
      <div class="sp-card">
        <div class="sp-headrow">
          <h3>Skills</h3>
          <button class="sp-btn primary" id="skill-new">New skill</button>
        </div>
        <div class="sp-list">${list}</div>
      </div>
      ${editor}`;
  }

  // ---- wiring --------------------------------------------------------------

  function wirePane() {
    const pane = $('#sp-pane');

    // Generic draft binding. Plain typing never re-renders, so focus is never
    // lost mid-word; a structural change (radio, checkbox, a list action) does.
    pane.querySelectorAll('[data-path]').forEach(input => {
      const handler = () => {
        let value;
        if (input.type === 'checkbox') value = input.checked;
        else if (input.hasAttribute('data-num')) {
          const n = Number(input.value);
          value = input.value === '' ? null : (Number.isFinite(n) ? n : null);
        } else value = input.value;
        setPath(state.draft, input.dataset.path, value);
        state.dirty = true;
        if (input.dataset.path === 'global_proxy.pool_id') {
          const hint = $('#gp-pool-hint');
          if (hint) hint.textContent = poolHint();
          renderRail();
        }
        if (input.type === 'checkbox') render();
      };
      input.addEventListener('input', handler);
      input.addEventListener('change', handler);
    });

    pane.querySelectorAll('input[name="gp-mode"]').forEach(r => {
      r.addEventListener('change', () => {
        state.draft.global_proxy.mode = r.value;
        state.dirty = true;
        render();
      });
    });
    pane.querySelectorAll('input[name="mcp-bind"]').forEach(r => {
      r.addEventListener('change', () => {
        state.draft.mcp.bind = r.value;
        state.dirty = true;
        render();
      });
    });
    pane.querySelectorAll('input[name="mcp-scope"]').forEach(r => {
      r.addEventListener('change', () => {
        state.draft.mcp.scope = r.value;
        state.dirty = true;
        render();
      });
    });
    pane.querySelectorAll('input[name="theme"]').forEach(r => {
      r.addEventListener('change', () => {
        state.draft.theme = r.value;
        state.dirty = true;
        // Preview immediately: the theme is the one setting whose effect the
        // user can see, so making them save first would be theatre.
        applyTheme(r.value);
        render();
      });
    });

    const edits = pane.querySelector('#mcp-edits');
    if (edits) {
      edits.addEventListener('change', () => {
        state.draft.mcp.allow_app_edits = edits.checked;
        state.dirty = true;
        render();
      });
    }

    // ---- proxy test
    const testGlobal = pane.querySelector('#test-global');
    if (testGlobal) {
      testGlobal.addEventListener('click', async () => {
        const proxy = proxyObject(state.draft.global_proxy.custom);
        if (!proxy.host || !proxy.port) {
          setProbe('global', { status: 'fail', text: 'Enter a host and a port first.' });
          return;
        }
        setProbe('global', { status: 'busy', text: 'Testing…' });
        try {
          const probe = await invoke('test_proxy', { uri: proxyUri(proxy) });
          setProbe('global', {
            status: probe.ok ? 'ok' : 'fail',
            text: probe.ok
              ? `Connected through the proxy${probe.exit_ip ? ` — exit address ${probe.exit_ip}` : ''}.`
              : probe.message,
            ms: probe.ms,
          });
        } catch (e) {
          setProbe('global', { status: 'fail', text: String(e?.message || e) });
        }
      });
    }

    // ---- pools
    const poolNew = pane.querySelector('#pool-new');
    if (poolNew) {
      poolNew.addEventListener('click', () => {
        state.editing = { kind: 'pool', index: -1 };
        render();
      });
    }
    pane.querySelectorAll('[data-pool-edit]').forEach(btn => {
      btn.addEventListener('click', () => {
        state.editing = { kind: 'pool', index: Number(btn.dataset.poolEdit) };
        render();
      });
    });
    pane.querySelectorAll('[data-pool-del]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const index = Number(btn.dataset.poolDel);
        const pool = state.draft.pools[index];
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Delete pool?',
          text: `"${poolName(pool)}" will be removed. Apps that inherit it will report that the pool is gone rather than silently connecting directly.`,
          okLabel: 'Delete',
        });
        if (!ok) return;
        state.draft.pools.splice(index, 1);
        if (state.draft.global_proxy.pool_id === pool.id) state.draft.global_proxy.pool_id = null;
        state.editing = null;
        state.dirty = true;
        render();
      });
    });
    const poolCancel = pane.querySelector('#pool-cancel');
    if (poolCancel) poolCancel.addEventListener('click', () => { state.editing = null; render(); });

    const poolSave = pane.querySelector('#pool-save');
    if (poolSave) {
      poolSave.addEventListener('click', () => {
        const pool = state.editing && (state.editing.index >= 0
          ? state.draft.pools[state.editing.index]
          : state.editing.draft);
        if (!pool) return;
        const name = pane.querySelector('#pool-name').value.trim();
        const strategy = pane.querySelector('#pool-strategy').value;
        const lines = pane.querySelector('#pool-members').value.split(/\r?\n/);
        const errorBox = pane.querySelector('#pool-error');
        if (!name) { errorBox.textContent = 'Give the pool a name.'; return; }
        const proxies = [];
        const problems = [];
        lines.forEach((line, i) => {
          if (!line.trim()) return;
          const parsed = parseProxyLine(line);
          if (parsed.error) problems.push(`Line ${i + 1}: ${parsed.error}`);
          else proxies.push(parsed.proxy);
        });
        if (problems.length) { errorBox.textContent = problems.join(' · '); return; }
        errorBox.textContent = '';
        const saved = { id: pool.id, name, proxies, strategy };
        if (state.editing.index >= 0) state.draft.pools[state.editing.index] = saved;
        else state.draft.pools.push(saved);
        state.editing = null;
        state.dirty = true;
        render();
      });
    }

    const poolTest = pane.querySelector('#pool-test');
    if (poolTest) {
      poolTest.addEventListener('click', async () => {
        const lines = pane.querySelector('#pool-members').value.split(/\r?\n/).filter(l => l.trim());
        if (!lines.length) { setProbe('pool', { status: 'fail', text: 'No members to test.' }); return; }
        setProbe('pool', { status: 'busy', text: `Testing ${lines.length}…` });
        const results = [];
        for (let i = 0; i < lines.length; i += 1) {
          const parsed = parseProxyLine(lines[i]);
          if (parsed.error) { results.push(`✗ line ${i + 1}: ${parsed.error}`); continue; }
          try {
            const probe = await invoke('test_proxy', { uri: proxyUri(parsed.proxy) });
            results.push(probe.ok
              ? `✓ ${proxyUri(parsed.proxy)}${probe.exit_ip ? ` via ${probe.exit_ip}` : ''}`
              : `✗ ${proxyUri(parsed.proxy)}: ${probe.message}`);
          } catch (e) {
            results.push(`✗ ${proxyUri(parsed.proxy)}: ${String(e?.message || e)}`);
          }
        }
        const failed = results.filter(r => r.startsWith('✗')).length;
        setProbe('pool', { status: failed ? 'fail' : 'ok', text: results.join('\n') });
      });
    }

    // ---- providers
    const provNew = pane.querySelector('#prov-new');
    if (provNew) {
      provNew.addEventListener('click', () => {
        state.editing = { kind: 'provider', index: -1 };
        render();
      });
    }
    pane.querySelectorAll('[data-prov-edit]').forEach(btn => {
      btn.addEventListener('click', () => {
        state.editing = { kind: 'provider', index: Number(btn.dataset.provEdit) };
        render();
      });
    });
    pane.querySelectorAll('[data-prov-del]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const index = Number(btn.dataset.provDel);
        const provider = state.draft.providers[index];
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Delete provider?',
          text: `"${providerName(provider)}" and its stored key will be removed.`,
          okLabel: 'Delete',
        });
        if (!ok) return;
        state.draft.providers.splice(index, 1);
        if (state.draft.chat.provider_id === provider.id) state.draft.chat.provider_id = '';
        state.editing = null;
        state.dirty = true;
        render();
      });
    });
    const provCancel = pane.querySelector('#prov-cancel');
    if (provCancel) provCancel.addEventListener('click', () => { state.editing = null; render(); });

    const provKind = pane.querySelector('#prov-kind');
    const provBase = pane.querySelector('#prov-base');
    const refreshEndpoint = () => {
      const preview = pane.querySelector('#prov-endpoint');
      if (!preview) return;
      preview.innerHTML = `Requests go to <code>${esc(endpointPreview({
        base_url: provBase ? provBase.value : '',
        kind: provKind ? provKind.value : 'chat-completions',
      }))}</code>`;
    };
    if (provKind) provKind.addEventListener('change', refreshEndpoint);
    if (provBase) provBase.addEventListener('input', refreshEndpoint);

    const provAgent = pane.querySelector('#prov-agent');
    if (provAgent) {
      provAgent.addEventListener('change', () => {
        const hint = pane.querySelector('#prov-agent-hint');
        if (hint) hint.textContent = agentHint(provAgent.value);
      });
    }

    const provPrefix = pane.querySelector('#prov-prefix');
    if (provPrefix) {
      const refresh = () => {
        const provider = editingProvider();
        if (!provider) return;
        // A live preview of the id a client will send. The draft is updated in
        // place so the preview and the note agree with what Save will write.
        provider.prefix = provPrefix.value;
        const preview = pane.querySelector('#prov-prefix-preview');
        if (preview) {
          preview.innerHTML = `${esc(prefixPreview(provider))}<button class="sp-copy" data-copy-prefix="1">Copy</button>`;
        }
        const note = pane.querySelector('#prov-prefix-note');
        if (note) note.textContent = prefixNote(provider);
      };
      provPrefix.addEventListener('input', refresh);
      provPrefix.addEventListener('change', async () => {
        refresh();
        // The page's copy of the rule is a guess kept in step with the backend's;
        // asking the backend is what settles it. A refusal here is authoritative,
        // and Save would fail on it anyway.
        const provider = editingProvider();
        const note = pane.querySelector('#prov-prefix-note');
        if (!provider || !note) return;
        try {
          const problem = await invoke('check_prefix', {
            providerId: provider.id,
            prefix: slugify(provPrefix.value),
          });
          if (problem) {
            note.textContent = problem;
            note.classList.add('warn');
          } else {
            note.classList.remove('warn');
          }
        } catch (_) {
          // A backend that cannot answer leaves the local note in place.
        }
      });
    }
    const provName = pane.querySelector('#prov-name');
    if (provName) {
      provName.addEventListener('input', () => {
        const provider = editingProvider();
        if (!provider) return;
        // Only the placeholder moves: an explicit prefix is the user's choice
        // and must not be rewritten because the name changed.
        if (!String(provider.prefix || '').trim()) {
          provPrefix.placeholder = defaultPrefix({ name: provName.value });
          const preview = pane.querySelector('#prov-prefix-preview');
          if (preview) {
            preview.innerHTML = `${esc(prefixPreview({ name: provName.value, prefix: '', models: providerModels(provider) }))}<button class="sp-copy" data-copy-prefix="1">Copy</button>`;
          }
          const note = pane.querySelector('#prov-prefix-note');
          if (note) note.textContent = prefixNote({ id: provider.id, name: provName.value, prefix: '' });
        }
      });
    }
    pane.querySelectorAll('[data-copy-prefix]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const provider = editingProvider();
        if (!provider) return;
        try {
          await navigator.clipboard.writeText(prefixPreview(provider));
          btn.textContent = 'Copied';
          setTimeout(() => { btn.textContent = 'Copy'; }, 1200);
        } catch (_) {
          btn.textContent = 'Select manually';
        }
      });
    });

    const hdrAdd = pane.querySelector('#hdr-add');
    if (hdrAdd) {
      hdrAdd.addEventListener('click', () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        provider.headers = (provider.headers || []).concat([{ name: '', value: '' }]);
        render();
      });
    }
    pane.querySelectorAll('[data-hdr-del]').forEach(btn => {
      btn.addEventListener('click', () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        provider.headers.splice(Number(btn.dataset.hdrDel), 1);
        render();
      });
    });

    const modelAdd = pane.querySelector('#prov-model-add-btn');
    if (modelAdd) {
      const commit = () => {
        const provider = editingProvider();
        const field = pane.querySelector('#prov-model-add');
        if (!provider || !field) return;
        const value = field.value.trim();
        if (!value) return;
        collectProvider(provider);
        if (!provider.models.includes(value)) provider.models.push(value);
        // Adding to a list that had none makes this the default.
        if (!provider.model) provider.model = provider.models[0];
        field.value = '';
        render();
      };
      modelAdd.addEventListener('click', commit);
      const field = pane.querySelector('#prov-model-add');
      if (field) {
        field.addEventListener('keydown', (e) => {
          if (e.key === 'Enter') { e.preventDefault(); commit(); }
        });
      }
    }
    pane.querySelectorAll('[data-model-del]').forEach(btn => {
      btn.addEventListener('click', () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        provider.models.splice(Number(btn.dataset.modelDel), 1);
        provider.model = provider.models.length ? provider.models[0] : '';
        render();
      });
    });

    /** Test one model of the provider being edited.
     *
     *  The backend's probe takes the model to call, so a per-model test is the
     *  same request with `model` set to the row's id. That is the difference
     *  between "the endpoint answers" and "this model exists on it": a router can
     *  keep serving three vendors while returning `model_not_found` for the
     *  fourth, and only a request naming that model reveals it. */
    pane.querySelectorAll('[data-model-test]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        const model = providerModels(provider)[Number(btn.dataset.modelTest)];
        if (!model) return;
        setProbe('model', { status: 'busy', text: `Sending one request to ${model}…` });
        try {
          const probe = await invoke('test_provider', {
            provider: {
              id: provider.id || uid('prov'),
              name: provider.name || 'provider',
              kind: provider.kind,
              base_url: provider.base_url,
              api_key: provider.api_key,
              // The whole point: name the model being tested, not the default.
              model,
              models: provider.models || [],
              agent: provider.agent,
              headers: (provider.headers || []).filter(h => h.name.trim()),
            },
          });
          setProbe('model', {
            status: probe.ok ? 'ok' : 'fail',
            // Naming the model in both outcomes, because the box is shared and
            // "the provider answered" would be read as a statement about all of
            // them after testing just one.
            text: probe.ok ? `${model} answered.` : `${model}: ${probe.message}`,
            ms: probe.ms,
          });
        } catch (e) {
          setProbe('model', { status: 'fail', text: `${model}: ${String(e?.message || e)}` });
        }
      });
    });
    pane.querySelectorAll('[data-model-default]').forEach(btn => {
      btn.addEventListener('click', () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        const index = Number(btn.dataset.modelDefault);
        const [picked] = provider.models.splice(index, 1);
        provider.models.unshift(picked);
        provider.model = provider.models[0];
        render();
      });
    });

    const provSave = pane.querySelector('#prov-save');
    if (provSave) {
      provSave.addEventListener('click', () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        const errorBox = pane.querySelector('#prov-error');
        if (!provider.name.trim()) { errorBox.textContent = 'Give the provider a name.'; return; }
        if (!provider.base_url.trim()) { errorBox.textContent = 'Enter the base URL of the endpoint.'; return; }
        if (!provider.models.length) { errorBox.textContent = 'Add at least one model — the chat panel cannot send without one.'; return; }
        provider.name = provider.name.trim();
        // A trailing slash would produce a double slash in the request URL.
        provider.base_url = provider.base_url.trim().replace(/\/+$/, '');
        provider.headers = (provider.headers || []).filter(h => h.name.trim());
        errorBox.textContent = '';
        const index = state.editing.index;
        if (index >= 0) state.draft.providers[index] = provider;
        else state.draft.providers.push(provider);
        if (!state.draft.chat.provider_id) state.draft.chat.provider_id = provider.id;
        state.editing = null;
        state.dirty = true;
        render();
      });
    }

    const provTest = pane.querySelector('#prov-test');
    if (provTest) {
      provTest.addEventListener('click', async () => {
        const provider = editingProvider();
        if (!provider) return;
        collectProvider(provider);
        setProbe('provider', { status: 'busy', text: 'Sending one short request…' });
        try {
          const probe = await invoke('test_provider', {
            provider: {
              id: provider.id || uid('prov'),
              name: provider.name || 'provider',
              kind: provider.kind,
              base_url: provider.base_url,
              api_key: provider.api_key,
              model: provider.model,
              models: provider.models || [],
              agent: provider.agent,
              headers: (provider.headers || []).filter(h => h.name.trim()),
            },
          });
          setProbe('provider', {
            status: probe.ok ? 'ok' : 'fail',
            text: probe.ok ? 'The provider answered.' : probe.message,
            ms: probe.ms,
          });
        } catch (e) {
          setProbe('provider', { status: 'fail', text: String(e?.message || e) });
        }
      });
    }

    // ---- AI Router
    const routerToggle = pane.querySelector('#router-toggle');
    if (routerToggle) {
      routerToggle.addEventListener('click', async () => {
        const want = !routerLive().running;
        routerToggle.disabled = true;
        routerToggle.textContent = want ? 'Starting…' : 'Stopping…';
        try {
          // Persist first: the listener reads its port from the stored settings,
          // so starting before a save would bind the previous port.
          if (state.dirty) {
            const ok = await save();
            if (!ok) return;
          }
          await invoke('set_router_enabled', { enabled: want });
          await refreshRouterLive();
          flash(want ? 'Router listening' : 'Router stopped');
        } catch (e) {
          flash(String(e?.message || e), true);
        } finally {
          render();
        }
      });
    }

    const tunnelToggle = pane.querySelector('#tunnel-toggle');
    if (tunnelToggle) {
      tunnelToggle.addEventListener('click', async () => {
        const want = !routerLive().tunnel.running;
        tunnelToggle.disabled = true;
        tunnelToggle.textContent = want ? 'Starting…' : 'Stopping…';
        try {
          await invoke('set_router_tunnel', { enabled: want });
          await refreshRouterLive();
          const live = routerLive();
          flash(want
            ? (live.tunnel && live.tunnel.url ? `Tunnel up at ${live.tunnel.url}` : 'Tunnel started')
            : 'Tunnel stopped');
        } catch (e) {
          flash(String(e?.message || e), true);
        } finally {
          render();
        }
      });
    }

    const tailscaleToggle = pane.querySelector('#tailscale-toggle');
    if (tailscaleToggle) {
      tailscaleToggle.addEventListener('click', async () => {
        const want = !state.draft.router.tailscale;
        try {
          const stateNow = await invoke('set_router_tailscale', { enabled: want });
          if (want && !stateNow.enabled) {
            flash(stateNow.error || 'Tailscale is not ready.', true);
            return;
          }
          state.draft.router.tailscale = want;
          await refreshRouterLive();
          render();
        } catch (e) {
          flash(String(e?.message || e), true);
        }
      });
    }

    const keyNew = pane.querySelector('#key-new');
    if (keyNew) {
      keyNew.addEventListener('click', () => {
        state.editing = { kind: 'key', index: -1 };
        render();
      });
    }
    pane.querySelectorAll('[data-key-edit]').forEach(btn => {
      btn.addEventListener('click', () => {
        state.editing = { kind: 'key', index: Number(btn.dataset.keyEdit) };
        render();
      });
    });
    pane.querySelectorAll('[data-key-toggle]').forEach(btn => {
      btn.addEventListener('click', () => {
        const i = Number(btn.dataset.keyToggle);
        const key = state.draft.router.keys[i];
        key.enabled = key.enabled === false;
        state.dirty = true;
        render();
      });
    });
    pane.querySelectorAll('[data-key-del]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const i = Number(btn.dataset.keyDel);
        const key = state.draft.router.keys[i];
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Delete API key?',
          text: `"${key.name || 'Untitled key'}" will stop working immediately for every client using it.`,
          okLabel: 'Delete',
        });
        if (!ok) return;
        state.draft.router.keys.splice(i, 1);
        state.editing = null;
        state.dirty = true;
        render();
      });
    });
    const keyCancel = pane.querySelector('#key-cancel');
    if (keyCancel) keyCancel.addEventListener('click', () => { state.editing = null; render(); });
    const keyGenerate = pane.querySelector('#key-generate');
    if (keyGenerate) {
      keyGenerate.addEventListener('click', async () => {
        const field = pane.querySelector('#key-secret');
        if (!field) return;
        try {
          field.value = await invoke('new_router_key');
        } catch (e) {
          flash(String(e?.message || e), true);
        }
      });
    }
    const keySave = pane.querySelector('#key-save');
    if (keySave) {
      keySave.addEventListener('click', async () => {
        const key = state.editing.index >= 0
          ? state.draft.router.keys[state.editing.index]
          : state.editing.draft;
        if (!key) return;
        const errorBox = pane.querySelector('#key-error');
        const name = pane.querySelector('#key-name').value.trim();
        if (!name) { errorBox.textContent = 'Give the key a name, so the log can say which one was used.'; return; }
        key.name = name;
        key.key = pane.querySelector('#key-secret').value.trim();
        if (!key.key) {
          try {
            key.key = await invoke('new_router_key');
          } catch (e) {
            errorBox.textContent = String(e?.message || e);
            return;
          }
        }
        key.models = pane.querySelector('#key-models').value
          .split(/\r?\n/).map(s => s.trim()).filter(Boolean);
        key.enabled = key.enabled !== false;
        errorBox.textContent = '';
        if (state.editing.index >= 0) state.draft.router.keys[state.editing.index] = key;
        else state.draft.router.keys.push(key);
        state.editing = null;
        state.dirty = true;
        render();
      });
    }

    pane.querySelectorAll('input[name="log-limit"]').forEach(r => {
      r.addEventListener('change', () => {
        if (!state.draft.router.log) state.draft.router.log = { enabled: true, limit: 100 };
        state.draft.router.log.limit = Number(r.value);
        state.dirty = true;
        render();
      });
    });

    // ---- AI Combo
    const comboNew = pane.querySelector('#combo-new');
    if (comboNew) {
      comboNew.addEventListener('click', () => {
        state.editing = { kind: 'combo', index: -1 };
        render();
      });
    }
    pane.querySelectorAll('[data-combo-edit]').forEach(btn => {
      btn.addEventListener('click', () => {
        state.editing = { kind: 'combo', index: Number(btn.dataset.comboEdit) };
        render();
      });
    });
    pane.querySelectorAll('[data-combo-toggle]').forEach(btn => {
      btn.addEventListener('click', () => {
        const i = Number(btn.dataset.comboToggle);
        const combo = state.draft.combos[i];
        combo.enabled = combo.enabled === false;
        state.dirty = true;
        render();
      });
    });
    pane.querySelectorAll('[data-combo-del]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const i = Number(btn.dataset.comboDel);
        const combo = state.draft.combos[i];
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Delete combo?',
          text: `"${combo.label || combo.name}" will stop answering as combo/${combo.name}. Clients still asking for it get a 404.`,
          okLabel: 'Delete',
        });
        if (!ok) return;
        state.draft.combos.splice(i, 1);
        state.editing = null;
        state.dirty = true;
        render();
      });
    });
    const comboToggle = pane.querySelector('#combo-cancel');
    if (comboToggle) comboToggle.addEventListener('click', () => { state.editing = null; render(); });

    pane.querySelectorAll('input[name="combo-strategy"]').forEach(r => {
      r.addEventListener('change', () => {
        const combo = editingCombo();
        if (!combo) return;
        collectCombo(combo);
        combo.strategy = r.value;
        render();
      });
    });
    pane.querySelectorAll('[data-combo-member]').forEach(box => {
      box.addEventListener('change', () => {
        const combo = editingCombo();
        if (!combo) return;
        collectCombo(combo);
        const id = (((state.view && state.view.published) || [])[Number(box.dataset.comboMember)] || {}).id;
        if (!id) return;
        const at = combo.members.indexOf(id);
        if (box.checked && at < 0) combo.members.push(id);
        if (!box.checked && at >= 0) combo.members.splice(at, 1);
        render();
      });
    });
    const comboJudge = pane.querySelector('#combo-judge');
    if (comboJudge) {
      comboJudge.addEventListener('change', () => {
        const combo = editingCombo();
        if (!combo) return;
        combo.judge = comboJudge.value;
      });
    }
    const comboLabel = pane.querySelector('#combo-label');
    if (comboLabel) {
      comboLabel.addEventListener('input', () => {
        const combo = editingCombo();
        if (!combo) return;
        combo.label = comboLabel.value;
        // The slug is what a client sends, so the preview has to follow the
        // name as it is typed rather than only after a save.
        const preview = pane.querySelector('#combo-slug');
        if (preview) preview.textContent = `combo/${slugify(comboLabel.value) || 'name'}`;
        state.dirty = true;
      });
    }
    const comboSave = pane.querySelector('#combo-save');
    if (comboSave) {
      comboSave.addEventListener('click', () => {
        const combo = editingCombo();
        if (!combo) return;
        collectCombo(combo);
        const errorBox = pane.querySelector('#combo-error');
        if (!combo.label.trim()) { errorBox.textContent = 'Give the combo a name.'; return; }
        if (!combo.members.length) { errorBox.textContent = 'Pick at least one model — a combo with no members answers nothing.'; return; }
        const slug = slugify(combo.label);
        if (!slug) { errorBox.textContent = 'That name has no letters or digits to build a model id from.'; return; }
        combo.name = slug;
        if (combo.strategy === 'fusion' && !combo.members.includes(combo.judge)) {
          combo.judge = combo.members[0];
        }
        errorBox.textContent = '';
        if (state.editing.index >= 0) state.draft.combos[state.editing.index] = combo;
        else state.draft.combos.push(combo);
        state.editing = null;
        state.dirty = true;
        render();
      });
    }

    // ---- Request logs
    const logsRefresh = pane.querySelector('#logs-refresh');
    if (logsRefresh) {
      logsRefresh.addEventListener('click', refreshLogs);
    }
    // A row opens its detail; the same row closes it again. Delegated from the
    // table rather than bound per row, because the live rows are replaced on
    // every poll tick and a bound handler would go with them.
    const logTable = pane.querySelector('.sp-logtable');
    if (logTable) {
      const toggle = (row) => {
        const key = row.dataset.logRow;
        state.logDetail = state.logDetail === key ? null : key;
        render();
      };
      logTable.addEventListener('click', (event) => {
        const row = event.target.closest('[data-log-row]');
        if (row) toggle(row);
      });
      logTable.addEventListener('keydown', (event) => {
        const row = event.target.closest('[data-log-row]');
        if (row && (event.key === 'Enter' || event.key === ' ')) {
          event.preventDefault();
          toggle(row);
        }
      });
    }
    const logsClear = pane.querySelector('#logs-clear');
    if (logsClear) {
      logsClear.addEventListener('click', async () => {
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Clear the request log?',
          text: 'The list of recorded requests is deleted. The chart, the ranking and the totals stay, because those live in a separate database.',
          okLabel: 'Clear',
        });
        if (!ok) return;
        try {
          await invoke('clear_router_logs');
          state.logDetail = null;
          state.logs = await invoke('router_logs');
        } catch (e) {
          flash(String(e?.message || e), true);
        }
        render();
      });
    }

    // ---- skills
    const skillNew = pane.querySelector('#skill-new');
    if (skillNew) {
      skillNew.addEventListener('click', () => {
        state.editing = { kind: 'skill', index: -1 };
        render();
      });
    }
    pane.querySelectorAll('[data-skill-edit]').forEach(btn => {
      btn.addEventListener('click', () => {
        state.editing = { kind: 'skill', index: Number(btn.dataset.skillEdit) };
        render();
      });
    });
    pane.querySelectorAll('[data-skill-toggle]').forEach(btn => {
      btn.addEventListener('click', () => {
        const i = Number(btn.dataset.skillToggle);
        state.draft.skills[i].enabled = state.draft.skills[i].enabled === false;
        state.dirty = true;
        render();
      });
    });
    pane.querySelectorAll('[data-skill-del]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const i = Number(btn.dataset.skillDel);
        const ok = await window.RustBoxUI.askConfirm({
          title: 'Delete skill?',
          text: `"${state.draft.skills[i].name || 'Untitled skill'}" will stop being added to the prompt.`,
          okLabel: 'Delete',
        });
        if (!ok) return;
        state.draft.skills.splice(i, 1);
        state.editing = null;
        state.dirty = true;
        render();
      });
    });
    const skillCancel = pane.querySelector('#skill-cancel');
    if (skillCancel) skillCancel.addEventListener('click', () => { state.editing = null; render(); });

    const skillSave = pane.querySelector('#skill-save');
    if (skillSave) {
      skillSave.addEventListener('click', () => {
        const errorBox = pane.querySelector('#skill-error');
        const name = pane.querySelector('#skill-name').value.trim();
        const description = pane.querySelector('#skill-desc').value.trim();
        const instructions = pane.querySelector('#skill-text').value;
        const url = pane.querySelector('#skill-url').value.trim();
        if (!name) { errorBox.textContent = 'Give the skill a name.'; return; }
        if (!instructions.trim()) { errorBox.textContent = 'A skill with no instructions does nothing.'; return; }
        const existing = state.editing.index >= 0 ? state.draft.skills[state.editing.index] : null;
        const skill = {
          id: existing ? existing.id : (state.editing.draft ? state.editing.draft.id : uid('skill')),
          name,
          description,
          instructions,
          enabled: existing ? existing.enabled !== false : true,
          source: url ? 'marketplace' : (existing ? existing.source : 'manual'),
          url: url || (existing ? existing.url : null),
        };
        errorBox.textContent = '';
        if (existing) state.draft.skills[state.editing.index] = skill;
        else state.draft.skills.push(skill);
        state.editing = null;
        state.dirty = true;
        render();
      });
    }
    const skillFetch = pane.querySelector('#skill-fetch');
    if (skillFetch) skillFetch.addEventListener('click', fetchSkill);

    // ---- MCP
    const mcpToggle = pane.querySelector('#mcp-toggle');
    if (mcpToggle) {
      mcpToggle.addEventListener('click', async () => {
        const want = !(state.view && state.view.mcp_enabled);
        if (want) {
          const ok = await window.RustBoxUI.askConfirm({
            title: 'Let an agent drive your tabs?',
            text: 'This opens a bridge so an MCP client can read pages, run scripts and take screenshots in the tabs that are open, using the accounts already signed in. It stays on this machine unless you change the reach setting, and every request needs a token from the endpoint file.',
            okLabel: 'Enable',
          });
          if (!ok) return;
        }
        try {
          // Persist first: the listener starts from the stored settings, so
          // enabling before saving would bind the old port.
          if (state.dirty) {
            const ok = await save();
            if (!ok) return;
          }
          await invoke('set_mcp_enabled', { enabled: want });
          state.mcp = await invoke('mcp_state').catch(() => state.mcp);
          state.view.mcp_enabled = want;
          render();
          flash(want ? 'Agent access on' : 'Agent access off');
        } catch (e) {
          flash(String(e?.message || e), true);
        }
      });
    }

    const mcpPublic = pane.querySelector('#mcp-public');
    if (mcpPublic) {
      mcpPublic.addEventListener('click', async () => {
        mcpPublic.disabled = true;
        mcpPublic.textContent = 'Looking up…';
        try {
          state.facts = await invoke('network_facts', { includePublic: true });
        } catch (e) {
          flash(String(e?.message || e), true);
        }
        render();
      });
    }

    // ---- clipboard
    //
    // The browser API is tried first because it is the one the probe stubs, and
    // in a focused window it works. It is not enough on its own: WebView2 rejects
    // `writeText` with `NotAllowedError` whenever the webview's document is not
    // focused, and a Copy button can legitimately be clicked in that state — the
    // click arrives, the promise rejects, and the user gets an error instead of
    // their ids. The backend command goes through Win32, which has no such rule.
    //
    // Order matters for the probe: its stub resolves, so the fallback never runs
    // there and the assertions stay on the browser path.
    const writeClipboard = async (text) => {
      try {
        await navigator.clipboard.writeText(text);
        return true;
      } catch (_) {
        try {
          await invoke('copy_text', { text });
          return true;
        } catch (_) {
          return false;
        }
      }
    };

    pane.querySelectorAll('[data-copy]').forEach(btn => {
      btn.addEventListener('click', async () => {
        const ok = await writeClipboard(btn.dataset.copy);
        btn.textContent = ok ? 'Copied' : 'Select manually';
        if (ok) setTimeout(() => { btn.textContent = 'Copy'; }, 1200);
      });
    });

    // ---- published models: copy one prefix, or everything
    //
    // The ids are rebuilt from `state.view` on click rather than baked into the
    // attribute: a copy button holding a stale list is worse than no button,
    // because the user pastes it and cannot tell it is a day old.
    const copyIds = (ids, btn) => {
      if (!ids.length) return;
      writeClipboard(ids.join('\n')).then(ok => {
        if (!ok) {
          flash('Could not write to the clipboard.', true);
          return;
        }
        const was = btn.textContent;
        btn.textContent = ids.length === 1 ? 'Copied' : `Copied ${ids.length}`;
        setTimeout(() => { btn.textContent = was; }, 1400);
      });
    };
    const publishedIds = () => ((state.view && state.view.published) || []);
    pane.querySelectorAll('[data-copy-group]').forEach(btn => {
      btn.addEventListener('click', () => {
        const group = btn.dataset.copyGroup;
        const ids = publishedIds()
          .filter(p => (group === 'combo' ? p.source === 'combo' : p.source === 'provider' && p.id.split('/')[0] === group))
          .map(p => p.id);
        copyIds(ids, btn);
      });
    });
    pane.querySelectorAll('[data-copy-all]').forEach(btn => {
      btn.addEventListener('click', () => copyIds(publishedIds().map(p => p.id), btn));
    });
    // ---- one combo, from the AI Combo pane
    //
    // `combo/${combo.name}` is the id a client sends, but the name is derived
    // from the label on save, so a combo being edited has no slug yet. Reading it
    // from the draft and slugifying here matches what the backend will publish.
    pane.querySelectorAll('[data-copy-combo]').forEach(btn => {
      btn.addEventListener('click', () => {
        const combo = (state.draft.combos || [])[Number(btn.dataset.copyCombo)];
        if (!combo) return;
        const name = combo.name || slugify(combo.label || '');
        if (!name) {
          flash('Give the combo a name first — the id is combo/name.', true);
          return;
        }
        copyIds([`combo/${name}`], btn);
      });
    });

    // ---- general
    if (state.tab === 'general') {
      const host = pane.querySelector('#sp-paths');
      if (host) {
        invoke('data_paths').then(paths => {
          host.innerHTML = `Config: <code>${esc(paths.config)}</code><br>Profiles: <code>${esc(paths.profiles)}</code><br>Root: <code>${esc(paths.root)}</code>`;
        }).catch(() => { host.textContent = 'Could not read the data folder.'; });
      }
    }
  }

  /** Read the provider editor's fields into the object being edited. */
  function collectProvider(provider) {
    const read = (sel, key) => {
      const el = $(sel);
      if (el) provider[key] = el.value;
    };
    read('#prov-name', 'name');
    read('#prov-kind', 'kind');
    read('#prov-base', 'base_url');
    read('#prov-agent', 'agent');
    read('#prov-key', 'api_key');
    read('#prov-prefix', 'prefix');
    provider.prefix = slugify(provider.prefix || '');
    const price = (sel, key) => {
      const el = $(sel);
      if (el) provider[key] = Math.max(0, Number(el.value) || 0);
    };
    price('#prov-input-price', 'input_price');
    price('#prov-output-price', 'output_price');
    // The model list is the authority; `model` mirrors its first entry so the
    // backend has one obvious default and cannot drift from the list.
    const models = (provider.models || []).filter(m => String(m).trim());
    provider.models = models;
    provider.model = models.length ? String(models[0]).trim() : '';
    const rows = document.querySelectorAll('[data-header]');
    if (rows.length || (provider.headers || []).length) {
      const headers = [];
      rows.forEach(row => {
        const name = row.querySelector('.hdr-name').value.trim();
        const value = row.querySelector('.hdr-value').value;
        if (name) headers.push({ name, value });
      });
      provider.headers = headers;
    }
  }

  /** Read the combo editor's fields into the object being edited.
   *
   *  The name is derived from the label on save, not typed separately: a combo
   *  has one visible name, and a second "slug" field would be a second thing to
   *  keep in step for no benefit. */
  function collectCombo(combo) {
    const label = $('#combo-label');
    if (label) combo.label = label.value;
    const judge = $('#combo-judge');
    if (judge) combo.judge = judge.value;
    combo.members = (combo.members || []).filter(m => String(m).trim());
  }

  /** Read the combo editor's fields into the object being edited.
   *
   *  The name is derived from the label on save, not typed separately: a combo
   *  has one visible name, and a second "slug" field would be a second thing to
   *  keep in step for no benefit. */
  function collectCombo(combo) {
    const label = $('#combo-label');
    if (label) combo.label = label.value;
    const judge = $('#combo-judge');
    if (judge) combo.judge = judge.value;
    combo.members = (combo.members || []).filter(m => String(m).trim());
  }

  /** Load a SKILL.md into the editor.
   *
   *  Fetched in the page: it is one unauthenticated document with no side effect,
   *  and the result is text the user reviews before saving. A failure is reported
   *  verbatim rather than guessed at. */
  async function fetchSkill() {
    const input = $('#skill-url');
    const note = $('#skill-fetch-note');
    const raw = (input.value || '').trim();
    if (!raw) { note.textContent = 'Paste a URL or owner/repo first.'; return; }

    let url = raw;
    if (!/^https?:\/\//i.test(raw)) {
      if (!/^[\w.-]+\/[\w.-]+$/.test(raw)) {
        note.textContent = 'That is neither a URL nor an owner/repo pair.';
        return;
      }
      url = `https://raw.githubusercontent.com/${raw}/HEAD/SKILL.md`;
    }

    note.textContent = `Fetching ${url}…`;
    let text;
    try {
      const response = await fetch(url, { headers: { accept: 'text/plain' } });
      if (!response.ok) throw new Error(`the server answered ${response.status}`);
      text = await response.text();
    } catch (e) {
      note.textContent = `Could not read ${url}: ${e?.message || e}. Paste the instructions by hand instead.`;
      return;
    }

    // Front matter is optional; when present it names the skill and says what it
    // is for, which is exactly the two fields the form asks for.
    let name = '';
    let description = '';
    let body = text;
    const fm = text.match(/^---\r?\n([\s\S]*?)\r?\n---\r?\n?/);
    if (fm) {
      body = text.slice(fm[0].length);
      for (const line of fm[1].split(/\r?\n/)) {
        const m = line.match(/^(\w[\w-]*)\s*:\s*(.*)$/);
        if (!m) continue;
        const value = m[2].replace(/^['"]|['"]$/g, '').trim();
        if (m[1].toLowerCase() === 'name') name = value;
        if (m[1].toLowerCase() === 'description') description = value;
      }
    }
    if (!name) {
      const heading = body.match(/^#\s+(.+)$/m);
      name = heading ? heading[1].trim() : (url.split('/').slice(-2, -1)[0] || 'Skill');
    }

    $('#skill-name').value = name;
    $('#skill-desc').value = description;
    $('#skill-text').value = body.trim();
    note.textContent = `Loaded ${body.length.toLocaleString()} characters from ${url}. Review it, then save.`;
  }

  // ---- boot ----------------------------------------------------------------

  function boot() {
    const page = $('#settings-page');
    if (!page) return;
    page.querySelector('#sp-close').addEventListener('click', close);
    page.querySelector('#sp-save').addEventListener('click', () => save());
    document.addEventListener('keydown', (e) => {
      if (!state.open) return;
      if (e.key === 'Escape') {
        e.stopPropagation();
        // An open inline editor owns Escape: closing it is the smaller action.
        if (state.editing) { state.editing = null; render(); return; }
        close();
      }
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 's') {
        e.preventDefault();
        save();
      }
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }

  window.RustBoxSettings = { open, close, isOpen: () => state.open };
})();
