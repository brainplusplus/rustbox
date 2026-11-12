# ADR 0005 (proposed): Chromium as a selectable embedded engine (CEF)

**Status:** Proposed — nothing implemented.
**Date:** 2026-11-10.
**Relates to:** ADR 0001 (engine list), ADR 0002 (isolation + proxy, reused unchanged).

## 1. Problem

`Engine::OsWebview` renders a tab in the platform webview: WebView2 on Windows,
WKWebView on macOS, WebKitGTK on Linux. On macOS that is **WebKit, the same
engine family as Safari** — verified in `wry` 0.57.0, whose macOS backend binds
`objc2_web_kit::WKWebView`. The operating system's "default browser" setting
cannot change this. `open https://…` opening Chrome proves only that
LaunchServices maps a URL to an *application*; it says nothing about which engine
renders inside another app's window. Chrome ships no embeddable engine and is not
redistributable, so "embedded tab renders with the user's Chrome" is not
implementable on any platform.

The requested outcome is therefore narrower and achievable: **RustBox carries its
own Chromium**, and an app can choose it per app. This is the Electron model —
Rambox, the app this project is measured against, does exactly that (verified:
`ramboxapp/community-edition/package.json` → `"electron": "13.6.3"`,
`"main": "electron/main.js"`).

## 2. Decision under consideration

CEF is **not** a fourth engine. It is a second *backend* inside the existing
embedded engine, chosen per app — "embedded, rendered by the built-in webview or
by CEF".

```rust
pub enum Engine {
    OsWebview,   // unchanged: embedded, in the RustBox window
    External,    // unchanged: the user's browser, own window
}

/// New, only meaningful when `engine == OsWebview`.
pub enum EmbeddedBackend {
    System,      // default: WebView2 / WKWebView / WebKitGTK
    Chromium,    // new: bundled Chromium (CEF), same tab, same window
}

pub struct App {
    // ...
    #[serde(default)]
    pub embedded_backend: EmbeddedBackend,
}
```

- `Engine` keeps exactly two values, so no serialized config changes meaning and
  no migration is needed. `#[serde(default)]` makes every existing `config.json`
  read as `System`, i.e. today's behaviour.
- The Settings form shows the backend picker only when the engine is
  `os-webview`, and the browser picker stays disabled for both — neither launches
  an external program.
- The invariant from ADR 0002 extends naturally: changing the backend recreates
  the tab, the same way changing the engine already does.
- Available on **Windows, macOS and Linux** (decided 2026-11-10). On Windows the
  win is not "Chromium instead of Chromium" — Windows already has Chromium
  through WebView2 — but escaping the WebView2 runtime-skew failure class
  (`RPC_E_DISCONNECTED`, `webview2_skew.rs`) and presenting a plain Chromium
  identity rather than Edge. ADR 0006 reduces the urgency of the first half: that
  failure class is now handled by pinning the runtime, so CEF no longer has to
  justify itself on stability alone.
- macOS ships as **two dmgs (arm64, x64)**, not one universal dmg: a universal
  binary cannot carry a `lipo`-merged CEF framework (decided 2026-11-10).

### Non-goals

- Embedding the user's installed Chrome. Not possible; see §1.
- Replacing `Engine::External`.
- Fingerprint seeding inside CEF (`--fingerprint=` is a stealth-build switch).
  CEF tabs get per-app storage, proxy and UA — not identity pinning.

## 3. Verified facts

Measured 2026-11-10, not recalled:

| Fact | Value |
| --- | --- |
| CEF crate | `cef` 156.4.0+156.0.26 (Chromium 156), Apache-2.0 OR MIT, published 2026-11-08 |
| CEF targets | Linux, macOS, Windows on x86_64 **and** ARM64 |
| Dependency resolution on this toolchain | resolves: 162 packages locked, rustc 1.99.1 |
| macOS CEF `minimal` | 135.1 MB (archive) |
| macOS CEF `standard` | 344.0 MB (archive) |
| `cef::Settings` fields needed | `external_message_pump`, `windowless_rendering_enabled`, `cache_path`, `root_cache_path`, `remote_debugging_port`, `no_sandbox`, `framework_dir_path`, `user_agent`, `multi_threaded_message_loop` — all present |
| Current `RustBox_0.1.1_universal.dmg` | 31.4 MB |
| Current `RustBox_0.1.1_x64-setup.exe` | 14.0 MB |
| Current `RustBox_0.1.1_amd64.AppImage` | 92.2 MB |
| `tabs.rs` size | 2969 lines, 3 `WebviewWindowBuilder` sites, 17 `get_webview_window` uses |

**Honest conclusion:** a CEF build is 4x–11x the current macOS download before the
framework is even recompressed into a dmg. That is the cost of carrying a browser.

## 4. Process model

Two ways to put Chromium inside a RustBox window. The spike (M0) picks one; the
other is the fallback.

### Option A — windowed CEF, child native view (recommended)

CEF creates a browser in windowed mode and parents its native view (NSView on
macOS, HWND on Windows) into the window RustBox already allocates for a tab.

- Chromium renders with its own compositor into that child view.
- Input, IME, scrolling, cursor, drag-and-drop and accessibility come from the
  platform, the same way they do today.
- The existing `CONTENT_RECT` placement logic applies unchanged: both backends
  receive the same rectangle.

The hard part is the run loop. CEF owns a message loop, and Tauri owns one
(tao). Two supported integrations:

1. `external_message_pump = true`, then call `CefDoMessageLoopWork()` from a
   timer driven by Tauri's event loop. Keeps exactly one loop, so nothing is
   pinned to a second thread.
2. `multi_threaded_message_loop = true`, CEF runs its loop on its own thread.

**Correction, verified in CEF's own header** (`cef_settings_t`, in the
`cef-dll-sys` bindings): `multi_threaded_message_loop` is documented as *"only
supported on Windows and Linux"*. So on macOS option 2 does not exist, and the
fallback there is either off-screen rendering (Option B) or a separate helper
process. The earlier draft of this ADR listed option 2 as the general fallback;
that was wrong for macOS, which is the platform the whole milestone exists for.

On macOS, CEF additionally requires the host to be a real `.app` bundle with the
framework in `Contents/Frameworks/` and its helper executables in place. That is
a packaging requirement, not an optional nicety, and it affects `tauri dev` too.
Verified against the `cef` crate's own bundler (`src/build_util/mac.rs`), which
is what its `bundle-cef-app` tool and `tauri-cli` do:

- the framework is copied from the CEF distribution, **not** from
  `target/release` — on macOS the crate does not stage the runtime beside the
  binary the way it does on Windows and Linux;
- five helper bundles are required, named `<app> Helper`, `Helper (GPU)`,
  `Helper (Renderer)`, `Helper (Plugin)`, `Helper (Alerts)`, each with
  `LSUIElement = 1`;
- the helper resolves the framework through a different relative path
  (`../../..`) than the main binary (`../Frameworks`), which is a runtime
  detail, not just layout;
- a missing or misnamed helper makes a page load hang with no error, which is
  the worst kind of failure to debug from a user report.

### Option B — windowless OSR (off-screen rendering)

CEF renders into a shared texture, RustBox composites it. The crate has an
`accelerated_osr` feature pulling `wgpu` + `objc2-metal` + `objc2-io-surface`,
which is a real Metal path on macOS.

- Removes the child-window parenting question entirely; the surface is a layer we
  already control.
- Costs: keyboard/IME/scroll/hit-testing must be implemented and kept correct, and
  every frame is an extra GPU copy.

**Decision rule:** if M0 shows Option A cannot coexist with tao's loop, take
Option B. Do **not** fall back to a separate helper process: that means two
windows to keep positioned and focused, which regresses the window behaviour
users already have.

## 5. Code seam

`SubTab` holds `window: WebviewWindow` and `tabs.rs` calls webview methods on it
throughout. Method set, counted from the file (not estimated):

| Operation | Call sites |
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

Plan:

```rust
enum Surface {
    System(WebviewWindow),  // wry
    Chromium(CefTab),       // CEF
}
```

`SubTab.window` becomes `SubTab.surface: Surface`. Call sites that genuinely need
a real `WebviewWindow` — the Windows-only `with_webview` / `webview2_com` CDP
handlers in `watch_tab_navigation`, and `crate::cdp::call(&win, …)` — move behind
`Surface::with_platform(…)`, so the working Windows path is not rewritten.

The seam lands **first, alone, with zero behaviour change** (M1). That is what
makes it reviewable: no config can select CEF yet, and the whole suite must stay
green.

### Invariants that must survive the seam

1. **URL and title stay cached in `SubTab`.** The existing comment says why:
   `WebviewWindow::url()` posts to the main thread and waits, so calling it while
   holding the tab registry lock deadlocks. CEF's URL read is also a main-thread
   call, so the cache and the navigation watcher stay as they are.
2. **Hidden is not closed.** `show_only` hides other tabs and they keep running —
   the reason `webview_args::BASE` exists (background timers must not be
   throttled). A CEF tab must be hidden, never closed, and must not be throttled.

### What CEF needs per tab

- Own `CefBrowser` with the per-app request context.
- CEF reports navigation, title, console and network through its own handlers
  (`OnLoadStart`, `OnTitleChange`, `OnConsoleMessage`, `OnBeforePopup`,
  `OnBeforeDownload`). These replace the `webview2_com` handlers for CEF tabs;
  the Windows ones stay for WebView2 tabs.
- CDP: `remote_debugging_port` plus the existing `crate::cdp` client, so
  `tab_cdp`, `tab_eval`, `tab_console` and `tab_network` keep their signatures and
  `docs/mcp.md` does not change.

## 6. Reuse, not re-invention

- **Storage:** `paths::embedded_profile(&app_id)` becomes CEF's `cache_path` on a
  per-app `CefRequestContext`. Same folder semantics as `data_directory`, so
  `reset_app_profile` and `purge_app_data` keep working.
- **Proxy:** Chromium's `--proxy-server`; `Proxy::normalized_scheme` already emits
  the `http` / `socks5` forms Chromium accepts. Per request context, so it cannot
  leak between apps.
- **UI:** one more `<option>` in a new "Embedded backend" picker, shown only when
  the engine is `os-webview`. The browser picker stays disabled for both embedded
  backends, because neither launches an external program.

## 7. Milestones and gates

Every gate needs evidence. macOS gates need the user's Mac or a macOS CI runner;
I cannot verify macOS from this Windows machine and will not claim to.

| # | Work | Gate | Where | Status |
| --- | --- | --- | --- | --- |
| M0 | Spike outside RustBox: `cef` crate, one windowed browser, one page, message pump from a plain loop | A or B decided; which distribution (`minimal` vs `standard`) is enough; startup cost measured | Mac | Windows done (§11); macOS open |
| M1 | `Surface` seam, `System` only, no behaviour change | `cargo test --release` + `check-ui` green; no config can select CEF | Windows | Done, `021af33` |
| M2 | `Surface::Chromium`: navigate, size, show/hide, close, title, URL | A CEF tab renders a page inside the RustBox window | Mac | Blocked on M0 macOS |
| M3 | Per-app request context, proxy, user agent | Login survives a restart; proxy probe shows the request | Mac | Not started |
| M4 | Link chooser (`OnBeforePopup`), link targets, downloads, devtools, CDP | All four chooser answers work; `tab_cdp` answers | Mac | Not started |
| M5 | Windows port + the skew case no longer applies to CEF tabs | Same checks | Windows | Not started |
| M6 | Linux port | Same checks | Linux | Not started |
| M7 | Packaging: two dmgs, framework + helpers in bundle, signing, notarization, CI split, licence notices | A downloaded arm64 dmg opens on a Mac that never had RustBox | CI + Mac | Not started |

M0 and M1 are the cheap, de-risking steps. Neither adds a byte to a release
artefact.

## 8. Packaging consequences

**macOS**

- `Chromium Embedded Framework.framework` in `RustBox.app/Contents/Frameworks/`,
  `@rpath` links, plus CEF's required helper apps. CEF's layout, not a choice.
- Hardened runtime needs `com.apple.security.cs.allow-jit` and
  `com.apple.security.cs.allow-unsigned-executable-memory` for V8, and
  `com.apple.security.inherit` on helpers.
- **Signing and notarization stop being optional.** The macOS build is unsigned
  today and Gatekeeper merely warns; an unsigned CEF build does not launch at
  all. A Developer ID is a prerequisite for M7 — this is a hard blocker, not a
  polish item.
  - For local work (M0 through M6) it is not needed: an ad-hoc signature
    (`codesign -s -`, no Apple account) satisfies the checks macOS applies when a
    process loads a framework, and `tools/cef-spike/run-macos.sh` does it inside
    out. See `tools/cef-spike/SIGNING-WITHOUT-DEVELOPER-ID.md`.
  - What an ad-hoc signature cannot do is distribute: every new build is a new
    unsigned binary, so the receiver must override Gatekeeper each time
    (`xattr -dr com.apple.quarantine`, or right-click → Open). That recurring
    support cost, not a technical limit, is what M7 is for.
- Third-party licence notices (Chromium and its dependencies) must ship in the
  bundle.

**Windows** — CEF DLLs beside the exe; NSIS grows from 14 MB to the hundreds.
**Linux** — AppImage from 92 MB upward; deb dependency list grows.

## 9. Risks

| Risk | Severity | Mitigation |
| --- | --- | --- |
| CEF's loop fights tao's on macOS | High | M0 decides A vs B before code depends on it |
| We take over Chromium's security cadence | High | Standing commitment: pinned version, rebuild on Chromium security releases. This never goes away |
| Signing/notarization of framework + helpers | High | Needs a Developer ID; blocks M7 |
| Download size 4x–11x | Medium | Accepted; per-arch dmg; `minimal` if it suffices |
| Two webview stacks, two crash surfaces | Medium | CEF keeps its own helper processes; verify a CEF crash cannot take the shell down (M2) |
| Focus, IME, drag-drop regressions vs WebView2 | Medium | Explicit checks in M2/M4, not assumptions |
| `tabs.rs` is 2969 lines and the seam touches its core | Medium | M1 first, behaviour-preserving, suite green |
| macOS unverifiable from here | High | macOS gates run on the user's Mac or CI |

## 10. Open questions for M0

1. Option A or Option B on macOS (run-loop coexistence)?
2. Is the `minimal` distribution sufficient, or is `standard` needed for
   PDF/printing/DRM?
3. Startup-time and memory cost against WebView2 and WKWebView.
4. Does CEF ever become the Windows default, or stay opt-in? (Stays opt-in until
   M5 evidence exists.)
5. Developer ID availability, since M7 cannot ship without it.

## 11. M0 results (Windows, measured 2026-11-10)

The spike is `tools/cef-spike`. Run on Windows, release build:

| Question | Result |
| --- | --- |
| CEF compiles on this toolchain? | Yes, once `cmake` and `ninja` are on PATH (the crate's build script needs both). |
| Host-pumped loop (`do_message_loop_work`) works? | **Yes.** 120 heartbeat samples over the full 60 s, monotonic with no stall: 24 → 10538 pumps. The host loop was never blocked. |
| Browser created and loaded a page? | **Yes.** `create_browser returned 1`, `after_created` reported a main frame, and the page's own title arrived: `title=Example Domain`. |
| Distribution size | `target/release` is **371 MB** in 24 files; `libcef.dll` alone is **281.0 MB**. |
| GPU process | Logged `GPU process exited unexpectedly` and fell back to software compositing. Expected on this headless session; must be re-checked with a visible window before M2. |

Two consequences for the plan:

- The run-loop risk that made Option A uncertain is retired **on Windows**. It
  stays open on macOS, where CEF also requires the bundle layout, so M0 must be
  repeated there with `run-macos.sh` before M2.
- **371 MB is much more than the ADR's earlier estimate.** The `minimal`
  distribution still ships a 281 MB `libcef.dll`, so "hundreds of MB" is the
  realistic download, not a worst case. Windows currently ships a 14 MB
  installer.

### macOS M0 gate — not yet run

Nothing on this page claims macOS behaviour. The gate needs one command on the
user's Mac:

```sh
cd tools/cef-spike && ./run-macos.sh
```

It builds, assembles the bundle (framework + five helpers, per the crate's own
bundler), signs it ad-hoc, verifies the signature, and runs. **No Developer ID is
required for this**; see `tools/cef-spike/SIGNING-WITHOUT-DEVELOPER-ID.md`.

What decides the milestone, in order:

| Reading | Meaning | Next |
| --- | --- | --- |
| `pumps=` climbs and a window shows the page | Option A holds on macOS | Start M2 |
| `pumps=` climbs but no title ever arrives | Loop fine, helpers or framework wrong | Fix the bundle, re-run; not an M2 blocker yet |
| `pumps=` stalls, or the process stops pumping after the window appears | The host-pumped loop cannot coexist with tao on macOS | **Reopen the decision**: Option B (off-screen), since option 2 is Windows/Linux only |
| `== 5/6` fails `codesign --verify` | Bundle assembly is wrong | Fix before interpreting anything else |

Because `multi_threaded_message_loop` does not exist on macOS, a stalled loop
there has exactly one remaining fallback, and it changes the design. That is why
this gate is the blocker rather than a formality. The spike therefore **refuses**
`--multi-threaded` on macOS (exit 2) instead of attempting it: an attempt there
produces a process that never paints, which would be filed as "the host-pumped
loop failed too" and would corrupt the one measurement this milestone exists to
take.

### What was fixed before handing the script over

The first version of `run-macos.sh` would have failed on the Mac for three
reasons, all found by reading the crate's own bundler rather than by running it:

1. It copied the framework from `target/release`, where the crate never puts it
   on macOS.
2. It copied helper `.app` bundles that do not exist until something creates
   them; the crate's bundler creates all five by reusing one binary.
3. It assumed the helper and the main binary resolve the framework the same way.
   They do not, and a wrong resolver panics inside `canonicalize()`.

All three are fixed, and the framework search now falls back to a `find` under
`target/`, so an unexpected build layout produces a run instead of a dead end.

The script was verified by a dry run on Windows with stub `cargo`, `codesign`,
`ditto` and `xattr` that log their arguments: bundle layout (framework + five
helper apps + plists), signing order (framework dylibs → framework → helpers →
main binary → outer bundle), argument passthrough, and the fallback framework
search all behave as intended. **What a dry run cannot prove is that macOS
accepts the result.** `codesign --verify` at step 5 and the run itself are the
only real evidence, and both need the Mac.

New unit tests cover the three macOS-path decisions that are pure logic:
`is_helper_process` (the `--type=` switch that decides which framework path to
use), `find_framework` (both bundle depths, plus the no-framework case), and
`multi_threaded_supported` (the flag is refused where CEF lacks the feature).
These run on Windows; the load they feed does not.

## Consequences

- A RustBox tab can be Chromium on macOS for the first time, and on Windows can
  escape the WebView2 skew class.
- RustBox starts carrying a browser: its own Chromium security cadence, hundreds
  of MB per platform, and a signing requirement that does not exist today.
- ADR 0001's "Embedded engine: OS webview" becomes "OS webview by default,
  optionally bundled Chromium".
- Every future tab feature must be implemented for two backends or abstracted.
  That is the recurring cost, not the one-time work.
