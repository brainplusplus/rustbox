// Injected by scripts/probe.ps1 into a temporary copy of ui/index.html.
//
// Guards the sub-tab strip's two rules:
//
//  1. The first sub-tab is the app itself: it is pinned, has no close button,
//     and cannot be dragged — the same "this one is the anchor" idea that
//     already stops it being closed.
//  2. Every other sub-tab can be dragged, and a drop names the right tab, the
//     right target and the right side.
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

  // Horizontal drop position decides before/after, so the event needs clientX.
  function dragEvent(type, el, { clientX, clientY, dataTransfer } = {}) {
    const ev = new Event(type, { bubbles: true, cancelable: true });
    if (clientX !== undefined) ev.clientX = clientX;
    if (clientY !== undefined) ev.clientY = clientY;
    if (dataTransfer) Object.defineProperty(ev, 'dataTransfer', { value: dataTransfer });
    el.dispatchEvent(ev);
    return ev;
  }

  function makeDataTransfer() {
    try {
      return new DataTransfer();
    } catch (_) {
      return { effectAllowed: '', dropEffect: '', setData() {}, getData() { return ''; } };
    }
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
      // The anchor cannot move, for the same reason it cannot close.
      log('the pinned sub-tab cannot be dragged',
        pinned && pinned.getAttribute('draggable') === 'false',
        pinned ? 'draggable=' + pinned.getAttribute('draggable') : 'no chip');
      log('every other sub-tab can be dragged',
        chips.filter((c) => !c.classList.contains('pinned'))
          .every((c) => c.getAttribute('draggable') === 'true'),
        chips.map((c) => c.dataset.tab + ':' + c.getAttribute('draggable')).join(','));

      // --- right half inserts after -------------------------------------
      const from = q('.sub-tab[data-tab="t3"]');
      const to = q('.sub-tab[data-tab="t2"]');
      const dt = makeDataTransfer();
      const r = to.getBoundingClientRect();
      dragEvent('dragstart', from, { dataTransfer: dt });
      dragEvent('dragover', to, { clientX: r.right - 2, dataTransfer: dt });
      log('dragging over the right half shows an insert-after marker',
        to.classList.contains('drop-after'), 'class=' + to.className);
      dragEvent('drop', to, { clientX: r.right - 2, dataTransfer: dt });
      dragEvent('dragend', from, { dataTransfer: dt });
      await wait(200);

      const sent = (window.__probeReorders || [])[0];
      log('dropping asks the backend to reorder',
        !!sent && sent.tabId === 't3' && sent.targetId === 't2' && sent.after === true,
        JSON.stringify(sent || null));
      log('the reorder names the app it belongs to',
        !!sent && sent.appId === 'app-a', sent ? sent.appId : 'none');

      // --- left half inserts before -------------------------------------
      (window.__probeReorders || []).length = 0;
      const from2 = q('.sub-tab[data-tab="t2"]');
      const to2 = q('.sub-tab[data-tab="t3"]');
      const dt2 = makeDataTransfer();
      const r2 = to2.getBoundingClientRect();
      dragEvent('dragstart', from2, { dataTransfer: dt2 });
      dragEvent('dragover', to2, { clientX: r2.left + 2, dataTransfer: dt2 });
      log('dragging over the left half shows an insert-before marker',
        to2.classList.contains('drop-before'), 'class=' + to2.className);
      dragEvent('drop', to2, { clientX: r2.left + 2, dataTransfer: dt2 });
      dragEvent('dragend', from2, { dataTransfer: dt2 });
      await wait(200);
      const sent2 = (window.__probeReorders || [])[0];
      log('dropping on the left half inserts before the target',
        !!sent2 && sent2.after === false, JSON.stringify(sent2 || null));

      // --- a drop on itself is a no-op ----------------------------------
      (window.__probeReorders || []).length = 0;
      const self = q('.sub-tab[data-tab="t2"]');
      const dt3 = makeDataTransfer();
      dragEvent('dragstart', self, { dataTransfer: dt3 });
      dragEvent('drop', self, { dataTransfer: dt3 });
      dragEvent('dragend', self, { dataTransfer: dt3 });
      await wait(150);
      log('dropping a tab on itself sends nothing',
        (window.__probeReorders || []).length === 0,
        (window.__probeReorders || []).length + ' requests');

      // The pinned chip refuses to start a drag at all.
      (window.__probeReorders || []).length = 0;
      const pinChip = q('.sub-tab.pinned');
      const dt4 = makeDataTransfer();
      const target = q('.sub-tab[data-tab="t3"]');
      const r4 = target.getBoundingClientRect();
      dragEvent('dragstart', pinChip, { dataTransfer: dt4 });
      dragEvent('drop', target, { clientX: r4.left + 2, dataTransfer: dt4 });
      dragEvent('dragend', pinChip, { dataTransfer: dt4 });
      await wait(150);
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
