// Injected by scripts/probe-ctx.ps1 into a temporary copy of ui/ctx.html.
//
// Guards the bug where the right-click menu was rendered in a window whose
// height was a hardcoded constant: the menu grew a row, the constant did not,
// and "Remove" ended up clipped by 30px with `overflow: hidden` making it
// unreachable. The page must therefore report its real height and Rust must be
// the one that sizes the window.
(function () {
  'use strict';

  const results = [];
  const log = (name, ok, detail) =>
    results.push({ name, ok: !!ok, detail: detail === undefined ? '' : String(detail) });

  function report() {
    const sink = document.getElementById('probe');
    if (sink) {
      sink.textContent = results
        .map((r) => `${r.ok ? 'PASS' : 'FAIL'}|${r.name}|${String(r.detail).replace(/\|/g, '/')}`)
        .join('\n');
    }
  }

  async function run() {
    try {
      const calls = window.__fits || [];
      log('the menu reports its height to Rust', calls.length > 0,
        'ctx_menu_fit calls: ' + calls.length);

      const menu = document.getElementById('menu');
      const measured = Math.ceil(menu.getBoundingClientRect().height);
      // The reported number is what the window is sized to, so it has to be the
      // real content height, not an estimate.
      log('reported height matches the rendered menu',
        calls.length > 0 && calls[0] === measured,
        'reported=' + (calls[0] ?? 'none') + ' measured=' + measured);

      // Every row must sit inside the reported height. This is the check that
      // fails when a new entry is added and nothing accounts for it.
      const rows = [...menu.querySelectorAll('.ctx-item')];
      const bottom = Math.max(...rows.map((r) => r.offsetTop + r.offsetHeight));
      log('every menu row fits inside the reported height',
        calls.length > 0 && bottom <= calls[0],
        'last row bottom=' + bottom + ' reported=' + (calls[0] ?? 'none'));

      // The specific regression: Remove is the last row and the one users
      // reported as missing.
      const remove = rows.find((r) => r.dataset.action === 'remove-app');
      log('Remove is present and fully reachable',
        !!remove && remove.offsetTop + remove.offsetHeight <= (calls[0] ?? 0),
        remove ? 'top=' + remove.offsetTop + ' bottom=' + (remove.offsetTop + remove.offsetHeight) : 'missing');

      // The height must be reported once, not once per frame: a repeated resize
      // would make the popup flicker and jump.
      log('height is reported once when it does not change',
        calls.length === 1, 'calls=' + JSON.stringify(calls));

      // Reordering without a drag. The stub lists 'emb' first and 'ext' second,
      // so exactly one of the two rows must appear: "Move up" is impossible at
      // the top and "Move down" is impossible at the bottom. A row that is
      // offered but does nothing is worse than no row at all.
      const up = rows.find((r) => r.dataset.action === 'move-app-up');
      const down = rows.find((r) => r.dataset.action === 'move-app-down');
      // The probe runs in its own script scope, so the app id comes from the URL
      // the harness opened rather than from the page's own constants.
      const probeApp = new URLSearchParams(location.search).get('app');
      const first = probeApp === 'emb';
      log('the reorder rows match the app position',
        first ? (!up && !!down) : (!!up && !down),
        'app=' + probeApp + ' up=' + !!up + ' down=' + !!down);

      // Both rows have to be reachable, i.e. inside the height Rust is told to
      // use, or the menu silently loses the only non-drag way to reorder.
      const move = up || down;
      log('the reorder row fits inside the reported height',
        !!move && move.offsetTop + move.offsetHeight <= (calls[0] ?? 0),
        move ? 'top=' + move.offsetTop + ' bottom=' + (move.offsetTop + move.offsetHeight) : 'missing');

      // Both rows draw from the icon map. A missing key renders an empty <svg>,
      // which reads as a blank gutter next to the label.
      log('the reorder row has an icon',
        !!move && !!move.querySelector('svg path'),
        move ? 'svg paths=' + move.querySelectorAll('svg path').length : 'missing');

      // Activation happens on `mousedown`, not `click`.
      //
      // The popup is its own top-level window: when another application takes
      // the foreground between the press and the release, the `click` never
      // arrives and the row does nothing. The press is always delivered to the
      // window under the cursor, so it must be the trigger. This check is the
      // regression guard for that: dispatch only `mousedown` — no `mouseup`, no
      // `click` — and require the action to be sent anyway.
      const target = rows.find((r) => r.dataset.action === 'toggle-muted');
      if (target) {
        // A right-button press must not act: the menu is opened by one and
        // dismissing it by accident on the same button would be a regression.
        target.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, cancelable: true, button: 2 }));
        await new Promise((r) => setTimeout(r, 30));
        log('a right-button press does not activate the row',
          (window.__invoked || []).filter((c) => c.cmd === 'emit_ctx_action').length === 0,
          'emit_ctx_action calls so far: ' + (window.__invoked || []).filter((c) => c.cmd === 'emit_ctx_action').length);

        target.dispatchEvent(new MouseEvent('mousedown', { bubbles: true, cancelable: true, button: 0 }));
        // `run()` is async: give its invokes a turn to land.
        await new Promise((r) => setTimeout(r, 50));
        const sent = (window.__invoked || []).filter((c) => c.cmd === 'emit_ctx_action');
        log('a press alone activates the row (no click needed)',
          sent.length === 1 && sent[0].args.action === 'toggle-muted',
          'emit_ctx_action calls: ' + JSON.stringify(sent.map((c) => c.args.action)));
      } else {
        log('a right-button press does not activate the row', false, 'no toggle-muted row');
        log('a press alone activates the row (no click needed)', false, 'no toggle-muted row');
      }
    } catch (e) {
      log('probe threw', false, (e && (e.message || e)) || e);
    }
    report();
  }

  // The page reports from inside its own init(), after list_apps resolves and
  // the menu is built. Give that promise a turn to settle.
  setTimeout(run, 400);
})();
