// Injected by scripts/probe.ps1 into a temporary copy of ui/index.html.
//
// Tauri's globals are stubbed by that script; this file only drives the real UI
// and reports what happened. It writes its rows into <pre id="probe">, which the
// PowerShell wrapper parses.
(function () {
  'use strict';

  const results = [];
  const log = (name, ok, detail) =>
    results.push({ name, ok: !!ok, detail: detail === undefined ? '' : String(detail) });

  const saved = window.__probeSaved || [];
  window.__probeSaved = saved;

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
  const fire = (el, type) => el.dispatchEvent(new Event(type, { bubbles: true }));

  const openTile = async (name) => {
    const tile = [...document.querySelectorAll('.catalog-item')].find((t) => t.dataset.name === name);
    if (!tile) return false;
    tile.click();
    return until(() => q('#modal-backdrop').classList.contains('open'));
  };
  const closeModal = async () => {
    q('#btn-cancel').click();
    await until(() => !q('#modal-backdrop').classList.contains('open'));
  };
  // The proxy fields are only editable for a Custom app, so every test that
  // types an endpoint has to choose that mode first.
  const setProxyMode = (mode) => {
    const radio = document.querySelector(`#f-proxy-mode input[value="${mode}"]`);
    radio.checked = true;
    fire(radio, 'change');
  };
  // A new app opens on Light, and Light cannot carry a proxy: this probe's global
  // proxy is a fixed endpoint, so a Light app that inherited it would connect
  // direct while the form said otherwise, and the save is refused. Every section
  // that exercises a proxy therefore has to ask for Isolated first.
  const useIsolated = () => {
    const sel = q('#f-profile-mode');
    sel.value = 'isolated';
    fire(sel, 'change');
  };

  async function run() {
    try {
      const ready = await until(() => document.querySelectorAll('.catalog-item').length > 100, 9000);
      const tiles = document.querySelectorAll('.catalog-item').length;
      log('catalog rendered', ready, tiles + ' tiles');
      if (!ready) throw new Error('catalog never rendered');
      log('all recipes reach the catalog', tiles >= 441, tiles + ' (was 369)');

      const setup = document.querySelectorAll('.setup-tag').length;
      log('setup tag only where input is needed', setup === 72,
        setup + ' (expect 43 templated + 29 self-hosted)');

      // --- plain recipe ---------------------------------------------------
      await openTile('Gmail');
      log('plain recipe keeps its URL', q('#f-url').value === 'https://mail.google.com', q('#f-url').value);
      log('plain recipe shows no setup block', q('#f-template').classList.contains('hidden'));
      await closeModal();

      // --- templated recipe ----------------------------------------------
      await openTile('Slack');
      const shown = !q('#f-template').classList.contains('hidden');
      const label = q('.tpl-field > span')?.textContent || '';
      log('templated recipe asks for its missing part', shown, label);
      log('raw template never reaches the URL field', q('#f-url').value === '',
        JSON.stringify(q('#f-url').value));

      q('#btn-save').click();
      await wait(300);
      const refused = q('#modal-backdrop').classList.contains('open');
      const msg = q('#modal-sub').textContent;
      log('cannot save while a part is missing', refused && /required/i.test(msg), msg);

      const input = q('.tpl-input');
      input.value = 'acme';
      fire(input, 'input');
      log('value is substituted into the template', q('#f-url').value === 'https://acme.slack.com',
        q('#f-url').value);
      const preview = q('#tpl-preview')?.textContent || '';
      log('live preview shows the resolved URL', preview.includes('https://acme.slack.com'), preview);

      q('#btn-save').click();
      await until(() => !q('#modal-backdrop').classList.contains('open'), 4000);
      const slack = (saved[saved.length - 1] || []).find((a) => (a.name || '').startsWith('Slack'));
      log('templated recipe saves the resolved URL', slack && slack.url === 'https://acme.slack.com',
        slack && slack.url);

      // --- self-hosted recipe --------------------------------------------
      await openTile('Mattermost');
      const shLabel = q('.tpl-field > span')?.textContent || '';
      log('self-hosted recipe asks for an address', shLabel === 'Server address', shLabel);
      const sh = q('.tpl-input');
      sh.value = 'chat.example.com';
      fire(sh, 'input');
      log('a bare host gets a scheme', q('#f-url').value === 'https://chat.example.com', q('#f-url').value);
      await closeModal();

      // --- storage default ------------------------------------------------
      // Light is the default for a new app, and it is the first row of the list.
      // Both matter: a select whose first row is not the default makes the user
      // hunt for the mode the form already chose for them.
      //
      // The one exception is a proxy in force. Light shares its WebView2
      // environment with the shell and cannot apply one, so defaulting to Light
      // there would produce a form whose save is refused. The default follows the
      // settings instead. This probe's stub has a custom global proxy, so both
      // halves are exercised: proxy on, then proxy off.
      await openTile('Gmail');
      log('Light is listed before Isolated',
        [...q('#f-profile-mode').options].map((o) => o.value).join(' / ') === 'light / isolated',
        [...q('#f-profile-mode').options].map((o) => o.value).join(' / '));
      log('a proxy in force keeps the default on Isolated',
        q('#f-profile-mode').value === 'isolated', q('#f-profile-mode').value);
      await closeModal();

      window.__probeSettings.global_proxy = { mode: 'none', custom: null, pool_id: null };
      await openTile('Gmail');
      log('with no proxy, a new app defaults to Light',
        q('#f-profile-mode').value === 'light', q('#f-profile-mode').value);
      log('Light is offered on this platform',
        !!q('#f-profile-mode').querySelector('option[value="light"]'),
        [...q('#f-profile-mode').options].map((o) => o.value).join(' / '));
      await closeModal();
      window.__probeSettings.global_proxy = {
        mode: 'custom',
        custom: { scheme: 'http', host: '10.9.8.7', port: 3128, username: 'probe', password: 'secret' },
        pool_id: null,
      };

      // --- proxy scheme ---------------------------------------------------
      await openTile('Gmail');
      log('a new app inherits the global proxy by default',
        q('#f-proxy-mode input:checked').value === 'inherit',
        q('#f-proxy-mode input:checked').value);
      log('inheriting leaves the endpoint fields inert',
        q('#f-proxy-host').disabled === true, 'disabled=' + q('#f-proxy-host').disabled);
      // Light cannot carry the inherited proxy, so a save in that mode has to be
      // refused and say which two settings are in conflict. Picking Light by hand
      // is still allowed; it is the save that stops it.
      q('#f-profile-mode').value = 'light';
      fire(q('#f-profile-mode'), 'change');
      q('#btn-save').click();
      await wait(300);
      const lightRefused = q('#modal-backdrop').classList.contains('open');
      const lightMsg = q('#modal-sub').textContent;
      log('Light refuses a save whose proxy would be dropped',
        lightRefused && /cannot use a proxy/i.test(lightMsg), lightMsg);
      // Isolated is the mode that can carry a proxy, so the rest of this section
      // runs there.
      useIsolated();
      setProxyMode('custom');
      log('choosing Custom makes the endpoint editable',
        q('#f-proxy-host').disabled === false, 'disabled=' + q('#f-proxy-host').disabled);
      const host = q('#f-proxy-host');
      host.value = '127.0.0.1';
      fire(host, 'input');
      q('#f-proxy-port').value = '1080';
      q('#f-proxy-scheme').value = 'socks5';
      const hint = q('#proxy-hint').textContent;
      log('proxy hint explains embedded scope', /this tab only/i.test(hint), hint);
      q('#btn-save').click();
      await until(() => !q('#modal-backdrop').classList.contains('open'), 4000);
      const proxied = (saved[saved.length - 1] || []).find((a) => a.proxy);
      log('socks5 survives the save',
        proxied && proxied.proxy.scheme === 'socks5' && proxied.proxy.port === 1080,
        proxied && JSON.stringify(proxied.proxy));
      log('a Custom app records its mode', proxied && proxied.proxy_mode === 'custom',
        proxied && proxied.proxy_mode);

      // --- switching back to Inherit drops the endpoint --------------------
      // A credential left in config.json for a tunnel nothing uses is a secret
      // stored for no reason, so the mode has to govern what is written.
      await openTile('Gmail');
      setProxyMode('inherit');
      q('#btn-save').click();
      await until(() => !q('#modal-backdrop').classList.contains('open'), 4000);
      const inherited = (saved[saved.length - 1] || [])[0];
      log('Inherit saves no endpoint', inherited && inherited.proxy === null,
        inherited && JSON.stringify(inherited.proxy));
      log('Inherit is written as the mode', inherited && inherited.proxy_mode === 'inherit',
        inherited && inherited.proxy_mode);

      // --- Direct is not the same answer as Inherit ------------------------
      await openTile('Gmail');
      setProxyMode('none');
      const directHint = q('#proxy-mode-hint').textContent;
      log('Direct says it ignores the global proxy', /even when a global proxy/i.test(directHint), directHint);
      log('Direct empties the endpoint fields', q('#f-proxy-host').value === '',
        JSON.stringify(q('#f-proxy-host').value));
      await closeModal();

      // --- Light + proxy must be called out -------------------------------
      // The host has to be filled in first: the warning is about a proxy that
      // cannot be applied, not about an empty proxy field.
      await openTile('Gmail');
      setProxyMode('custom');
      const host2 = q('#f-proxy-host');
      host2.value = '127.0.0.1';
      fire(host2, 'input');
      q('#f-profile-mode').value = 'light';
      fire(q('#f-profile-mode'), 'change');
      const lightHint = q('#proxy-hint').textContent;
      log('Light mode warns the proxy cannot apply', /cannot be applied/i.test(lightHint), lightHint);
      await closeModal();

      // --- browser picker -------------------------------------------------
      // Two Camoufox installs must not read as identical rows; the picker has
      // to say which copy is which.
      const opts = [...q('#f-browser').options].map((o) => o.textContent);
      const camoufoxRows = opts.filter((t) => t.toLowerCase().startsWith('camoufox'));
      log('every detected browser gets one row', opts.length === 4, opts.join(' / '));
      log('repeated browser names are disambiguated',
        camoufoxRows.length === 2 && new Set(camoufoxRows).size === 2,
        camoufoxRows.join(' / '));

      // Embedded apps render in RustBox's own webview, so the external-browser
      // picker must be inert there rather than implying a choice.
      await openTile('Gmail');
      log('embedded app disables the browser picker', q('#f-browser').disabled === true,
        'disabled=' + q('#f-browser').disabled);
      q('#f-engine').value = 'external';
      fire(q('#f-engine'), 'change');
      log('external app enables the browser picker', q('#f-browser').disabled === false,
        'disabled=' + q('#f-browser').disabled);
      log('embedded-only storage picker is disabled for external apps',
        q('#f-profile-mode').disabled === true, 'disabled=' + q('#f-profile-mode').disabled);
    } catch (e) {
      log('probe threw', false, (e && (e.message || e)) || e);
    }
    // One line per check: "PASS|name|detail". A JSON array has to survive HTML
    // escaping and PowerShell's own quoting, and a mangled parse would make a
    // failing check look like a passing one.
    const sink = document.getElementById('probe');
    if (sink) {
      sink.textContent = results
        .map((r) => `${r.ok ? 'PASS' : 'FAIL'}|${r.name}|${String(r.detail).replace(/\|/g, '/')}`)
        .join('\n');
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', run);
  } else {
    run();
  }
})();
