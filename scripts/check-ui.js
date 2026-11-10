// Syntax-checks the UI's scripts and asserts the wiring between them.
//
// The UI has no build step and no bundler, so a stray bracket is only discovered
// when the window comes up blank. `new Function` compiles a script without
// running it, which catches exactly that class of mistake. The separate files
// are checked with `node --check` semantics via the same compiler.
const fs = require('fs');
const path = require('path');

const ui = path.join(__dirname, '..', 'ui');

function fail(message) {
  console.error(`FAIL: ${message}`);
  process.exit(1);
}

/** Compile a script without running it. */
function parses(source) {
  new Function(source);
}

// ---- ui/index.html: the inline bootstrap --------------------------------
const htmlFile = path.join(ui, 'index.html');
const html = fs.readFileSync(htmlFile, 'utf8');

const marker = "'use strict';";
const start = html.indexOf(marker);
if (start < 0) fail('no inline script found in ui/index.html');
const end = html.lastIndexOf('</script>');
const js = html.slice(start, end);

try {
  parses(js);
} catch (e) {
  fail(`inline script does not parse: ${e.message}`);
}

// ---- the two surfaces that live in their own files ---------------------
for (const file of ['settings.js', 'chat.js']) {
  const full = path.join(ui, file);
  if (!fs.existsSync(full)) fail(`ui/${file} is missing`);
  try {
    parses(fs.readFileSync(full, 'utf8'));
  } catch (e) {
    fail(`ui/${file} does not parse: ${e.message}`);
  }
}

// ---- the link chooser, which has an inline script of its own -----------
// Missed until now: a syntax error there leaves a dialog that never appears,
// and nothing else in the build would catch it.
for (const file of ['link-choice.html']) {
  const full = path.join(ui, file);
  if (!fs.existsSync(full)) fail(`ui/${file} is missing`);
  const source = fs.readFileSync(full, 'utf8');
  const from = source.indexOf("'use strict';");
  if (from < 0) fail(`ui/${file} has no inline script`);
  try {
    parses(source.slice(from, source.lastIndexOf('</script>')));
  } catch (e) {
    fail(`ui/${file} inline script does not parse: ${e.message}`);
  }
}

// ---- the three surfaces are wired --------------------------------------
// A missing one leaves a button that does nothing, which is worse than no
// button at all.
const required = [
  'id="settings-btn"',
  'id="chat-btn"',
  'id="settings-page"',
  'id="chat-panel"',
  "invoke('mcp_state')",
  'refreshMcpState()',
  'window.RustBoxUI',
  'window.RustBoxSettings',
  'window.RustBoxChat',
  '<script src="settings.js">',
  '<script src="chat.js">',
];
for (const needle of required) {
  if (!html.includes(needle)) fail(`ui/index.html is missing ${needle}`);
}

// Agent access lives only in Settings now. The sidebar button was removed, so a
// reappearing `#mcp-btn` means a second entry point for one setting came back.
if (html.includes('id="mcp-btn"')) {
  fail('ui/index.html still has the sidebar #mcp-btn; agent access belongs in Settings only');
}

// ---- settings.js: every tab has a pane, and every pane has a builder ----
const settings = fs.readFileSync(path.join(ui, 'settings.js'), 'utf8');

// The rail is built from TABS, and render() looks the builder up by the same
// id. A tab with no builder renders an empty pane; a builder with no tab is
// dead code. Both are silent failures, so they are checked against each other.
const tabsMatch = settings.match(/const TABS = \[([\s\S]*?)\];/);
if (!tabsMatch) fail('settings.js has no TABS list');
const tabIds = [...tabsMatch[1].matchAll(/id:\s*'([a-z-]+)'/g)].map((m) => m[1]);
if (!tabIds.length) fail('settings.js TABS list is empty');

const builderBlock = settings.match(/const builders = \{([\s\S]*?)\n\s*\};/);
if (!builderBlock) fail('settings.js has no builders map');
const builtIds = [...builderBlock[1].matchAll(/(\w+):\s*(\w+)/g)].map((m) => m[1]);

for (const id of tabIds) {
  if (!builtIds.includes(id)) fail(`settings.js has a "${id}" tab but no builder for it`);
  if (!new RegExp(`${id}:\\s*pane\\w+`).test(builderBlock[1])) {
    fail(`settings.js builder for "${id}" does not point at a pane function`);
  }
}
for (const id of builtIds) {
  if (!tabIds.includes(id)) fail(`settings.js builds a "${id}" pane that has no tab`);
}

// The three panes this build adds. Named individually because a rail entry that
// opens an empty pane looks like a working tab in a screenshot.
for (const [fn, id] of [
  ['paneRouter', 'router'],
  ['paneCombo', 'combo'],
  ['paneLogs', 'logs'],
]) {
  if (!new RegExp(`function ${fn}\\(`).test(settings)) fail(`settings.js has no ${fn}()`);
  if (!tabIds.includes(id)) fail(`settings.js TABS has no "${id}" entry`);
}

// The router page's contract with the backend: every command it calls must
// exist, or the button fails at click time instead of at build time.
const routerCommands = [
  'router_state',
  'set_router_enabled',
  'set_router_tunnel',
  'set_router_tailscale',
  'router_logs',
  'clear_router_logs',
  'new_router_key',
];
for (const command of routerCommands) {
  if (!settings.includes(`'${command}'`)) fail(`settings.js never calls ${command}`);
}

// ---- ui/settings.css: the classes the new panes use --------------------
const css = fs.readFileSync(path.join(ui, 'settings.css'), 'utf8');
const cssClasses = [
  'sp-endpoint',
  'ep-row',
  'ep-dot',
  'sp-prefix-group',
  'sp-prefix-preview',
  'sp-tunnel',
  'sp-check-item',
  'sp-chart',
  'sp-stats',
  'sp-bars',
  'sp-logtable',
  'sp-logrow',
  'sp-logerror',
  'sp-rule-hint',
];
for (const cls of cssClasses) {
  if (!css.includes(`.${cls}`)) fail(`ui/settings.css has no .${cls} rule`);
}

// ---- editing a proxy must rebuild the tab ------------------------------
// The proxy is baked into the webview when it is built (`--proxy-server` in the
// argument string, plus the environment choice), so changing it in the form
// cannot reach a tab that is already open: config.json would say one thing and
// the running tab would keep using the old endpoint, or none. Every property
// with that constraint has to be in the close condition, which is exactly what
// was missing for the proxy.
const rebuildMatch = js.match(/if \(([^)]*?)\) \{\s*\n\s*await invoke\('close_embedded_tab'/);
if (!rebuildMatch) fail('ui/index.html has no close-and-rebuild branch on save');
const rebuildCondition = rebuildMatch[1];
for (const prop of ['wasEngine', 'wasProfileMode', 'proxyChanged']) {
  if (!rebuildCondition.includes(prop)) {
    fail(`the save path does not rebuild the tab when ${prop} changes: ${rebuildCondition}`);
  }
}
if (!/function proxySignature\(app\)/.test(js)) {
  fail('ui/index.html has no proxySignature(); the proxy comparison has nothing to compare');
}

// Exercise the comparison itself: it is a pure function, so the decision that
// tears a tab down can be tested without a webview. Getting this wrong in the
// safe direction costs a reload; getting it wrong in the other direction leaves
// the tab on the old proxy while the form says the new one.
const sigMatch = js.match(/function proxySignature\(app\) \{[\s\S]*?\n\}/);
if (!sigMatch) fail('could not extract proxySignature from ui/index.html');
const proxySignature = new Function(`${sigMatch[0]}; return proxySignature;`)();

const same = proxySignature({
  proxy_mode: 'custom',
  proxy: { scheme: 'http', host: '1.1.1.1', port: 8080, username: 'u', password: 'p' },
});
const reSaved = proxySignature({
  proxy_mode: 'custom',
  proxy: { scheme: 'http', host: '1.1.1.1', port: 8080, username: 'u', password: 'p' },
});
const otherHost = proxySignature({
  proxy_mode: 'custom',
  proxy: { scheme: 'http', host: '2.2.2.2', port: 8080, username: 'u', password: 'p' },
});
const otherPass = proxySignature({
  proxy_mode: 'custom',
  proxy: { scheme: 'http', host: '1.1.1.1', port: 8080, username: 'u', password: 'q' },
});
const cleared = proxySignature({ proxy_mode: 'none', proxy: null });
const inherited = proxySignature({ proxy_mode: 'inherit', proxy: null });

if (same !== reSaved) fail('re-saving an unchanged proxy would rebuild the tab for nothing');
if (same === otherHost) fail('changing the proxy host would not rebuild the tab');
if (same === otherPass) fail('changing the proxy password would not rebuild the tab');
if (cleared === inherited) fail('switching custom -> none would not rebuild the tab');
if (same === cleared) fail('switching custom -> none would not rebuild the tab');

// A Settings save that moves the global proxy or a pool must reach the tabs
// that follow it. Both halves have to exist: the event listener here, and the
// emit in main.rs. Either one missing is a silent no-op.
if (!/listen\('proxy-changed'/.test(js)) {
  fail("ui/index.html never listens for 'proxy-changed', so a global proxy edit leaves tabs on the old endpoint");
}
if (!/rebuildTabsForProxy\(/.test(js)) {
  fail('ui/index.html has no rebuildTabsForProxy(), so nothing rebuilds the stale tabs');
}
const mainRs = fs.readFileSync(path.join(__dirname, '..', 'src-tauri', 'src', 'main.rs'), 'utf8');
if (!/emit\("proxy-changed"/.test(mainRs)) {
  fail('src-tauri/src/main.rs never emits "proxy-changed" after save_settings');
}

console.log(
  `OK: inline script parses (${js.length} chars), settings.js and chat.js parse, ` +
    `${tabIds.length} settings tabs all have a pane, ${cssClasses.length} CSS classes exist, ` +
    `and a proxy edit rebuilds the tab`
);
