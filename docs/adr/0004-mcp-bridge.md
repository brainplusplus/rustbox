# ADR 0004: MCP Bridge for Agent Access to Tab Browsers

**Status:** Accepted (2026-10-20)

## Context

Users asked to let an AI agent drive the tabs rustbox already holds — open URLs, read
console output, inspect and block network requests, take screenshots, run scripts. The
tabs are already logged in to real services, which is exactly what makes the capability
useful and exactly what makes it dangerous.

Four facts constrained the design:

1. **Only the app process holds a `WebView2` controller.** A separate process cannot reach
   an existing webview, so the app has to be the one that talks to the browser.
2. **`--remote-debugging-port` is unauthenticated.** WebView2 exposes it without any token;
   any local program could attach. It would also break the profile-sharing invariant,
   because every webview in one user-data folder must be created with identical
   environment options (see ADR 0002).
3. **`with_webview` is fire-and-forget.** It posts a message to the window's event loop and
   returns; the closure runs later on the main thread. Calling it from an IPC command
   handler and then blocking is the wry#583 reentrancy deadlock.
4. **Release builds are `windows_subsystem = "windows"`.** There is no console, so a failed
   start is invisible unless it is logged to a file.

## Decision

Expose the tabs through a **loopback bridge inside the app**, fronted by a separate
**stdio MCP server**.

```text
MCP client <--stdio--> rustbox-mcp.exe <--127.0.0.1--> rustbox <--CDP--> tab webview
```

- **`src-tauri/src/cdp.rs`** owns the CDP layer:
  - `call()` posts to the main thread via `with_webview`, installs a
    `CallDevToolsProtocolMethodCompletedHandler`, and blocks the *caller* on a channel.
    Blocking the caller and never the main thread is what keeps it deadlock-free.
  - `evaluate()` wraps `Runtime.evaluate` with `returnByValue`, `awaitPromise`, and
    `userGesture`.
  - `subscribe()` installs one `ICoreWebView2DevToolsProtocolEventReceiver` per event and
    keeps them alive in a **thread-local** (they are COM objects bound to the UI thread).
  - `enable_domains()` sends `Runtime.enable` / `Network.enable`, gated by `set_capture()`
    so a session that never uses the bridge pays nothing for it.
  - Console and network events land in capped ring buffers (`EventBuffer`, 500 entries)
    with a monotonic cursor, so an agent can ask for "everything since 42" instead of
    polling and de-duplicating.
- **`src-tauri/src/mcp.rs`** owns the bridge: a `127.0.0.1` listener on an ephemeral port,
  one JSON object per line, a per-launch token, and an endpoint descriptor at
  `<data-root>/mcp-endpoint.json`.
- **`src-tauri/src/bin/mcp.rs`** is the MCP server: `initialize`, `tools/list`,
  `tools/call`, `ping`. It forwards each call to the bridge and translates a screenshot
  into MCP's first-class `image` content type.

## Why a line protocol instead of HTTP

The only client is the bundled shim. A line protocol has no header-parsing surface, is
trivial to audit, and cannot be reached by a web page performing a cross-origin `fetch` —
that would send an HTTP request, not a bare line. No HTTP server dependency is added.

## Security posture

- **Off by default.** Nothing listens unless `RB_MCP=1` is set or the persisted
  `mcp_enabled` toggle is on.
- **Loopback only**, bound to `127.0.0.1` on an ephemeral port.
- **Per-launch token**, 128 bits from the OS, compared without early exit so a wrong token
  cannot be discovered byte by byte.
- **Token file permissions**: `0600` on Unix; `%LOCALAPPDATA%` is already per-user ACL on
  Windows.
- **Token never reaches the UI.** `mcp_state` reports only whether the bridge is up and
  where the endpoint file is, because a token inside a webview is a token in reach of any
  script that page runs.
- **Regenerated every launch**, and the descriptor is deleted on a clean exit. A hard kill
  skips that hook, so the shim deletes a descriptor whose port refuses connections and
  reports "rustbox is not running" instead of dialling a dead port forever.

## Rejected alternatives

- **`--remote-debugging-port` on the tabs.** Unauthenticated, and would force identical
  environment options across webviews sharing a data folder, breaking ADR 0002.
- **Existing Tauri MCP plugins** (`tauri-plugin-mcp`, `tauri-mcp`, `hypothesi/mcp-server-tauri`).
  All target the app's *own* webview for dev-time debugging and refuse release builds.
  They cannot see a second webview, let alone a profile-isolated one.
- **Blocking inside an IPC command handler.** That is the wry#583 deadlock. `tab_eval` and
  `tab_cdp` are `async` and use `spawn_blocking`.

## Screenshots of hidden tabs

`Page.captureScreenshot` with `fromSurface: false` captures the already-presented surface,
which is **blank** for a tab that has not painted since it was occluded — verified: 3.1 KB
of white for a hidden WhatsApp tab. `fromSurface: true` makes the browser compositor
produce the frame, which renders correctly (103 KB, real content). The tool therefore sends
`fromSurface: true, captureBeyondViewport: true`.

## Invariants

- A CDP call never runs on the main thread's message pump.
- Event receivers are never installed twice for one tab (`mark_subscribed` claims first;
  a duplicate would double every log line).
- Buffers are dropped and receivers detached when a tab closes, and the app id is released
  so reopening the same app subscribes again.
- `cdp_result` returns the payload as WebView2 delivers it — the CDP *result object*, with
  no envelope. Callers read the fields their own command defines.
