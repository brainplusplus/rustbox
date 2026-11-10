# ADR 0002 (rev.2): Per-Tab Session Isolation & Proxy Model

**Status:** Accepted (2026-10-20, supersedes rev.1 — Servo references removed per ADR 0001 rev.2)

> Planned amendment (see `docs/adr/0005-embedded-chromium-cef.md`): if a `cef`
> engine is added, a CEF tab would receive the same per-app storage folder and the
> same proxy through a per-context setting instead of WebView2's data directory
> and switches. The rules below — one store per app, per-tab proxy, immutable
> while open — are unchanged.

## Decision
- Every app tab gets its own `Session` = isolated cookie/storage partition (one user-data folder per session id).
- **Proxy is configured per tab**, stored in the app's config JSON: `{ scheme, host, port, auth? }`.
  - **Embedded (OS webview / WebView2), `Isolated` mode:** the configured proxy is applied to the tab's own webview environment (`WebviewWindowBuilder::proxy_url` → wry `--proxy-server` in that environment's options). Works because `Isolated` gives every app its own data directory, so each environment is created once and never has to agree with another. Verified end-to-end: an app pointed at an unresolvable host through a local logging proxy produces the request in the proxy's log (`scripts/probe-proxy.ps1`).
  - **Embedded, `Light` mode:** the proxy is *not* applied. Light shares one browser environment with the shell, and WebView2 refuses a webview whose options differ from those already in use for the same folder (HRESULT 0x8007139F). The UI warns about this instead of silently doing nothing.
  - **External browser (Chromium-family: Brave, Chrome, Edge, Vivaldi, Opera):** proxy via `--proxy-server=scheme://host:port`, plus `--user-data-dir` per session for isolation.
  - **External, no browser chosen but proxy set:** autodetect first installed Chromium-family browser so proxy flags apply; fallback = OS default browser without proxy (documented limitation).
  - **External Firefox:** `-no-remote --new-instance -profile <dir>` for isolation. Firefox has no `--proxy-server`, so the proxy is written into the profile's `user.js` as `network.proxy.*` prefs. Verified: Camoufox (a Firefox build) absorbs the file into `prefs.js` on start. `user.js` is rewritten on every launch, including the no-proxy case (`network.proxy.type = 5`), because it is replayed over `prefs.js` and a stale proxy would otherwise stick forever.
  - **External Camoufox:** treated as Firefox for isolation and proxy. A fingerprint seed is *not* honoured: Camoufox generates its identity in its Python wrapper, not from a CLI flag, so the launcher reports that instead of silently ignoring the field.
  - **Fingerprint seed:** sent as `--fingerprint=<seed>` only to stealth Chromium builds (currently recognised by name). Plain Chrome ignores unknown switches, so the launcher reports when a seed was not applied.
  - **External via OS default browser:** no flags possible; no proxy control — documented limitation.

## Scheme normalisation
WebView2 and wry only parse `http` and `socks5`. `https` is normalised to `http` and `socks` to `socks5` at the point of use (`Proxy::normalized_scheme`), so a config that names a scheme the engine cannot read does not silently end up with no proxy.

## Invariant
A tab's engine + proxy + session are immutable while the tab is open. Changing any of them recreates the tab.
