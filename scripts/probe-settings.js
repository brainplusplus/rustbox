// Injected by scripts/probe.ps1 into a temporary copy of ui/index.html.
//
// Covers the two surfaces added for Phase 16:
//
//   Settings  a full-height page with six sections, a rail that states the live
//             value of each one, and draft-based editing that survives a
//             re-render. The checks are about the properties that break
//             silently: the rail has to name the real value, a masked key must
//             never be re-sent as if it were the key, and the MCP guides have to
//             change when the bind mode does.
//
//   Chat      a right-hand column that shrinks #content so the tab windows
//             follow it, a provider picker built from the masked rows rather
//             than the raw providers, and a send path that shows the user's turn
//             before the round trip.
//
// The tab windows are OS windows, so what "opening a surface" means here is
// hide_all_tabs + a new #content rect. The stub records both.
(function () {
  'use strict';

  const results = [];
  // Written after every check, not once at the end: a probe that hangs half way
  // through otherwise reports nothing at all, which hides which stage hung.
  let stage = 'boot';
  let finished = false;
  const flush = () => {
    const sink = document.getElementById('probe');
    if (!sink) return;
    const rows = results.map(
      (r) => `${r.ok ? 'PASS' : 'FAIL'}|${r.name}|${String(r.detail).replace(/\|/g, '/')}`
    );
    // A trailing marker rather than an empty sink: "stopped here" is a real
    // failure the runner can report, where "no rows at all" is indistinguishable
    // from a broken harness.
    if (!finished) rows.push(`FAIL|probe stopped during: ${stage}||`);
    sink.textContent = rows.join('\n');
  };
  const log = (name, ok, detail) => {
    results.push({ name, ok: !!ok, detail: detail === undefined ? '' : String(detail) });
    flush();
  };
  const at = (name) => { stage = name; };

  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const until = async (fn, ms = 6000) => {
    const t0 = Date.now();
    while (Date.now() - t0 < ms) {
      try { if (fn()) return true; } catch (_) {}
      await wait(40);
    }
    return false;
  };

  const q = (s) => document.querySelector(s);
  const qa = (s) => [...document.querySelectorAll(s)];
  const fire = (el, type) => el.dispatchEvent(new Event(type, { bubbles: true }));
  const type = (el, value) => { el.value = value; fire(el, 'input'); };

  const hidden = () => q('#settings-page').classList.contains('hidden');
  const calls = (name) => (window.__probeCalls || []).filter((c) => c === name).length;
  const railStat = (id) =>
    (q(`.sp-rail-item[data-tab="${id}"] .st`) || { textContent: '' }).textContent.trim();
  const confirmOpen = () => {
    const b = q('#confirm-backdrop');
    return !!b && b.classList.contains('open');
  };

  /** Close the settings page, answering the discard question if it appears.
   *
   *  Closing with unsaved edits asks first, and that dialog is a promise the
   *  page waits on forever if nobody answers it — which is exactly what a probe
   *  that calls close() and forgets the dialog does. */
  async function closeSettings() {
    const done = window.RustBoxSettings.close();
    let asked = false;
    for (let i = 0; i < 80; i++) {
      await wait(25);
      if (confirmOpen()) { q('#confirm-ok').click(); asked = true; break; }
      if (hidden()) break;
    }
    await done;
    await until(() => hidden(), 4000);
    return asked;
  }

  /** Click a sidebar surface button and wait for the page to come up. */
  async function openSettings(tab) {
    if (hidden()) {
      q('#settings-btn').click();
      await until(() => !hidden(), 6000);
    }
    if (tab) {
      q(`.sp-rail-item[data-tab="${tab}"]`).click();
      await wait(80);
    }
  }

  async function run() {
    try {
      at('module load');
      // The shell boots asynchronously; the sidebar buttons exist immediately but
      // the app list has to settle before a surface is opened over it.
      await until(() => typeof window.RustBoxSettings === 'object', 9000);
      log('settings module is loaded', !!window.RustBoxSettings);
      log('chat module is loaded', !!window.RustBoxChat);
      log('the overlay bridge is exposed', !!window.RustBoxUI
        && typeof window.RustBoxUI.hideTabs === 'function'
        && typeof window.RustBoxUI.restoreAfterOverlay === 'function'
        && typeof window.RustBoxUI.relayout === 'function'
        && typeof window.RustBoxUI.askConfirm === 'function');

      // ---- settings: opening hides the tab windows ----------------------
      at('settings open');
      const hiddenBefore = calls('hide_all_tabs');
      await window.RustBoxSettings.open();
      await until(() => !hidden(), 6000);
      const hiddenAfter = calls('hide_all_tabs');
      log('opening settings hides the tab windows', hiddenAfter > hiddenBefore,
        `${hiddenBefore} -> ${hiddenAfter}`);
      log('settings page is visible', !hidden());
      log('the settings button reports the surface is up',
        q('#settings-btn').classList.contains('on'));

      // ---- settings: six sections, each with a live value -----------------
      at('settings rail');
      const rail = qa('.sp-rail-item');
      log('the rail lists all nine sections', rail.length === 9, rail.length + ' rows');
      const labels = rail.map((r) => r.querySelector('.lbl').textContent.trim());
      log('the sections are the ones the brief asked for',
        labels.join(',') === 'General,Global proxy,Proxy pool,AI provider,AI Router,AI Combo,Request logs,MCP,Skills',
        labels.join(','));
      const stats = rail.map((r) => r.querySelector('.st').textContent.trim());
      log('every rail row states a value', stats.every((s) => s.length > 0), stats.join(' | '));
      // The global proxy stub is a fixed endpoint, so the rail has to name it
      // rather than say "custom".
      log('the proxy row names the real endpoint',
        /10\.9\.8\.7:3128/.test(stats[1]), stats[1]);
      log('the MCP row reads off while the bridge is off', stats[7] === 'off', stats[7]);
      log('the pool row counts pools, not members', stats[2] === '1', stats[2]);
      log('the router row says the listener is off', stats[4] === 'off', stats[4]);
      log('the combo row counts combos', stats[5] === '1', stats[5]);
      log('the log row counts requests', stats[6] === '3 req', stats[6]);
      log('the log row turns amber when something failed',
        q('.sp-rail-item[data-tab="logs"] .st').classList.contains('warn'),
        q('.sp-rail-item[data-tab="logs"] .st').className);

      // ---- settings: every pane renders -----------------------------------
      at('panes');
      for (const [id, needle] of [
        ['general', 'Theme'],
        ['proxy', 'Direct connection'],
        ['pools', 'Proxy pools'],
        ['ai', 'AI provider'],
        ['router', 'AI Router'],
        ['combo', 'AI Combo'],
        ['logs', 'Request logs'],
        ['mcp', 'MCP bridge'],
        ['skills', 'Skills'],
      ]) {
        q(`.sp-rail-item[data-tab="${id}"]`).click();
        await wait(60);
        const text = q('#sp-pane').textContent;
        log(`the ${id} pane renders`, text.includes(needle), text.slice(0, 60).replace(/\s+/g, ' '));
      }

      // ---- settings: MCP guides follow the bind mode ----------------------
      // One guide on loopback, three on a published port: a client on this
      // machine, one on the LAN and one outside all need a different address.
      at('mcp guides');
      q('.sp-rail-item[data-tab="mcp"]').click();
      await wait(80);
      const loopbackGuides = qa('#sp-pane .sp-code').length;
      const networkRadio = q('#sp-pane input[value="network"]');
      log('a loopback bridge shows one client guide', loopbackGuides === 1, loopbackGuides + ' guides');
      networkRadio.checked = true;
      fire(networkRadio, 'change');
      await wait(80);
      const networkGuides = qa('#sp-pane .sp-code').length;
      log('a published bridge shows three client guides', networkGuides === 3, networkGuides + ' guides');
      log('the guides warn about the wider reach',
        /network you trust/i.test(q('#sp-pane').textContent));
      const loopbackRadio = q('#sp-pane input[value="loopback"]');
      loopbackRadio.checked = true;
      fire(loopbackRadio, 'change');
      await wait(80);

      // ---- settings: theme applies immediately ---------------------------
      at('theme');
      q('.sp-rail-item[data-tab="general"]').click();
      await wait(80);
      const light = q('#sp-pane input[value="light"]');
      light.checked = true;
      fire(light, 'change');
      await wait(80);
      log('choosing Light repaints without a save',
        document.documentElement.dataset.theme === 'light',
        document.documentElement.dataset.theme);
      log('the choice is remembered for the next launch',
        localStorage.getItem('rustbox.theme') === 'light', localStorage.getItem('rustbox.theme'));
      const dark = q('#sp-pane input[value="dark"]');
      dark.checked = true;
      fire(dark, 'change');
      await wait(80);
      log('choosing Dark repaints back', document.documentElement.dataset.theme === 'dark',
        document.documentElement.dataset.theme);

      // ---- settings: closing restores the tab windows --------------------
      at('settings close');
      const restoredBefore = calls('resize_tabs');
      const asked = await closeSettings();
      const restoredAfter = calls('resize_tabs');
      log('closing with unsaved edits asks before dropping them', asked);
      log('closing settings gives the tab area back', restoredAfter >= restoredBefore,
        `${restoredBefore} -> ${restoredAfter}`);
      log('the settings button clears its state',
        !q('#settings-btn').classList.contains('on'));

      // ---- settings: a masked key is never re-sent as the key -------------
      // save_settings keeps the stored secret when it sees the mask, so the page
      // must send the mask back rather than an empty string or a stripped one.
      // The stub seeds one provider with the key "****1234" for exactly this.
      await openSettings('ai');
      q('[data-prov-edit="0"]').click();
      await wait(80);
      const keyField = q('#prov-key');
      log('the stored key arrives masked', !!keyField && keyField.value === '****1234',
        keyField && keyField.value);
      log('the key field is a password input', !!keyField && keyField.type === 'password',
        keyField && keyField.type);
      // The preset list is the backend's, not a copy in the page: a preset the
      // page offers but the backend does not implement would send no headers at
      // all, and the hint is the only thing that says what a choice does. The
      // stub's hints are distinguishable, so the fallback cannot pass for it.
      const agentPicker = q('#prov-agent');
      const agentIds = qa('#prov-agent option').map(o => o.value);
      log('the agent picker is built from the backend preset list',
        !!agentPicker && agentIds.join(',') === 'plain,opencode,oh-my-pi,claude-code,codex',
        agentIds.join(','));
      log('the preset hint comes from the backend, not the page fallback',
        q('#prov-agent-hint').textContent.trim() === 'stub: plain',
        q('#prov-agent-hint').textContent.trim());
      agentPicker.value = 'codex';
      fire(agentPicker, 'change');
      await wait(60);
      log('the hint follows the chosen preset',
        q('#prov-agent-hint').textContent.trim() === 'stub: codex',
        q('#prov-agent-hint').textContent.trim());
      q('#prov-cancel').click();
      await wait(80);

      q('#prov-new').click();
      await wait(80);
      q('#prov-name').value = 'Probe provider';
      q('#prov-base').value = 'https://api.example.com/v1';
      // A provider takes several models; the first added becomes the default.
      q('#prov-model-add').value = 'probe-model';
      q('#prov-model-add-btn').click();
      await wait(80);
      q('#prov-model-add').value = 'probe-model-2';
      q('#prov-model-add-btn').click();
      await wait(80);
      const modelRows = [...document.querySelectorAll('[data-model-row] .i-name')].map(n => n.textContent.trim());
      log('the editor lists every model added to the provider',
        modelRows.join(',') === 'probe-model,probe-model-2', modelRows.join(','));
      log('the first model is marked as the default',
        (q('[data-model-row] .i-meta') || {}).textContent.trim() === 'default for new conversations',
        (q('[data-model-row] .i-meta') || {}).textContent);
      q('[data-model-default="1"]').click();
      await wait(80);
      const reordered = [...document.querySelectorAll('[data-model-row] .i-name')].map(n => n.textContent.trim());
      log('making a model the default moves it to the front',
        reordered[0] === 'probe-model-2', reordered.join(','));
      q('#prov-save').click();
      await wait(80);
      const before = (window.__probeSaves || []).length;
      q('#sp-save').click();
      await until(() => (window.__probeSaves || []).length > before, 5000);
      const sent = (window.__probeSaves || [])[before];
      const sentProviders = (sent && sent.providers) || [];
      const added = sentProviders.find((p) => p.name === 'Probe provider');
      const seeded = sentProviders.find((p) => p.name === 'Probe Cloud');
      log('saving sends the whole settings block once',
        (window.__probeSaves || []).length === before + 1,
        `${before} -> ${(window.__probeSaves || []).length}`);
      log('the editor adds to the list instead of replacing it',
        sentProviders.length === 2, sentProviders.map((p) => p.name).join(', '));
      log('the new provider keeps its base URL',
        added && added.base_url === 'https://api.example.com/v1',
        added && added.base_url);
      log('the new provider keeps every model, in the order shown',
        added && Array.isArray(added.models) && added.models.join(',') === 'probe-model-2,probe-model',
        added && JSON.stringify(added.models));
      log('the provider default model follows the list order',
        added && added.model === 'probe-model-2', added && added.model);
      log('the seeded provider keeps its three models',
        seeded && Array.isArray(seeded.models) && seeded.models.length === 3,
        seeded && JSON.stringify(seeded.models));
      log('the untouched key goes back as the mask, not as a blank',
        seeded && seeded.api_key === '****1234', seeded && seeded.api_key);
      log('every provider carries the full proxy shape a reload needs',
        !!(sent && sent.global_proxy && sent.global_proxy.mode),
        sent && JSON.stringify(sent.global_proxy));
      log('the proxy credentials survive a save',
        sent && sent.global_proxy.custom && sent.global_proxy.custom.host === '10.9.8.7',
        sent && JSON.stringify(sent.global_proxy.custom));

      // ---- settings: a provider publishes under a prefix ------------------
      // The prefix is the routing key: `risun` plus a model of
      // `bansos/deepseek-v4.1` is called as `risun/bansos/deepseek-v4.1`. The
      // editor has to show that full id, or a client config is guesswork.
      const addedProvider = sentProviders.find((p) => p.name === 'Probe provider');
      log('a new provider keeps the prefix it was given',
        addedProvider && addedProvider.prefix === '', JSON.stringify(addedProvider && addedProvider.prefix));
      q('[data-prov-edit="0"]').click();
      await wait(80);
      log('the editor offers a prefix field', !!q('#prov-prefix'));
      log('the prefix field is prefilled from the provider',
        q('#prov-prefix').value === 'probe-cloud', q('#prov-prefix').value);
      log('the preview shows the full model id a client would send',
        /^probe-cloud\/probe-large/.test(q('#prov-prefix-preview').textContent.trim()),
        q('#prov-prefix-preview').textContent.trim());
      type(q('#prov-prefix'), 'Risun Cloud');
      await wait(120);
      log('typing a prefix slugifies it for the preview',
        /^risun-cloud\/probe-large/.test(q('#prov-prefix-preview').textContent.trim()),
        q('#prov-prefix-preview').textContent.trim());
      type(q('#prov-prefix'), 'combo');
      await wait(200);
      log('the reserved prefix is refused in the note',
        /reserved/i.test(q('#prov-prefix-note').textContent),
        q('#prov-prefix-note').textContent.trim().slice(0, 80));
      type(q('#prov-prefix'), 'probe-cloud');
      await wait(120);
      q('#prov-cancel').click();
      await wait(80);

      // ---- settings: the MCP switch is one round trip ---------------------
      // Turning the bridge on is a confirmation, because it opens a socket that
      // can drive signed-in tabs. The probe answers it the way a user would.
      await openSettings('mcp');
      const mcpBefore = (window.__probeMcpCalls || []).length;
      q('#mcp-toggle').click();
      await until(confirmOpen, 4000);
      log('turning the bridge on asks first', confirmOpen(), 'confirm dialog');
      q('#confirm-ok').click();
      await until(() => (window.__probeMcpCalls || []).length > mcpBefore, 5000);
      log('the switch turns the bridge on without a save',
        (window.__probeMcpCalls || [])[mcpBefore] === true,
        JSON.stringify(window.__probeMcpCalls || []));
      await until(() => railStat('mcp') === ':7420', 4000);
      log('the rail picks up the live port', railStat('mcp') === ':7420', railStat('mcp'));
      q('#mcp-toggle').click();
      await until(() => (window.__probeMcpCalls || []).length > mcpBefore + 1, 5000);
      log('turning it off needs no confirmation',
        (window.__probeMcpCalls || [])[mcpBefore + 1] === false,
        JSON.stringify(window.__probeMcpCalls || []));
      await until(() => railStat('mcp') === 'off', 4000);
      log('the rail reads off again', railStat('mcp') === 'off', railStat('mcp'));

      // ---- settings: the router's reach strip is the point of the tab -----
      // What a client puts in its config is the one question this tab exists to
      // answer, so the strip has to name a real endpoint before anything else.
      await openSettings('router');
      const strip = q('#sp-pane .sp-endpoint');
      log('the router pane leads with a reach strip', !!strip);
      log('the strip names the loopback endpoint a client would paste',
        /http:\/\/127\.0\.0\.1:11930\/v1/.test(strip.textContent),
        strip.textContent.replace(/\s+/g, ' ').slice(0, 90));
      log('a stopped router still shows the endpoint, marked stopped',
        /Stopped/.test(strip.textContent), strip.textContent.replace(/\s+/g, ' ').slice(0, 40));
      log('an unpublished router offers only the loopback route',
        qa('#sp-pane .ep-row').length === 1, qa('#sp-pane .ep-row').length + ' routes');
      log('the strip counts what is published',
        /5 models and 1 combo published/.test(strip.textContent),
        (strip.textContent.match(/\d+ models?[^.]*/) || [''])[0]);
      log('the port field shows the configured port',
        q('#sp-pane input[data-path="router.port"]').value === '11930',
        q('#sp-pane input[data-path="router.port"]').value);

      const routerBefore = (window.__probeRouterCalls || []).length;
      q('#router-toggle').click();
      await until(() => (window.__probeRouterCalls || []).length > routerBefore, 20000);
      await until(() => /Stop router/.test(q('#router-toggle').textContent), 20000);
      log('starting the router reaches the backend',
        (window.__probeRouterCalls || [])[routerBefore] === true,
        JSON.stringify(window.__probeRouterCalls || []));
      await until(() => railStat('router') === ':11930', 20000);
      log('the rail picks up the live port', railStat('router') === ':11930', railStat('router'));
      log('the strip switches to Listening', /Listening/.test(q('#sp-pane .sp-endpoint').textContent));
      q('#router-toggle').click();
      await until(() => (window.__probeRouterCalls || []).length > routerBefore + 1, 20000);
      await until(() => /Start router/.test(q('#router-toggle').textContent), 20000);
      log('stopping the router reaches the backend',
        (window.__probeRouterCalls || [])[routerBefore + 1] === false,
        JSON.stringify(window.__probeRouterCalls || []));
      await until(() => railStat('router') === 'off', 20000);
      log('the rail reads off again', railStat('router') === 'off', railStat('router'));

      // The reach strip reports where the router is *now*, not where it would be
      // after a save. Publishing only widens the bind when the listener restarts,
      // so the LAN route must not appear until the change has been saved —
      // otherwise the page would name an address that does not answer yet.
      const publishBox = q('#sp-pane input[data-path="router.publish"]');
      publishBox.checked = true;
      fire(publishBox, 'change');
      await wait(80);
      log('an unsaved publish does not invent a LAN route yet',
        qa('#sp-pane .ep-row').length === 1, qa('#sp-pane .ep-row').length + ' routes');
      const publishSave = (window.__probeSaves || []).length;
      q('#sp-save').click();
      await until(() => (window.__probeSaves || []).length > publishSave, 20000);
      await wait(80);
      log('publishing adds the same-network route once saved',
        qa('#sp-pane .ep-row').length === 2, qa('#sp-pane .ep-row').length + ' routes');
      log('the LAN route uses the machine address, not 127.0.0.1',
        /192\.168\.1\.42:11930/.test(q('#sp-pane').textContent));
      const publishOff = q('#sp-pane input[data-path="router.publish"]');
      publishOff.checked = false;
      fire(publishOff, 'change');
      await wait(80);
      const unpublishSave = (window.__probeSaves || []).length;
      q('#sp-save').click();
      await until(() => (window.__probeSaves || []).length > unpublishSave, 20000);
      await wait(80);
      log('unpublishing takes the LAN route away',
        qa('#sp-pane .ep-row').length === 1, qa('#sp-pane .ep-row').length + ' routes');

      // ---- settings: the published list is copyable -----------------------
      //
      // The ids are what a client config has to contain, so a Copy button is the
      // difference between reading them off the screen and getting them right.
      const groups = qa('#sp-pane .sp-prefix-group');
      log('published models are grouped, with the combos as their own group',
        groups.length >= 2 && !!q('#sp-pane .combo-group'),
        groups.length + ' groups');
      log('every group has a Copy button',
        qa('#sp-pane .sp-prefix-group .pg-copy').length === groups.length,
        qa('#sp-pane .sp-prefix-group .pg-copy').length + ' buttons');
      // A button that exists but is laid out somewhere else cannot be clicked.
      // `.sp-copy` is absolutely positioned for the guide blocks it was written
      // for; inside a prefix group it has to sit in the row as a flex child.
      // Assert the geometry, because a `.click()` in a probe bypasses layout.
      const groupButtons = qa('#sp-pane .sp-prefix-group .pg-copy');
      const misplaced = groupButtons.filter(btn => {
        const row = btn.closest('.sp-prefix-group').getBoundingClientRect();
        const box = btn.getBoundingClientRect();
        return box.width === 0 || box.height === 0
          || box.left < row.left - 1 || box.right > row.right + 1
          || box.top < row.top - 1 || box.bottom > row.bottom + 1;
      });
      log('each group Copy button sits inside its own row',
        misplaced.length === 0,
        misplaced.length + ' of ' + groupButtons.length + ' outside their row');
      log('the group Copy buttons are not stacked on one spot',
        new Set(groupButtons.map(b => Math.round(b.getBoundingClientRect().top))).size === groupButtons.length,
        groupButtons.map(b => Math.round(b.getBoundingClientRect().top)).join(','));
      log('the combo group lists the combo ids the client sends',
        /combo\/fast/.test(q('#sp-pane .combo-group').textContent),
        q('#sp-pane .combo-group').textContent.replace(/\s+/g, ' ').trim().slice(0, 60));

      (window.__probeClipboard || []).length = 0;
      const firstPrefix = q('#sp-pane .sp-prefix-group .pg-head code').textContent.replace('/', '');
      q(`#sp-pane [data-copy-group="${firstPrefix}"]`).click();
      await until(() => (window.__probeClipboard || []).length > 0, 4000);
      const copied = (window.__probeClipboard[0] || '').split('\n');
      log('a group copies one full id per line, prefix included',
        copied.length > 0 && copied.every(id => id.startsWith(firstPrefix + '/')),
        JSON.stringify(copied));
      // The label is set once the write resolves, and the clipboard is written
      // before that promise settles — so a read of the label right after the
      // write lands can still catch the old text. Wait for the settled DOM.
      await until(() => /Copied/.test(q(`#sp-pane [data-copy-group="${firstPrefix}"]`).textContent), 4000);
      log('the copy button says how many ids it copied',
        /Copied/.test(q(`#sp-pane [data-copy-group="${firstPrefix}"]`).textContent),
        q(`#sp-pane [data-copy-group="${firstPrefix}"]`).textContent);

      (window.__probeClipboard || []).length = 0;
      q('#sp-pane [data-copy-group="combo"]').click();
      await until(() => (window.__probeClipboard || []).length > 0, 4000);
      log('copying the combo group yields the combo ids, not the member models',
        window.__probeClipboard[0] === 'combo/fast',
        JSON.stringify(window.__probeClipboard[0] || null));

      (window.__probeClipboard || []).length = 0;
      q('#sp-pane [data-copy-all]').click();
      await until(() => (window.__probeClipboard || []).length > 0, 4000);
      const all = (window.__probeClipboard[0] || '').split('\n');
      log('Copy all covers every published id, combos included',
        all.length === (window.__probePublished() || []).length
        && all.some(id => id.startsWith('combo/')),
        `${all.length} ids: ${all.join(', ')}`);
      // ---- settings: copy survives an unfocused document -------------------
      // WebView2 rejects `navigator.clipboard` with NotAllowedError whenever the
      // document does not hold focus, and a Copy button can be clicked in that
      // state. The page has to fall back to the backend command, or the user gets
      // an error toast for a click that was delivered correctly.
      window.__probeClipboardDeny = true;
      window.__probeNativeClipboard = [];
      (window.__probeClipboard || []).length = 0;
      const allBtn = q('#sp-pane [data-copy-all]');
      allBtn.click();
      await until(() => (window.__probeNativeClipboard || []).length > 0, 4000);
      log('a rejected browser copy falls back to the native clipboard',
        (window.__probeNativeClipboard || []).length === 1,
        `${(window.__probeNativeClipboard || []).length} native write(s)`);
      log('the fallback copies the same ids the browser path would have',
        (window.__probeNativeClipboard[0] || '') === all.join('\n'),
        JSON.stringify((window.__probeNativeClipboard[0] || '').split('\n').length) + ' ids');
      // The label is set after the write resolves, so waiting on the write alone
      // can still catch the button before its promise chain has finished.
      await until(() => /Copied/.test(allBtn.textContent), 4000);
      log('the button still confirms the copy when the fallback carried it',
        /Copied/.test(allBtn.textContent),
        allBtn.textContent);
      window.__probeClipboardDeny = false;

      // ---- settings: a tunnel is started, not saved ------------------------
      // Headless Chrome runs on a virtual clock, so a 4s poll can expire in
      // milliseconds of real time — before the click's promise chain has even
      // reached its first await. These waits are budgeted generously and then
      // assert on the settled DOM, not on a timer.
      const tunnelBefore = (window.__probeTunnelCalls || []).length;
      q('#tunnel-toggle').click();
      await until(() => (window.__probeTunnelCalls || []).length > tunnelBefore, 20000);
      await until(() => q('#tunnel-toggle') && /Stop tunnel/.test(q('#tunnel-toggle').textContent), 20000);
      log('starting the tunnel reaches the backend',
        (window.__probeTunnelCalls || [])[tunnelBefore] === true,
        JSON.stringify(window.__probeTunnelCalls || []));
      log('the tunnel prints the public /v1 endpoint it got',
        /https:\/\/calm-river-1234\.trycloudflare\.com\/v1/.test(q('#sp-pane').textContent),
        (q('#sp-pane .sp-endpoint').textContent.match(/https:\/\/\S+/g) || ['none']).join(' '));
      log('the public route joins the reach strip',
        qa('#sp-pane .ep-row').length === 2, qa('#sp-pane .ep-row').length + ' routes');
      log('the tunnel exposes a stop control once it is up',
        q('#tunnel-toggle').textContent.trim() === 'Stop tunnel',
        q('#tunnel-toggle').textContent.trim());

      // ---- settings: restricted access is per key --------------------------
      log('every key lists its rules', qa('#sp-pane [data-key-edit]').length === 2,
        qa('#sp-pane [data-key-edit]').length + ' keys');
      q('[data-key-edit="0"]').click();
      await wait(80);
      log('editing a key opens its editor', !!q('#key-models'));
      log('the editor shows the stored rules, one per line',
        q('#key-models').value === 'probe-cloud/*', JSON.stringify(q('#key-models').value));
      log('the editor lists what is published, so a rule can be checked against it',
        /probe-cloud\/probe-large/.test(q('#sp-pane .sp-rule-hint').textContent),
        q('#sp-pane .sp-rule-hint').textContent.replace(/\s+/g, ' ').slice(0, 70));
      q('#key-cancel').click();
      await wait(80);

      // ---- settings: a combo answers under the reserved prefix ------------
      await openSettings('combo');
      log('the combo list shows the saved combo',
        /Fast fallback/.test(q('#sp-pane').textContent));
      log('a combo row names its full model id',
        /combo\/fast/.test(q('#sp-pane').textContent));
      q('#combo-new').click();
      await wait(80);
      q('#combo-label').value = 'Deepseek flash';
      fire(q('#combo-label'), 'input');
      await wait(80);
      log('the editor previews the id the client will send',
        q('#combo-slug').textContent.trim() === 'combo/deepseek-flash',
        q('#combo-slug').textContent.trim());
      log('the strategies come from the backend, not the page',
        qa('#sp-pane input[name="combo-strategy"]').length === 3
        && /stub: try each in order/.test(q('#sp-pane').textContent),
        qa('#sp-pane input[name="combo-strategy"]').length + ' strategies');
      const memberBoxes = qa('#sp-pane [data-combo-member]');
      log('every published model is offered as a member', memberBoxes.length === 6,
        memberBoxes.length + ' candidates');
      memberBoxes[0].checked = true;
      fire(memberBoxes[0], 'change');
      await wait(80);
      log('picking a member numbers it as priority #1',
        /#1/.test(q('#sp-pane').textContent));
      const fusion = q('#sp-pane input[name="combo-strategy"][value="fusion"]');
      fusion.checked = true;
      fire(fusion, 'change');
      await wait(80);
      log('choosing Fusion asks which model judges', !!q('#combo-judge'));
      const roundRobin = q('#sp-pane input[name="combo-strategy"][value="round-robin"]');
      roundRobin.checked = true;
      fire(roundRobin, 'change');
      await wait(80);
      log('leaving Fusion drops the judge picker', !q('#combo-judge'));
      const comboBefore = (window.__probeSaves || []).length;
      q('#combo-save').click();
      await wait(80);
      log('saving a combo adds it to the list',
        qa('#sp-pane [data-combo-edit]').length === 2,
        qa('#sp-pane [data-combo-edit]').length + ' combos');
      q('#sp-save').click();
      await until(() => (window.__probeSaves || []).length > comboBefore, 20000);
      const comboSent = (window.__probeSaves || [])[comboBefore] || {};
      const addedCombo = (comboSent.combos || []).find(c => c.name === 'deepseek-flash');
      log('the combo is saved under its slugged name', !!addedCombo,
        JSON.stringify((comboSent.combos || []).map(c => c.name)));
      log('the combo keeps the chosen strategy and members',
        addedCombo && addedCombo.strategy === 'round-robin'
        && addedCombo.members[0] === 'probe-cloud/probe-large',
        addedCombo && JSON.stringify({ s: addedCombo.strategy, m: addedCombo.members }));

      // A combo id is the whole client config for that combo, so each row copies
      // its own.
      log('each combo row has its own Copy button',
        qa('#sp-pane [data-copy-combo]').length === 2,
        qa('#sp-pane [data-copy-combo]').length + ' buttons');
      (window.__probeClipboard || []).length = 0;
      q('#sp-pane [data-copy-combo="0"]').click();
      await until(() => (window.__probeClipboard || []).length > 0, 4000);
      log('copying a combo yields combo/name, not the label',
        window.__probeClipboard[0] === 'combo/fast',
        JSON.stringify(window.__probeClipboard[0] || null));

      // ---- settings: the request log is a dashboard, not a dump -----------
      await openSettings('logs');
      log('the log pane draws a 24-bar activity chart',
        qa('#sp-pane .sp-chart rect').length === 24,
        qa('#sp-pane .sp-chart rect').length + ' bars');
      log('a quiet hour is drawn differently from a busy one',
        qa('#sp-pane .sp-chart rect.zero').length > 0
        && qa('#sp-pane .sp-chart rect.on').length > 0,
        `${qa('#sp-pane .sp-chart rect.zero').length} zero / ${qa('#sp-pane .sp-chart rect.on').length} on`);
      const figures = qa('#sp-pane .sp-stat .s-val').map(n => n.textContent.trim());
      log('five figures summarise the traffic', figures.length === 5, figures.join(' | '));
      log('the token figure is humanised', figures[0] === '2.6k', figures[0]);
      log('the mean first-token time is shown in seconds', figures[1] === '320 ms', figures[1]);
      log('the mean throughput is shown', figures[2] === '188.4', figures[2]);
      log('the cost figure is an estimate, and small enough to need 4 decimals',
        /\$0\.0063/.test(q('#sp-pane .sp-stats').textContent), JSON.stringify(figures));
      log('the log size is shown', /KB|MB/.test(figures[4]), figures[4]);
      log('the card says the totals outlive a clear',
        /Clear log<\/b> empties the list/.test(q('#sp-pane').innerHTML));
      log('top models are ranked by tokens with a bar each',
        qa('#sp-pane .sp-bar').length === 2, qa('#sp-pane .sp-bar').length + ' bars');
      log('a model with no rate says so instead of showing $0.00',
        /no rate/.test(q('#sp-pane').textContent));
      log('every finished request has a row', qa('#sp-pane .sp-logrow:not(.live)').length === 3,
        qa('#sp-pane .sp-logrow:not(.live)').length + ' rows');
      log('the request being served right now is in the list',
        qa('#sp-pane .sp-logrow.live').length === 1,
        qa('#sp-pane .sp-logrow.live').length + ' live rows');
      log('the running request says how long it has been running',
        /2\.5s/.test(q('#sp-pane .sp-logrow.live').textContent),
        q('#sp-pane .sp-logrow.live').textContent.replace(/\s+/g, ' ').trim());
      log('the header counts what is in flight',
        /1 running/.test(q('#sp-pane').textContent),
        (q('#sp-pane').textContent.match(/\d+ running/) || [''])[0]);
      log('the first-token time is its own column',
        qa('#sp-pane .lr-ttft').length === 4 + 1,
        qa('#sp-pane .lr-ttft').length + ' cells (header + 3 rows + 1 live)');
      log('throughput is its own column, and a buffered call shows a dash',
        /212\.1/.test(q('#sp-pane .sp-logtable').textContent)
        && qa('#sp-pane .sp-logrow .lr-tps').map(n => n.textContent.trim()).includes('—'),
        qa('#sp-pane .sp-logrow .lr-tps').map(n => n.textContent.trim()).join(' | '));
      log('a buffered reply marks the first-token cell as not applicable',
        qa('#sp-pane .sp-logrow .lr-ttft').map(n => n.textContent.trim()).includes('·'),
        qa('#sp-pane .sp-logrow .lr-ttft').map(n => n.textContent.trim()).join(' | '));
      log('the failure is marked and its message is shown',
        qa('#sp-pane .sp-logrow.bad').length === 1 && qa('#sp-pane .sp-logerror').length === 1,
        `${qa('#sp-pane .sp-logrow.bad').length} bad / ${qa('#sp-pane .sp-logerror').length} messages`);
      log('a row names the model id the client sent, not the upstream one',
        /combo\/fast/.test(q('#sp-pane .sp-logtable').textContent));
      log('the table states its own ceiling',
        /3\/100/.test(q('#sp-pane').textContent),
        (q('#sp-pane').textContent.match(/\d+\/100/) || [''])[0]);

      // ---- settings: a row opens its detail -------------------------------
      q('#sp-pane [data-log-row="done:' + (window.__probeLogs.entries[1].at) + '"]').click();
      await wait(120);
      const detail = q('#sp-pane .sp-logdetail');
      log('clicking a row opens its detail', !!detail, detail ? 'open' : 'missing');
      log('the detail names the provider and the upstream id',
        /Probe Cloud/.test(detail.textContent) && /probe-large/.test(detail.textContent));
      log('the detail separates latency from generation',
        /First token/.test(detail.textContent) && /Generation/.test(detail.textContent));
      log('a counted token figure is labelled as counted, not as the provider\'s',
        /counted/.test(detail.textContent));
      q('#sp-pane [data-log-row="done:' + (window.__probeLogs.entries[1].at) + '"]').click();
      await wait(120);
      log('clicking it again closes the detail', !q('#sp-pane .sp-logdetail'));

      // ---- settings: clearing the list keeps the dashboard ----------------
      const clearBefore = (window.__probeLogs || {}).entries.length;
      const chartBefore = qa('#sp-pane .sp-chart rect').length;
      q('#logs-clear').click();
      await until(confirmOpen, 4000);
      log('clearing the log asks first', confirmOpen(), 'confirm dialog');
      log('the confirm says the dashboard survives a clear',
        /separate database/.test(q('#confirm-text').textContent),
        q('#confirm-text').textContent.replace(/\s+/g, ' ').slice(0, 80));
      q('#confirm-ok').click();
      await wait(200);
      log('clearing empties the table', qa('#sp-pane .sp-logrow:not(.live)').length === 0,
        `${clearBefore} -> ${qa('#sp-pane .sp-logrow:not(.live)').length}`);
      log('clearing keeps the chart and the ranking, because the totals are in SQLite',
        qa('#sp-pane .sp-chart rect').length === chartBefore
        && qa('#sp-pane .sp-bar').length === 2,
        `${qa('#sp-pane .sp-chart rect').length} bars / ${qa('#sp-pane .sp-bar').length} model bars`);

      await closeSettings();

      // ---- chat: opening takes width from the tab area -------------------
      at('chat open');
      const hideBeforeChat = calls('hide_all_tabs');
      const rectsBefore = (window.__probeRects || []).length;
      await window.RustBoxChat.open();
      await until(() => q('#chat-panel').classList.contains('open'), 6000);
      log('the chat panel opens as a column', q('#chat-panel').classList.contains('open'));
      log('opening chat re-reports the content rect',
        (window.__probeRects || []).length > rectsBefore,
        `${rectsBefore} -> ${(window.__probeRects || []).length}`);
      const panelWidth = q('#chat-panel').getBoundingClientRect().width;
      log('the column has a real width', panelWidth > 300, Math.round(panelWidth) + 'px');
      log('the chat button reports the panel is up', q('#chat-btn').classList.contains('on'));
      // The panel sits beside the page, not over it: opening it must not hide
      // the tab windows, or the agent would be talking about nothing.
      log('opening chat does not hide the tab windows', calls('hide_all_tabs') === hideBeforeChat,
        `${hideBeforeChat} -> ${calls('hide_all_tabs')}`);

      // ---- chat: the model picker never shows a raw key ------------------
      const picker = q('#ch-provider');
      log('the provider picker is built from the masked rows', !!picker,
        picker ? picker.options.length + ' options' : 'missing');
      log('no provider option leaks a key',
        picker ? !/sk-|api[-_]?key|\*\*\*\*/i.test(picker.textContent) : false,
        picker ? picker.textContent.trim() : '');

      // ---- chat: a turn is shown before the round trip -------------------
      // The send is a blocking round trip, so an echoed-only transcript feels
      // dead for as long as the model takes. The optimistic turn is what fixes
      // that, and it has to disappear again if the call fails.
      at('chat send');
      const input = q('#ch-input');
      type(input, 'What is on this page?');
      q('#ch-send').click();
      await wait(120);
      const optimistic = q('#ch-log').textContent.includes('What is on this page?');
      log('the sent turn appears immediately', optimistic);
      await until(() => !q('#ch-send').disabled, 8000);
      log('the panel settles after the reply',
        /Probe reply/i.test(q('#ch-log').textContent),
        q('#ch-log').textContent.replace(/\s+/g, ' ').slice(0, 80));
      log('the transcript shows the turn once, not twice',
        qa('#ch-log .ch-msg.user').length === 1 && qa('#ch-log .ch-msg.assistant').length === 1,
        `${qa('#ch-log .ch-msg.user').length} user / ${qa('#ch-log .ch-msg.assistant').length} agent`);
      log('the tool call is rendered as its own block',
        qa('#ch-log .ch-tool').length >= 1, qa('#ch-log .ch-tool').length + ' blocks');
      log('the conversation takes its title from the first turn',
        q('#ch-title').textContent.trim() === 'What is on this page?',
        q('#ch-title').textContent.trim());

      // A provider serving several models gets a second picker, so the model is
      // a per-conversation choice rather than a provider-level one. Checked here
      // because the picker only exists once a conversation does: before the first
      // send there is no conversation to attach a model to.
      const modelPicker = q('#ch-model');
      log('a multi-model provider offers a model picker', !!modelPicker,
        modelPicker ? modelPicker.options.length + ' options' : 'missing');
      log('the model picker lists every model the provider serves',
        modelPicker ? [...modelPicker.options].map(o => o.value).join(',') === 'probe-large,probe-small,probe-vision' : false,
        modelPicker ? [...modelPicker.options].map(o => o.value).join(',') : 'missing');
      log('the model picker starts on the conversation model',
        modelPicker ? modelPicker.value === 'probe-large' : false,
        modelPicker ? modelPicker.value : 'missing');

      // Switching model writes it to the conversation, which is what makes the
      // next request use it and a reopened conversation remember it.
      at('chat switch model');
      if (modelPicker) {
        modelPicker.value = 'probe-vision';
        modelPicker.dispatchEvent(new Event('change'));
        await wait(200);
        const conv = window.__probeConversation;
        log('switching model is written to the conversation',
          conv && conv.model === 'probe-vision', conv && conv.model);
      } else {
        log('switching model is written to the conversation', false, 'no picker');
      }

      // ---- chat: closing gives the width back ----------------------------
      at('chat close');
      const rectsBeforeClose = (window.__probeRects || []).length;
      window.RustBoxChat.close();
      await wait(120);
      log('closing chat re-reports the content rect',
        (window.__probeRects || []).length > rectsBeforeClose,
        `${rectsBeforeClose} -> ${(window.__probeRects || []).length}`);
      log('the chat button clears its state', !q('#chat-btn').classList.contains('on'));
    } catch (e) {
      log('probe threw', false, (e && (e.message || e)) || e);
    }

    finished = true;
    flush();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', run);
  } else {
    run();
  }
})();
