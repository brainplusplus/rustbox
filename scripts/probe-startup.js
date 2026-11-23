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

  // The UI reorders from pointer events, not HTML5 drag and drop: the CEF engine
  // never delivers the `drop` the native drag loop owes the page, so a reorder
  // built on `drop` commits nothing there. The helpers below replay what a mouse
  // sends - press, cross the 4px threshold, hover the target, release - and the
  // reorder hit-tests with `elementFromPoint`, so the moves are dispatched at the
  // coordinates of the element being crossed.
  function pointerEvent(type, target, x, y) {
    const ev = new PointerEvent(type, {
      bubbles: true,
      cancelable: true,
      composed: true,
      pointerId: 1,
      pointerType: 'mouse',
      isPrimary: true,
      button: 0,
      buttons: type === 'pointerup' ? 0 : 1,
      clientX: x,
      clientY: y,
    });
    (target || window).dispatchEvent(ev);
    return ev;
  }

  // Press the middle of `from`, drag onto the upper or lower edge of `to`, and
  // report the marker class painted there. The release is separate so the caller
  // can assert on the preview before committing it.
  async function pressAndHover(from, to, before) {
    const a = from.getBoundingClientRect();
    const b = to.getBoundingClientRect();
    const x = b.left + b.width / 2;
    const y0 = a.top + a.height / 2;
    const y1 = before ? b.top + 2 : b.bottom - 2;
    pointerEvent('pointerdown', from, a.left + a.width / 2, y0);
    for (const f of [0.2, 0.45, 0.7, 0.9, 1]) {
      pointerEvent('pointermove', window, x, y0 + (y1 - y0) * f);
      await wait(15);
    }
    return to.className;
  }

  function releaseAt(to, before) {
    const b = to.getBoundingClientRect();
    const x = b.left + b.width / 2;
    const y = before ? b.top + 2 : b.bottom - 2;
    pointerEvent('pointerup', window, x, y);
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

      // --- pointer reorder ------------------------------------------------
      // Needs the three-app fixture; a caller that passes fewer apps gets the
      // startup checks only, so the probe never fails on missing elements.
      if (expected < 3) return finish();

      const before = iconOrder();
      const fromEl = q('.app-icon-btn[data-id="app-c"]');
      const toEl = q('.app-icon-btn[data-id="app-a"]');
      // Upper edge of the target = insert before it.
      const markerClass = await pressAndHover(fromEl, toEl, true);
      const markerShown = /\bdrop-before\b/.test(markerClass);
      log('dragging over the top half shows an insert-before marker', markerShown,
        'class=' + markerClass);
      releaseAt(toEl, true);
      await wait(200);

      const after = iconOrder();
      log('releasing moves the icon to the requested slot',
        after[0] === 'app-c' && after.length === before.length, after.join(','));
      log('the moved app is no longer in its old slot', after.indexOf('app-c') === 0, after.join(','));

      // --- persistence -----------------------------------------------------
      const saved = (window.__probeSaved || []);
      const last = saved[saved.length - 1] || [];
      const savedIds = last.map((a) => a.id);
      log('the new order is persisted', savedIds.join(',') === after.join(','), savedIds.join(','));

      // --- lower half inserts after ----------------------------------------
      const from2 = q('.app-icon-btn[data-id="app-a"]');
      const to2 = q('.app-icon-btn[data-id="app-b"]');
      const afterClass = await pressAndHover(from2, to2, false);
      const afterMarker = /\bdrop-after\b/.test(afterClass);
      log('dragging over the bottom half shows an insert-after marker', afterMarker,
        'class=' + afterClass);
      releaseAt(to2, false);
      await wait(200);

      const after2 = iconOrder();
      const aPos = after2.indexOf('app-a');
      const bPos = after2.indexOf('app-b');
      log('releasing on the lower half places the icon after the target',
        aPos === bPos + 1, after2.join(','));

      // --- a release on the item itself is a no-op -------------------------
      const selfEl = q('.app-icon-btn[data-id="app-b"]');
      const snapshot = iconOrder().join(',');
      const sr = selfEl.getBoundingClientRect();
      pointerEvent('pointerdown', selfEl, sr.left + sr.width / 2, sr.top + sr.height / 2);
      await wait(20);
      pointerEvent('pointerup', window, sr.left + sr.width / 2, sr.top + sr.height / 2);
      await wait(120);
      log('pressing and releasing an icon in place changes nothing',
        iconOrder().join(',') === snapshot, iconOrder().join(','));
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
