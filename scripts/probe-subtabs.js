// Injected by scripts/probe.ps1 into a temporary copy of ui/index.html.
//
// Guards the sub-tab strip's two rules:
//
//  1. The first sub-tab is the app itself: it is pinned, has no close button,
//     and cannot be moved - the same "this one is the anchor" idea that already
//     stops it being closed.
//  2. Every other sub-tab can be moved, and the request names the right tab, the
//     right target and the right side.
//
// The strip reorders from pointer events, not HTML5 drag and drop: under the CEF
// engine the page never receives the `drop` the native drag loop owes it, so a
// drag-and-drop reorder cannot commit there. This probe drives pointer events.
//
// What this cannot prove is that the backend reorders; that is
// `tabs::reorder_tests` in Rust. Here the assertion is the strip's own wiring.
//
// Run it with real apps, e.g.
//   scripts/probe.ps1 -ProbeScript scripts/probe-subtabs.js -ListAppsJson '[...]'
(function () {
  'use strict';

  const results = [];
  // Written after every check, in the runner's `PASS|name|detail` line format.
  // A probe that hangs half way through otherwise reports nothing at all.
  let finished = false;
  const flush = () => {
    const sink = document.getElementById('probe');
    if (!sink) return;
    const rows = results.map(
      (r) => `${r.ok ? 'PASS' : 'FAIL'}|${r.name}|${String(r.detail).replace(/\|/g, '/')}`
    );
    if (!finished) rows.push('FAIL|probe stopped before finishing||');
    sink.textContent = rows.join('\n');
  };
  const log = (name, ok, detail) => {
    results.push({ name, ok: !!ok, detail: detail === undefined ? '' : String(detail) });
    flush();
  };

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

  // The strip reorders from pointer events (see `enablePointerReorder`), so the
  // probe replays a mouse: press, cross the threshold, hover the target side,
  // release. `elementFromPoint` picks the target, so the moves carry real
  // coordinates from the chip rectangles.
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

  // Press the middle of `from`, drag onto the left or right edge of `to`, and
  // report the marker class painted there.
  async function pressAndHover(from, to, before) {
    const a = from.getBoundingClientRect();
    const b = to.getBoundingClientRect();
    const y = b.top + b.height / 2;
    const x0 = a.left + a.width / 2;
    const x1 = before ? b.left + 2 : b.right - 2;
    pointerEvent('pointerdown', from, x0, a.top + a.height / 2);
    for (const f of [0.2, 0.45, 0.7, 0.9, 1]) {
      pointerEvent('pointermove', window, x0 + (x1 - x0) * f, y);
      await wait(15);
    }
    return to.className;
  }

  function releaseAt(to, before) {
    const b = to.getBoundingClientRect();
    const x = before ? b.left + 2 : b.right - 2;
    pointerEvent('pointerup', window, x, b.top + b.height / 2);
  }

  async function run() {
    try {
      // A three-tab strip for the app startup opens: the pinned tab plus two the
      // user would have opened. Pushed as a `tabs-changed` snapshot, which is the
      // only thing the strip listens to — the backend sends one on every change.
      const snapshot = [
        { appId: 'app-a', active: 't2', tabs: [
          { id: 'main', url: 'https://a.test', title: 'Alpha', pinned: true, popup: false, active: false },
          { id: 't2', url: 'https://a.test/2', title: 'Second', pinned: false, popup: false, active: true },
          { id: 't3', url: 'https://a.test/3', title: 'Third', pinned: false, popup: false, active: false },
        ] },
      ];
      // Wait for the app to be active first: the strip only renders for one.
      await until(() => !!document.querySelector('.app-icon-btn.active'), 8000);
      window.__probeEmit('tabs-changed', snapshot);

      const up = await until(() => qa('#tab-strip .sub-tab').length === 3, 9000);
      log('the sub-tab strip renders every tab', up,
        qa('#tab-strip .sub-tab').length + ' chips');
      if (!up) return finish();
      const chips = qa('#tab-strip .sub-tab');
      const pinned = q('#tab-strip .sub-tab.pinned');

      log('the first sub-tab is the pinned app tab',
        !!pinned && pinned.dataset.tab === 'main',
        pinned ? pinned.dataset.tab : 'no pinned chip');
      log('the pinned sub-tab has no close button',
        pinned && !pinned.querySelector('[data-close]'),
        pinned ? 'close=' + !!pinned.querySelector('[data-close]') : 'no chip');
      // The anchor cannot move, for the same reason it cannot close. The strip no
      // longer advertises draggability as an attribute (nothing uses HTML5 drag
      // and drop any more), so the pinned chip is checked by behaviour below: a
      // press on it must not start a move.
      log('no chip advertises HTML5 drag and drop',
        qa('#tab-strip .sub-tab').every((c) => !c.hasAttribute('draggable')),
        qa('#tab-strip .sub-tab').map((c) => c.dataset.tab + ':' + c.getAttribute('draggable')).join(','));

      // --- right edge inserts after -------------------------------------
      const from = q('.sub-tab[data-tab="t3"]');
      const to = q('.sub-tab[data-tab="t2"]');
      const markerClass = await pressAndHover(from, to, false);
      log('dragging over the right half shows an insert-after marker',
        /\bdrop-after\b/.test(markerClass), 'class=' + markerClass);
      releaseAt(to, false);
      await wait(250);

      const sent = (window.__probeReorders || [])[0];
      log('releasing asks the backend to reorder',
        !!sent && sent.tabId === 't3' && sent.targetId === 't2' && sent.after === true,
        JSON.stringify(sent || null));
      log('the reorder names the app it belongs to',
        !!sent && sent.appId === 'app-a', sent ? sent.appId : 'none');

      // --- left edge inserts before -------------------------------------
      (window.__probeReorders || []).length = 0;
      const from2 = q('.sub-tab[data-tab="t2"]');
      const to2 = q('.sub-tab[data-tab="t3"]');
      const markerClass2 = await pressAndHover(from2, to2, true);
      log('dragging over the left half shows an insert-before marker',
        /\bdrop-before\b/.test(markerClass2), 'class=' + markerClass2);
      releaseAt(to2, true);
      await wait(250);
      const sent2 = (window.__probeReorders || [])[0];
      log('releasing on the left half inserts before the target',
        !!sent2 && sent2.after === false, JSON.stringify(sent2 || null));

      // --- a release on the chip itself is a no-op -----------------------
      (window.__probeReorders || []).length = 0;
      const self = q('.sub-tab[data-tab="t2"]');
      const sr = self.getBoundingClientRect();
      pointerEvent('pointerdown', self, sr.left + sr.width / 2, sr.top + sr.height / 2);
      await wait(20);
      pointerEvent('pointerup', window, sr.left + sr.width / 2, sr.top + sr.height / 2);
      await wait(200);
      log('pressing and releasing a tab in place sends nothing',
        (window.__probeReorders || []).length === 0,
        (window.__probeReorders || []).length + ' requests');

      // The pinned chip refuses to start a move at all.
      (window.__probeReorders || []).length = 0;
      const pinChip = q('.sub-tab.pinned');
      const target = q('.sub-tab[data-tab="t3"]');
      await pressAndHover(pinChip, target, true);
      releaseAt(target, true);
      await wait(200);
      log('the pinned tab never becomes the thing being moved',
        (window.__probeReorders || []).length === 0,
        (window.__probeReorders || []).length + ' requests');

      return finish();
    } catch (e) {
      log('the probe ran to completion', false, (e && e.message) || String(e));
      return finish();
    }
  }

  function finish() {
    finished = true;
    flush();
  }

  // The page's own script has already run by the time this executes, so startup
  // is in flight; a short settle keeps the first assertions off a blank strip.
  setTimeout(run, 400);
})();
