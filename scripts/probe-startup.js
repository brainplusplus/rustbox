// Injected by scripts/probe.ps1 into a temporary copy of ui/index.html.
//
// Guards two behaviours that are easy to break and hard to notice:
//
//  1. Launching lands on a tab, not on the welcome screen, whenever the
//     workspace has something openable.
//  2. The sidebar can be reordered by dragging, and the new order is what gets
//     persisted (the array order *is* the sidebar order).
//
// Run it with real apps, e.g.
//   scripts/probe.ps1 -ProbeScript scripts/probe-startup.js -ListAppsJson '[...]'
(function () {
  'use strict';

  const results = [];
  const log = (name, ok, detail) =>
    results.push({ name, ok: !!ok, detail: detail === undefined ? '' : String(detail) });

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

  const iconOrder = () =>
    [...document.querySelectorAll('.app-icon-btn')].map((b) => b.dataset.id);

  // The UI listens for real drag events, so the probe dispatches them. A
  // DataTransfer is required by dragstart; jsdom-less browsers have one, but
  // headless Chrome needs it constructed explicitly.
  function dragEvent(type, el, { clientY, dataTransfer } = {}) {
    const ev = new Event(type, { bubbles: true, cancelable: true });
    if (clientY !== undefined) ev.clientY = clientY;
    if (dataTransfer) {
      Object.defineProperty(ev, 'dataTransfer', { value: dataTransfer });
    }
    el.dispatchEvent(ev);
    return ev;
  }

  function makeDataTransfer() {
    try {
      return new DataTransfer();
    } catch (_) {
      // Minimal stand-in if DataTransfer is unavailable.
      return { effectAllowed: '', dropEffect: '', setData() {}, getData() { return ''; } };
    }
  }

  async function run() {
    try {
      // The seed, not localStorage: activate() persists the tab it opens, so by
      // the time the probe looks, localStorage can no longer tell a seeded run
      // from a fresh one.
      const remembered = window.__probeSeed || '';
      const expectWelcome = window.__probeExpectWelcome === true;

      // Expected sidebar size comes from the app's own state rather than a
      // constant, so the same probe works for any fixture the caller passes.
      const expected = (typeof apps !== 'undefined' && Array.isArray(apps)) ? apps.length : 0;
      await until(() => document.querySelectorAll('.app-icon-btn').length === expected, 9000);

      const order = iconOrder();
      log('sidebar renders every app', order.length === expected,
        `${order.length} of ${expected}: ${order.join(',')}`);

      // Chromium makes a bare <img> natively draggable, so without an explicit
      // opt-out a drag on a tile starts as an *image* drag (raw-icon ghost,
      // `copy` effect) even though the button's handler still fires. Assert the
      // opt-out on every icon the sidebar renders.
      const imgs = [...document.querySelectorAll('#app-list img')];
      const bad = imgs.filter((img) => img.getAttribute('draggable') !== 'false');
      log('sidebar icons do not hijack the drag', bad.length === 0,
        `${imgs.length} icons, ${bad.length} still draggable`);

      // Agent access is configured only in Settings now. A sidebar button was a
      // second entry point for one setting, so its absence is the requirement.
      log('the sidebar has no Agent Access button',
        !document.querySelector('#mcp-btn'),
        document.querySelector('#mcp-btn') ? 'still present' : 'absent');

      // --- startup landing -------------------------------------------------
      // init() opens a usable tab. The stub records open_embedded_tab calls,
      // which is the observable proof a tab was requested rather than the
      // welcome screen merely being hidden.
      //
      // Three legitimate outcomes, and the probe asserts exactly one of them:
      //   - a remembered tab exists      -> reopen it
      //   - something openable exists    -> open the first enabled embedded app
      //   - nothing openable             -> welcome screen, and no tab request
      if (expectWelcome) {
        await wait(1500);
        // Two welcome surfaces exist and the code picks between them: an empty
        // workspace shows the onboarding card, a workspace whose apps are all
        // external shows the catalog (the shell has nothing of its own to
        // render, but the user still needs somewhere to click). Either is a
        // valid landing, so assert that one of them is up rather than
        // hard-coding which.
        const onboardingUp = !q('#empty-state').classList.contains('hidden');
        const catalogUp = !q('#catalog').classList.contains('hidden');
        log('nothing openable leaves a welcome surface up', onboardingUp || catalogUp,
          `empty-state visible=${onboardingUp} catalog visible=${catalogUp}`);
        log('nothing openable requests no tab',
          (window.__probeOpened || []).length === 0,
          'opened=' + JSON.stringify(window.__probeOpened || []));
        return finish();
      }

      const opened = await until(() => (window.__probeOpened || []).length > 0, 6000);
      const openedId = (window.__probeOpened || [])[0];
      log('startup opens a tab instead of the welcome screen', opened, openedId || 'none');
      log('startup does not land on the welcome screen',
        q('#empty-state').classList.contains('hidden'),
        'empty-state hidden=' + q('#empty-state').classList.contains('hidden'));
      log('startup does not land on the catalog',
        q('#catalog').classList.contains('hidden'),
        'catalog hidden=' + q('#catalog').classList.contains('hidden'));

      // The first *enabled embedded* app must win when nothing is remembered.
      // With apps [a(enabled), b(disabled), c(enabled)] that is `a`. When a
      // remembered tab is seeded the probe asserts that instead, so the two
      // expectations cannot both be checked in one run.
      if (remembered) {
        log('startup reopens the remembered tab', openedId === remembered,
          `opened=${openedId} remembered=${remembered}`);
        log('the reopened tab is marked active',
          q(`.app-icon-btn[data-id="${remembered}"]`).classList.contains('active'),
          'class=' + q(`.app-icon-btn[data-id="${remembered}"]`).className);
      } else {
        log('startup prefers the first enabled embedded app', openedId === 'app-a', openedId);
        log('the opened tab is marked active',
          q('.app-icon-btn[data-id="app-a"]').classList.contains('active'),
          'class=' + q('.app-icon-btn[data-id="app-a"]').className);
      }

      // --- drag reorder ----------------------------------------------------
      // Needs the three-app fixture; a caller that passes fewer apps gets the
      // startup checks only, so the probe never fails on missing elements.
      if (expected < 3) return finish();

      const before = iconOrder();
      const fromEl = q('.app-icon-btn[data-id="app-c"]');
      const toEl = q('.app-icon-btn[data-id="app-a"]');
      const dt = makeDataTransfer();
      const rect = toEl.getBoundingClientRect();
      // Upper half of the target = insert before it.
      dragEvent('dragstart', fromEl, { dataTransfer: dt });
      dragEvent('dragover', toEl, { clientY: rect.top + 2, dataTransfer: dt });
      const markerShown = toEl.classList.contains('drop-before');
      log('dragging over the top half shows an insert-before marker', markerShown,
        'class=' + toEl.className);
      dragEvent('drop', toEl, { clientY: rect.top + 2, dataTransfer: dt });
      dragEvent('dragend', fromEl, { dataTransfer: dt });
      await wait(120);

      const after = iconOrder();
      log('dropping moves the icon to the requested slot',
        after[0] === 'app-c' && after.length === before.length, after.join(','));
      log('the moved app is no longer in its old slot', after.indexOf('app-c') === 0, after.join(','));

      // --- persistence -----------------------------------------------------
      const saved = (window.__probeSaved || []);
      const last = saved[saved.length - 1] || [];
      const savedIds = last.map((a) => a.id);
      log('the new order is persisted', savedIds.join(',') === after.join(','), savedIds.join(','));

      // --- lower-half drop inserts after --------------------------------
      const from2 = q('.app-icon-btn[data-id="app-a"]');
      const to2 = q('.app-icon-btn[data-id="app-b"]');
      const dt2 = makeDataTransfer();
      const r2 = to2.getBoundingClientRect();
      dragEvent('dragstart', from2, { dataTransfer: dt2 });
      dragEvent('dragover', to2, { clientY: r2.bottom - 2, dataTransfer: dt2 });
      const afterMarker = to2.classList.contains('drop-after');
      log('dragging over the bottom half shows an insert-after marker', afterMarker,
        'class=' + to2.className);
      dragEvent('drop', to2, { clientY: r2.bottom - 2, dataTransfer: dt2 });
      dragEvent('dragend', from2, { dataTransfer: dt2 });
      await wait(120);

      const after2 = iconOrder();
      const aPos = after2.indexOf('app-a');
      const bPos = after2.indexOf('app-b');
      log('dropping on the lower half places the icon after the target',
        aPos === bPos + 1, after2.join(','));

      // --- a drop onto itself is a no-op ---------------------------------
      const selfEl = q('.app-icon-btn[data-id="app-b"]');
      const dt3 = makeDataTransfer();
      const snapshot = iconOrder().join(',');
      dragEvent('dragstart', selfEl, { dataTransfer: dt3 });
      dragEvent('drop', selfEl, { clientY: 0, dataTransfer: dt3 });
      dragEvent('dragend', selfEl, { dataTransfer: dt3 });
      await wait(80);
      log('dropping an icon on itself changes nothing', iconOrder().join(',') === snapshot,
        iconOrder().join(','));
    } catch (e) {
      log('probe threw', false, (e && (e.message || e)) || e);
    }
    finish();
  }

  // One line per check: "PASS|name|detail". Parsed by hand on the PowerShell
  // side so a mangled row fails loudly instead of collapsing into a silent pass.
  function finish() {
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
