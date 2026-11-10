// Visual capture of the real UI, injected by scripts/shots.ps1 into a temporary
// copy of ui/index.html. It drives the page into each state worth looking at and
// leaves it there; the wrapper screenshots the window between steps.
//
// The state to show comes from `?state=<name>` on the URL.
(function () {
  'use strict';

  const state = new URLSearchParams(location.search).get('state') || 'catalog';
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const until = async (fn, ms = 8000) => {
    const t0 = Date.now();
    while (Date.now() - t0 < ms) {
      try { if (fn()) return true; } catch (_) {}
      await wait(50);
    }
    return false;
  };
  const q = (s) => document.querySelector(s);
  const fire = (el, type) => el.dispatchEvent(new Event(type, { bubbles: true }));

  const openTile = async (name) => {
    const tile = [...document.querySelectorAll('.catalog-item')].find((t) => t.dataset.name === name);
    if (!tile) return false;
    tile.scrollIntoView({ block: 'center' });
    await wait(120);
    tile.click();
    return until(() => q('#modal-backdrop').classList.contains('open'));
  };

  // The toolbar has two identities, so the shot has to be able to reach both.
  // Tab mode is reached by marking the bar directly: the real app switches it
  // from updateToolbar() once a tab exists, and building a live tab needs the
  // native backend this harness stubs out.
  const forceTabMode = () => {
    const bar = q('#toolbar');
    bar.classList.remove('mode-catalog');
    bar.classList.add('mode-tab');
    q('#tab-title').textContent = 'WhatsApp';
    q('#addr-input').value = 'https://web.whatsapp.com/';
    q('#nav-reload').disabled = false;
    q('#nav-home').disabled = false;
    q('#nav-back').disabled = true;
    q('#nav-fwd').disabled = true;
  };

  async function run() {
    await until(() => document.querySelectorAll('.catalog-item').length > 100, 12000);

    if (state === 'sidebar') {
      // The rail with apps in it, one of them open. The point is the difference
      // between an idle socket and the selected one, so the shot has to contain
      // both. Opening a real tab needs the native backend this harness stubs out,
      // so the selected state is set on the button directly.
      await until(() => document.querySelectorAll('.app-icon-btn').length > 3, 8000);
      const btns = [...document.querySelectorAll('.app-icon-btn')];
      if (btns[2]) {
        btns[2].classList.add('active');
        q('#tab-title').textContent = btns[2].title;
      }
      q('#catalog').classList.add('hidden');
      q('#empty-state').classList.add('hidden');
      forceTabMode();
      await wait(300);
      document.body.dataset.ready = 'sidebar';
      return;
    }

    if (state === 'catalog') {
      // With no saved apps the shell shows the welcome screen; the catalog has to
      // be opened the same way a user would open it.
      q('#add-app-btn').click();
      await until(() => !q('#catalog').classList.contains('hidden'), 6000);
      await wait(400);
      document.body.dataset.ready = 'catalog';
      return;
    }

    if (state === 'tabbar') {
      q('#add-app-btn').click();
      await until(() => !q('#catalog').classList.contains('hidden'), 6000);
      // A real tab is a native window sitting over #content, so nothing in the
      // page is visible behind it. Hiding the overlays reproduces that instead of
      // showing the catalog through a toolbar that says a tab is open.
      q('#catalog').classList.add('hidden');
      q('#empty-state').classList.add('hidden');
      forceTabMode();
      await wait(250);
      return;
    }

    if (state === 'templated') {
      await openTile('Slack');
      const input = q('.tpl-input');
      if (input) { input.value = 'acme'; fire(input, 'input'); }
      return;
    }

    if (state === 'selfhosted') {
      await openTile('Mattermost');
      return;
    }

    if (state === 'proxy') {
      await openTile('Gmail');
      const host = q('#f-proxy-host');
      if (host) { host.value = 'proxy.example.com'; fire(host, 'input'); }
      const port = q('#f-proxy-port');
      if (port) { port.value = '1080'; }
      const scheme = q('#f-proxy-scheme');
      if (scheme) { scheme.value = 'socks5'; fire(scheme, 'change'); }
      return;
    }

    // The three router surfaces. They read their data from the backend, so the
    // stub in shots.ps1 answers get_settings / router_state / router_logs with
    // the same fixtures probe.ps1 uses; without those the panes would capture
    // their empty states and say nothing about the layout.
    if (state === 'router' || state === 'combo' || state === 'logs') {
      await window.RustBoxSettings.open(state);
      await until(() => q('#settings-page') && !q('#settings-page').classList.contains('hidden'), 8000);
      await wait(500);
      document.body.dataset.ready = state;
      return;
    }

    if (state === 'browser') {
      // External engine so the picker is live, then open it: the point of the
      // shot is how repeated browser names read.
      await openTile('Gmail');
      q('#f-engine').value = 'external';
      fire(q('#f-engine'), 'change');
      const sel = q('#f-browser');
      if (sel) { sel.focus(); sel.size = sel.options.length; }
      await wait(250);
      return;
    }
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', run);
  } else {
    run();
  }
})();
