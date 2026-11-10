# ADR 0001 (rev.2): Shell & Engine Architecture

**Status:** Accepted (2026-10-20, supersedes rev.1 — Servo dropped per user decision)

## Context
Rambox-style multi-app workspace with selectable "engine" per app tab. Original plan included Servo via Verso; user dropped it (compat/maintenance burden).

## Decision
- **Shell:** Tauri v2.
- **Default engine:** `external` + OS default browser (no flags, no proxy control — documented limitation). Per user decision 2026-10-20.
- **External with explicit browser:** user picks an installed browser (Brave, Chrome, Firefox, ...) or a custom executable path; enables per-tab proxy flags (Chromium-family) and profile isolation.
- **Embedded engine:** OS webview (`os-webview`) — WebView2 on Windows, WKWebView on macOS, WebKitGTK on Linux — opt-in per app tab.
- **Per-tab proxy:** implemented at the native webview layer for embedded tabs (WebView2: per user-data-folder environment options). For `external` browser tabs, proxy is passed via browser flags (`--proxy-server=...`) for Chromium-family; Firefox-family: profile isolation only, proxy flag unsupported upstream.
- **Domain terms:** `App` (entry), `Session` (isolated storage partition), `Engine` (`os-webview` | `external`), `Proxy` (per tab).

## Consequences
- No Servo dependency; codebase smaller, compat surface = whatever OS webview + user's own browsers support.
- External-browser proxy only reliably works with Chromium-family browsers; document limitation.
- Engine/proxy/session of a tab are immutable while open; changing them recreates the tab.
