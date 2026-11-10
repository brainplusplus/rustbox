# ADR 0005 (proposed): Chromium (CEF) as a selectable embedded engine

**Status:** Proposed — 2026-11-10. Nothing implemented. Awaiting approval to start M0.

Relates to: ADR 0001 (engine list), ADR 0002 (isolation and proxy, reused as-is).

---

## 1. Why this exists

`Engine::OsWebview` renders a tab in the platform webview: WebView2 on Windows,
WKWebView on macOS, WebKitGTK on Linux. Two consequences matter here.

- **On macOS a RustBox tab is WebKit, the same engine family as Safari.** The
  user's default browser setting cannot change that: LaunchServices maps a URL to
  an *application*, and WKWebView is not that application. Chrome ships no
  embeddable engine, so "use the OS default browser inside the tab" is not a
  feature that can be built; verified in `wry` 0.57.0, whose macOS backend is
  `objc2_web_kit::WKWebView`.
- **On Windows it is already Chromium** (WebView2). The remaining Windows problem
  is not engine identity but the Wry/WebView2 runtime skew that produces
  `RPC_E_DISCONNECTED` (`webview2_skew.rs`, the blocked-open prompt).

Rambox solves the same problem by being an Electron app and carrying Chromium
itself (verified: `ramboxapp/community-edition/package.json` → `electron 13.6.3`,
`main: electron/main.js`).

## 2. Decision under consideration

Add a third engine value, selectable per app in Settings:

```rust
pub enum Engine {
    OsWebview,   // unchanged: WebView2 / WKWebView / WebKitGTK
    External,    // unchanged: the user's browser, own window
    Cef,         // new: bundled Chromium, rendered inside RustBox
}
```

- The new value is opt-in per app. Every existing config keeps working; the
  serialized values `os-webview` and `external` are untouched.
- Offered on all three platforms, as decided 2026-11-10.
- Produced as **two macOS dmgs (arm64 and x64)** instead of one universal dmg,
  because a universal binary cannot carry a `lipo`-merged CEF framework.

### Non-goals (stated so they are not re-litigated in review)

- **Embedding the user's installed Chrome.** Not possible, not redistributable.
  The Chrome they have is an application, not a renderer with an embedding API.
  CEF is *bundled Chromium*, a different browser with its own login state.
- **Replacing `Engine::External`.** Handing an app to the user's own browser stays
  the cheap answer and keeps its own profile behaviour.
- **Fingerprint seeding inside CEF.** `--fingerprint=` is a stealth-build switch.
  CEF tabs get per-app storage, a proxy and a UA, not identity pinning.

## 3. Verified facts (measured 2026-11-10)

| Fact | Value | Source |
| --- | --- | --- |
| CEF Rust crate | `cef` 156.4.0+156.0.26 (Chromium 156), Apache-2.0 OR MIT | crates.io, published 2026-11-08 |
| CEF targets | Linux, macOS, Windows on x86_64 and ARM64 | cef-rs README |
| macOS CEF `minimal` | **135.1 MB** (archive) | cef-builds index.json, 156.0.27 |
| macOS CEF `standard` | **344.0 MB** (archive) | same |
| Current `universal.dmg` | **31.4 MB** | GitHub release v0.1.1 |
| Current `x64-setup.exe` | **14.0 MB** | same |
| Current `amd64.AppImage` | **92.2 MB** | same |
| `cef::Settings` fields we need | `external_message_pump`, `windowless_rendering_enabled`, `cache_path`, `root_cache_path`, `remote_debugging_port`, `no_sandbox`, `framework_dir_path`, `user_agent` | docs.rs |
| OSR GPU path | `accelerated_osr` feature pulls `wgpu`, `objc2-metal`, `objc2-io-surface` | crates.io features |
| Surface we must cover | `tabs.rs` 2969 lines, 3 `WebviewWindowBuilder` sites, 17 `get_webview_window` uses | this repo |

**Conclusion from the measurements:** a CEF build is 4x-11x the current macOS
download. That is the price of carrying Chromium, and it is not avoidable if the
requirement is "a Chromium tab on macOS".

## 4. Two ways to put CEF inside a RustBox window

Both are real; the plan picks one and keeps the other as fallback.

### Option A — windowed CEF (child native view) — **recommended**

CEF creates a browser in windowed mode, parented into the window RustBox already
allocates for the tab (`CefWindowInfo`, `SetAsChild`). Chromium's own compositor
renders into that child view.

- Pros: native input, IME, scrolling, cursor, drag-and-drop and accessibility come
  for free; no per-frame copy; the existing `CONTENT_RECT` positioning applies.
- Cons: two run loops in one process. On macOS the CEF message loop must be
  pumped from the main thread (`external_message_pump = true` and a timer calling
  `CefDoMessageLoopWork`), and CEF on macOS insists on being the app bundle with
  helper executables. **This is the single biggest risk, and M0 exists to
  answer it.**

### Option B — windowless OSR (off-screen rendering) — fallback

CEF renders into a shared texture; RustBox composites it.

- Pros: no child window, no second run loop ownership question; simpler to
  layer inside a Tauri window; `accelerated_osr` gives a Metal path on macOS.
- Cons: we must synthesize input events, implement IME, scrolling, focus and
  hit-testing, and pay a GPU copy per frame. Weeks of work that Option A avoids.

Decision rule: if M0 shows Option A cannot coexist with tao's run loop, fall back
to B rather than to a separate helper process (a helper process means two windows
to keep positioned, which regresses the window behaviour users already have).

## 5. Code seam

Today `SubTab` owns a `WebviewWindow` (field `window`), and `tabs.rs` calls
webview methods on it throughout. A second backend needs one abstraction.

```rust
/// One tab's rendering surface, whichever backend owns it.
enum Surface {
    System(WebviewWindow),  // wry: WebView2 / WKWebView / WebKitGTK
    Chromium(CefTab),       // CEF
}
```

Method set, derived from what `tabs.rs` actually calls (counted, not guessed):

| Operation | Call sites today |
| --- | --- |
| `close()` | 6 |
| `set_position()` | 5 |
| `set_focus()` | 4 |
| `set_size()` | 4 |
| `hide()` | 3 |
| `show()` | 2 |
| `eval()` | 2 |
| `reload()` | 2 |
| `url()` | 1 |
| `navigate()` | 1 |
| `is_visible()` | 1 |

`SubTab.window` becomes `SubTab.surface: Surface`. Call sites that genuinely need
a real `WebviewWindow` — the Windows-only `with_webview` / `webview2_com` CDP
handlers in `watch_tab_navigation`, and `crate::cdp::call(&win, …)` — move behind
`Surface::with_platform(…)`, so the Windows path keeps its current code.

Two invariants already documented in `tabs.rs` must survive the seam:

1. **URL and title stay cached in `SubTab`.** The existing comment records why:
   `WebviewWindow::url()` posts to the main thread and waits, and calling it while
   the tab registry lock is held deadlocks. CEF's `url()` is also a main-thread
   call, so the cache and the navigation watcher stay.
2. **Hidden is not closed.** `show_only` hides the other tabs and they keep
   running — the reason `webview_args::BASE` exists. A CEF tab must be hidden,
   never closed, and must not be throttled while hidden.

## 6. Reuse, not re-invention

- **Storage:** `paths::embedded_profile(&app_id)` becomes CEF's `cache_path` +
  `root_cache_path` on a per-app `CefRequestContext`. Same folder semantics as
  `data_directory`, so `reset_app_profile` and `purge_app_data` keep working.
- **Proxy:** Chromium's `--proxy-server`; `Proxy::normalized_scheme` already emits
  the `http` / `socks5` forms Chromium accepts. Applied per request context, so it
  cannot leak between apps.
- **CDP:** `crate::cdp` already speaks CDP to WebView2. CEF exposes CDP via
  `remote_debugging_port`; `tab_cdp` / `tab_eval` / console / network keep their
  signatures, so `docs/mcp.md` does not change.
- **UI:** `Engine` gets one more `<option>`; the browser picker stays disabled for
  both embedded engines, since neither launches an external program.

## 7. Milestones and gates

Every gate needs evidence, and macOS gates need the user's Mac or a macOS CI
runner. I cannot verify macOS from this Windows machine and will not claim to.

| # | Work | Gate | Where |
| --- | --- | --- | --- |
| M0 | Spike outside RustBox: `cef` crate, one windowed browser, one page, message pump from a plain loop; also try it alongside a tao window | Whether A or B; `minimal` vs `standard` sufficient (PDF/print/DRM); measured startup cost | user's Mac |
| M1 | `Surface` seam, `System` only, zero behaviour change | `cargo test --release` and `check-ui` green; no config can select CEF | Windows |
| M2 | `Surface::Chromium`: navigate, size, show/hide, close, title, URL | A CEF tab renders `example.com` inside the RustBox window | user's Mac |
| M3 | Per-app `CefRequestContext`, proxy, user agent | Login survives restart; proxy probe (ADR 0002's `scripts/probe-proxy.ps1` equivalent) shows the request | user's Mac |
| M4 | Link chooser (`OnBeforePopup`), link targets, downloads, devtools, CDP | All four chooser answers work; `tab_cdp` answers | user's Mac |
| M5 | Windows | Same checks; skew prompt no longer applies to CEF tabs | Windows |
| M6 | Linux | Same checks | Linux |
| M7 | Packaging: two dmgs, framework in bundle, signing, notarization, CI split | A downloaded arm64 dmg opens on a Mac that never had RustBox | CI + user's Mac |

M0 and M1 are cheap and de-risking: M0 answers "does this work at all", M1 is a
mechanical refactor proven by the existing suite. Neither adds a byte to a
release artefact.

## 8. Packaging consequences (macOS)

- `Chromium Embedded Framework.framework` goes in `RustBox.app/Contents/Frameworks/`
  with `@rpath` links; helper apps are required by CEF's layout, not optional.
- Hardened runtime needs `com.apple.security.cs.allow-jit` and
  `com.apple.security.cs.allow-unsigned-executable-memory` for V8, plus
  `com.apple.security.inherit` on helpers.
- **Code signing and notarization stop being optional.** Today the macOS build is
  unsigned and Gatekeeper merely warns; an unsigned CEF build does not launch at
  all. A Developer ID is a prerequisite for M7.
- `/Applications` install matters: CEF resolves the framework relative to the
  bundle, and a translocated/quarantined app breaks that. The installer may need
  to force a move out of the DMG path.
- Third-party licence notices for Chromium and CEF must ship with the bundle.

Windows and Linux grow comparably: the NSIS installer moves from 14 MB into the
hundreds of MB, and the AppImage from 92 MB upward.

## 9. Risks

| Risk | Severity | Mitigation |
| --- | --- | --- |
| CEF's loop fights tao's on macOS | High | M0 decides A vs B before any code depends on it |
| We take over Chromium's security cadence | High | Standing commitment: pinned CEF version, rebuild on Chromium security releases. This is the cost that does not go away |
| Signing/notarization of helpers and entitlements | High | Needs a Developer ID; solve in M7 before any public release |
| Download size grows 4x-11x | Medium | Accepted by the decision; per-arch dmg; `minimal` distribution if it suffices |
| Two webview stacks, two crash surfaces | Medium | CEF's own helper processes; a CEF crash must not take the shell down — verify in M2 |
| Focus, keyboard, IME, drag-drop regressions in CEF tabs | Medium | Each is an explicit check in M2/M4, not an assumption |
| `tabs.rs` is 2969 lines and the seam touches its core | Medium | M1 lands first, behaviour-preserving, tests green |
| Cannot verify macOS from here | High | macOS gates run on the user's Mac or CI; no claim without one |

## 10. Open questions for M0

1. Option A or B on macOS? (run loop coexistence)
2. `minimal` distribution enough, or is `standard` needed for PDF/print/DRM?
3. Does CEF change app startup time noticeably?
4. Does the existing WebView2 path stay the default everywhere, including Windows,
   or should CEF become the Windows default once proven? (Default stays
   `os-webview` until M5 evidence exists.)
5. Developer ID availability and timing, since M7 cannot ship without it.
