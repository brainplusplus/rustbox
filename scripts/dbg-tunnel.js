// Throwaway: log every invoke the page makes, then click the tunnel toggle.
(function () {
  'use strict';
  const out = [];
  const wait = (ms) => new Promise((r) => setTimeout(r, ms));
  const q = (s) => document.querySelector(s);
  const log = [];
  const core = window.__TAURI__ && window.__TAURI__.core;
  if (core && core.invoke) {
    const original = core.invoke;
    core.invoke = async (cmd, args) => {
      log.push('-> ' + cmd);
      try {
        const value = await original(cmd, args);
        log.push('<- ' + cmd);
        return value;
      } catch (e) {
        log.push('!! ' + cmd + ': ' + e.message);
        throw e;
      }
    };
  }

  async function run() {
    try {
      for (let i = 0; i < 400 && typeof window.RustBoxSettings !== 'object'; i++) await wait(10);
      out.push('settings=' + typeof window.RustBoxSettings);
      await window.RustBoxSettings.open();
      await wait(300);
      q('.sp-rail-item[data-tab="router"]').click();
      await wait(150);
      log.length = 0;
      out.push('btn=' + JSON.stringify(q('#tunnel-toggle').textContent.trim()));
      q('#tunnel-toggle').click();
      await wait(500);
      out.push('log=' + JSON.stringify(log));
      out.push('btnAfter=' + JSON.stringify((q('#tunnel-toggle') || {}).textContent));
      out.push('rows=' + document.querySelectorAll('#sp-pane .ep-row').length);
      out.push('errors=' + JSON.stringify(window.__probeErrors || []));
    } catch (e) {
      out.push('THREW ' + e.message + '\n' + e.stack);
    }
    document.getElementById('probe').textContent = out.join('\n');
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', () => { run(); });
  } else {
    run();
  }
})();
