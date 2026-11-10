# RustBox MCP

An MCP server that lets an AI agent drive the browsers inside RustBox: open URLs, run
scripts in a tab's origin, read console output, inspect and block network requests, and
screenshot tabs (including tabs that are not on screen).

The tabs keep their own logins. That is the point, and also why this is **off by default**.

## How it is wired

```text
MCP client  <--stdio-->  rustbox-mcp.exe  <--127.0.0.1-->  RustBox  <--CDP-->  tab
```

RustBox holds the `WebView2` controllers, so it has to be the process that talks to the
browser. `rustbox-mcp.exe` is a thin MCP server that forwards to it over a loopback socket.
See `docs/adr/0004-mcp-bridge.md` for why `--remote-debugging-port` was rejected.

## Enabling

The bridge is off unless asked for. Either:

- start RustBox with `RB_MCP=1`, or
- turn on `mcp_enabled` in `%LOCALAPPDATA%\rustbox\config.json`.

When the bridge starts it writes `%LOCALAPPDATA%\rustbox\mcp-endpoint.json`:

```json
{ "port": 51421, "token": "<32 hex chars>", "pid": 69280, "version": 1 }
```

The token is regenerated on every launch and the file is removed on a clean exit. Nothing
listens on any other interface than `127.0.0.1`.

Diagnostics go to `%LOCALAPPDATA%\rustbox\mcp.log` (release builds have no console).

## Building the installer

`bundle.externalBin` in `src-tauri/tauri.conf.json` ships the shim, so the installer puts
`rustbox-mcp.exe` next to `rustbox.exe`. That setting has a build-order constraint worth
knowing: `tauri-build` validates the sidecar path (`binaries/rustbox-mcp-<triple>.exe`)
while compiling the app crate, so the shim has to exist before the app is built.

`scripts/build-installer.mjs` handles the order. Use it instead of calling
`cargo tauri build` directly:

```powershell
node scripts/build-installer.mjs
```

It builds the shim, stages it under the triple-suffixed name, then runs
`cargo tauri build --bundles nsis`. `src-tauri/binaries/` is generated output and is
git-ignored. The same script is wired up as `npm run build:installer`.

## Client configuration

Point the MCP client at the shim. For Claude Desktop / Cursor / Droid:

```json
{
  "mcpServers": {
    "rustbox": {
      "command": "C:\\Program Files\\RustBox\\rustbox-mcp.exe"
    }
  }
}
```

No arguments and no environment variables: the shim reads the endpoint file itself, so it
follows the app across restarts and port changes.

## Tools

| Tool | What it does |
| --- | --- |
| `list_tabs` | Every open app: `appId`, its active `tabId`, the URL, and a `subTabs` array of every tab it has open. Call this first. |
| `open_sub_tab` | Open another tab in an app, sharing its cookies, storage and proxy. Optional `url`. Requires app edits. |
| `close_sub_tab` | Close one sub-tab by `tabId`. The pinned first tab cannot be closed. Requires app edits. |
| `activate_tab` | Bring an app forward, optionally selecting one of its sub-tabs. |
| `set_link_target` | Choose what a `target="_blank"` link does: `ask`, `same`, `new-tab`, `popup`, `external`. Requires app edits. |
| `tab_eval` | Run a JS expression in the tab's own origin. Promises are awaited, so `await fetch('/api')` works. |
| `tab_cdp` | Send any CDP command, e.g. `DOM.getDocument`, `Network.setBlockedURLs`, `Emulation.setDeviceMetricsOverride`. |
| `tab_console` | Console messages and uncaught exceptions. Pass `since` with a cursor for only new entries. |
| `tab_network` | Requests with method, URL, resource type, status, and failure reason. Pass `since` for only new ones. |
| `tab_navigate` | Point a tab at a URL. A bare host is upgraded to `https`. |
| `tab_screenshot` | PNG of the tab's viewport as MCP image content. Works for hidden tabs. |
| `bridge_status` | Whether the bridge is reachable, and where the endpoint file lives. |

Every tab-scoped tool takes an optional `tabId`. Omitting it means the app's active
sub-tab, which is what a caller that does not know about sub-tabs expects, and what a
single-tab app always wants.

### Sub-tabs

An app can hold several tabs. They share one profile, so a login in one is a login in all
of them, and each keeps its **own** console and network buffers, because those come from
the page and not from the session:

```text
list_tabs                                  -> app "gmail" has tabs t1 (active) and t2
tab_network { appId: "gmail", tabId: "t2" } -> only t2's requests
tab_eval    { appId: "gmail", tabId: "t2" } -> runs in t2, not in what the user sees
```

The app's first tab is pinned: it is the app, so closing it would mean closing the app.
`close_sub_tab` refuses it and says so; use `delete_app` to remove the whole thing.

### Reading event streams

`tab_console` and `tab_network` return a `cursor` with each reply. Store it and pass it back
as `since` next time:

```text
tab_network { appId, since: 0 }   -> cursor 208, entries 0..207
tab_network { appId, since: 208 } -> only what happened after that
```

Each buffer keeps the most recent 500 entries. When `cursor` and the number of retained
entries diverge, older entries have been dropped; re-read with a larger `since` or accept
the gap.

## Capture cost

Installing the event receivers is free. The CDP domains that actually produce events
(`Runtime.enable`, `Network.enable`) are only switched on while the bridge is running, and
are switched on for tabs that were already open at that moment. A normal session with the
bridge off does no extra work.

## Limitations

- **Windows only.** CDP is reachable here because `WebView2` binds it. The macOS and Linux
  backends have no equivalent binding in this app, and the tools report that instead of
  pretending.
- **External-browser apps are out of scope.** Only embedded tabs are reachable; an app
  launched in Chrome or Camoufox is a separate process with no handle to hold.
- **Hidden-tab screenshots** use `fromSurface: true`. This renders via the compositor, so it
  is correct but slightly slower than capturing an already-presented surface.
- **Network correlation is by URL**, not by CDP `requestId`, because the id is never exposed
  to the agent. Two concurrent requests to the same URL can have their statuses swapped.

## Revoking

Turn off `mcp_enabled` in the UI (or stop passing `RB_MCP=1`) and restart.

Turning it off through the UI takes effect immediately: the endpoint file is deleted, the
accept loop drops new connections, and a connection that is already open refuses its next
request. Console and network capture switch off at the same time, so the buffers stop
growing. Because the token is per-launch, a token captured from a previous run is useless
against a later one.
