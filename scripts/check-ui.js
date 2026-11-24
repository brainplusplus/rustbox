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

// ---- the windows with inline scripts of their own ---------------------
// Missed until now: a syntax error there leaves a dialog that never appears,
// and nothing else in the build would catch it.
for (const file of ['link-choice.html', 'move.html']) {
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

// ---- reordering: pointer events, not HTML5 drag and drop ---------------
// The CEF engine reports `dragstart`/`dragover` to the page but never the
// `drop` the OS drag loop owes it, so a drag-and-drop reorder looks alive and
// commits nothing. Both lists therefore reorder from pointer events, and a
// reintroduced HTML5 drag handler would silently break reordering on CEF again.
if (!/function enablePointerReorder\(/.test(js)) {
  fail('ui/index.html has no enablePointerReorder(), so the lists cannot be reordered on CEF');
}
for (const surface of ['app-list', 'tab-strip']) {
  if (!new RegExp(`container:\\s*(list|strip)`).test(js)) {
    fail('enablePointerReorder() is not wired to a list container');
  }
}
for (const handler of ['dragstart', 'dragover', 'dragend']) {
  if (new RegExp(`addEventListener\\('${handler}'`).test(js)) {
    fail(
      `ui/index.html still listens for '${handler}' to reorder: the CEF engine never ` +
        'delivers the matching drop, so that path cannot commit a reorder',
    );
  }
}
if (/draggable="true"/.test(html)) {
  fail('ui/index.html marks an element draggable="true"; reordering does not use HTML5 drag and drop');
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

// ---- the data-folder move window ---------------------------------------
// The move runs in a session of its own, before the app has a window, so its
// page cannot share any of the checks above. It has its own failure mode
// instead: a page whose backend commands were renamed keeps building and then
// shows a window that never moves off 0%.
const moveHtml = fs.readFileSync(path.join(ui, 'move.html'), 'utf8');
const moveRs = fs.readFileSync(path.join(__dirname, '..', 'src-tauri', 'src', 'move_window.rs'), 'utf8');

if (!/mod move_window;/.test(mainRs)) fail('main.rs has no `mod move_window;`');
if (!/move_window::take\(\)/.test(mainRs)) {
  fail('main.rs never asks move_window whether this launch is a move; nothing would ever run the copy');
}
if (!/move_window::run\(/.test(mainRs)) {
  fail('main.rs never hands over to move_window::run, so the window is configured but never built');
}
// The move has to come before anything resolves the data root, or the copy
// would run against a tree the process already holds open.
const takeAt = mainRs.indexOf('move_window::take()');
const startupAt = mainRs.indexOf('data_root::startup()');
if (takeAt < 0 || startupAt < 0) fail('main.rs is missing the move branch or the root startup');
else if (takeAt > startupAt) {
  fail('main.rs calls data_root::startup() before the move window; the copy would run with the root already frozen');
}

// Every command the page invokes must be declared in the move session, and the
// move session must not ship commands the page never asks for.
const moveCalls = [...moveHtml.matchAll(/invoke\('(\w+)'\)/g)].map((m) => m[1]);
if (!moveCalls.length) fail('ui/move.html invokes nothing; the buttons would be dead');
const moveDeclared = [...moveRs.matchAll(/#\[tauri::command\]\s*\n\s*fn (\w+)/g)].map((m) => m[1]);
if (!moveDeclared.length) fail('move_window.rs declares no #[tauri::command] functions');
const handlers = moveRs.match(/generate_handler!\[([\s\S]*?)\]/);
if (!handlers) fail('move_window.rs has no generate_handler! list');
const registered = handlers[1].split(',').map((s) => s.trim()).filter(Boolean);
for (const name of moveCalls) {
  if (!moveDeclared.includes(name)) fail(`ui/move.html invokes '${name}' but move_window.rs does not declare it`);
  if (!registered.includes(name)) fail(`move_window.rs declares ${name} but never registers it in generate_handler!`);
}
for (const name of registered) {
  if (!moveCalls.includes(name)) fail(`move_window.rs registers ${name} but ui/move.html never calls it`);
}

// The session must not keep its profile in the folder it is copying: a webview
// profile inside the tree holds handles on files the copy is reading.
if (!/app_directories_override/.test(moveRs)) {
  fail('move_window.rs does not redirect the webview profile, so the move session writes into the data folder it is copying');
}
if (!/scratch_dir\(\)/.test(moveRs) || !/temp_dir\(\)/.test(moveRs)) {
  fail('move_window.rs keeps no scratch directory under the temp folder');
}
// - A window that reports progress but cannot be stopped is a trap on a 12 GB
//   copy. - A finished move that leaves the app closed looks like a crash.
if (!/app\.restart\(\)/.test(moveRs)) {
  fail('move_window.rs never restarts into the app; the user would be left with a closed window after a finished move');
}

// The outcome has to survive the restart that ends the move, which means both
// ends exist: the shell asks, and the backend answers.
for (const needle of ["invoke('data_move_result')", "invoke('dismiss_data_move_result')"]) {
  if (!html.includes(needle)) fail(`ui/index.html never calls ${needle}`);
}
if (!/fn data_move_result\(/.test(mainRs)) fail('main.rs has no data_move_result command');
if (!/fn dismiss_data_move_result\(/.test(mainRs)) {
  fail('main.rs has no dismiss_data_move_result; the strip could never be closed');
}
if (!/fn restart_for_data_move\(/.test(mainRs)) {
  fail('main.rs has no restart_for_data_move command');
}
// The settings pane is the only place a queued move can be reported: a launch
// that finds one boots the moving window instead of this page, so a strip here
// would be a branch nothing can reach.
if (!settings.includes("invoke('restart_for_data_move')")) {
  fail('ui/settings.js has no restart button for a queued move');
}

console.log(
  `OK: inline script parses (${js.length} chars), settings.js and chat.js parse, ` +
    `${tabIds.length} settings tabs all have a pane, ${cssClasses.length} CSS classes exist, ` +
    `and a proxy edit rebuilds the tab, and the move window is wired both ways`
);
