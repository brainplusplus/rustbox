# ADR 0005 (proposed): Chromium as a selectable embedded engine (CEF)

**Status:** Proposed — nothing implemented.
**Date:** 2026-11-10.
**Relates to:** ADR 0001 (engine list), ADR 0002 (isolation + proxy: same rules,
different mechanism — see §6 and §6.1).

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
- **Proxy:** per app, through the request context's preference manager (the
  `proxy` preference on `CefRequestContext`); `Proxy::normalized_scheme` already
  emits the `http` / `socks5` forms Chromium accepts. **Not** `--proxy-server`:
  that switch is command line, and one CEF client process serves every app, so a
  switch set there applies to all of them at once and leaks one app's proxy into
  another's. `--user-agent` and `--lang` have the same problem, which is why the
  per-app UA must also travel as a context setting rather than a switch. Whether
  the preference manager honours per-context proxy on the CEF version we ship is
  an **M0 gate, not an assumption** — it is the one part of this plan that can
  turn out to be impossible without a process per app (see §6.1).
- **UI:** one more `<option>` in a new "Embedded backend" picker, shown only when
  the engine is `os-webview`. The browser picker stays disabled for both embedded
  backends, because neither launches an external program.

### 6.1 What `ProfileMode` means under CEF

`ProfileMode` was designed around WebView2, where `Isolated` means *a browser
environment of its own* — its own process tree, GPU process and crash domain. CEF
has no equivalent: one CEF client is one process tree, however many browsers run
inside it. What CEF can separate per app is a `CefRequestContext` — cookies,
localStorage, IndexedDB, HTTP cache — and nothing above that.

| Mode | Windows (WebView2) | macOS (WKWebView) | Linux (WebKitGTK) | CEF, all three |
| --- | --- | --- | --- | --- |
| `Light` — data apart, runtime shared | named profile | not available, degrades to `Isolated` | not available, degrades to `Isolated` | `CefRequestContext` + `cache_path` |
| `Isolated` — data *and* process apart | own environment | named data store, macOS 14+ | own `WebsiteDataManager` | **not available**: data apart, process shared |

Two consequences, both of which reach the UI:

1. CEF brings `Light` to the two platforms that do not have it today, and removes
   macOS's 14+ requirement, because `cache_path` works on every version CEF
   supports.
2. Under CEF the two modes collapse into the same mechanism, so a picker that
   still offered both would describe a difference that no longer exists. The
   honest form is a single "storage separated" mode for CEF, plus a line saying
   the browser process is shared with the other apps — exposed through
   `platform_info` the way `light_profiles` and `named_data_stores` already are
   (`src-tauri/src/main.rs:584`).

Real `Isolated` would come back only with a separate CEF client process per app,
which §4 rules out: the tab is a child view inside one shell window, and a
separate process means a separate top-level window to keep positioned and focused.

## 7. Milestones and gates

Every gate needs evidence. macOS gates need the user's Mac or a macOS CI runner;
I cannot verify macOS from this Windows machine and will not claim to.

| # | Work | Gate | Where | Status |
| --- | --- | --- | --- | --- |
| M0 | Spike outside RustBox: `cef` crate, one windowed browser, one page, message pump from a plain loop | A or B decided; startup cost measured; whether development needs the `standard` SDK (debug build, samples) | Mac | Windows done (§11); macOS open |
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
| Download size 4x–11x | Medium | Accepted; per-arch dmg. `minimal` does **not** rescue this: measured 281 MB `libcef.dll` (§11) |
| Per-app proxy impossible without a process per app | High | M0 gate: prove `CefRequestContext` preferences apply the proxy per context. If they do not, CEF cannot keep ADR 0002's per-tab proxy rule, and the backend should not ship |
| Two webview stacks, two crash surfaces | Medium | CEF keeps its own helper processes; verify a CEF crash cannot take the shell down (M2) |
| Focus, IME, drag-drop regressions vs WebView2 | Medium | Explicit checks in M2/M4, not assumptions |
| `tabs.rs` is 2969 lines and the seam touches its core | Medium | M1 first, behaviour-preserving, suite green |
| macOS unverifiable from here | High | macOS gates run on the user's Mac or CI |

## 10. Open questions for M0

1. Option A or Option B on macOS (run-loop coexistence)?
2. Is the `standard` SDK needed for development (debug `libcef`, `cefclient`,
   symbol files), given that both distributions ship the same release runtime and
   therefore the same installer contents (§12)?
3. Startup-time and memory cost against WebView2 and WKWebView.
4. Does CEF ever become the Windows default, or stay opt-in? (Stays opt-in until
   M5 evidence exists.)
5. Developer ID availability, since M7 cannot ship without it.
6. Does a per-context `proxy` preference actually apply per app inside one CEF
   process? This is the gate that decides whether ADR 0002 survives CEF (§6.1).

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

## 12. Alternatives considered

Recorded so this question is not re-opened from scratch. Sizes are what the
project would add to an installer, not SDK archive sizes; the rows for the
current engine are the numbers in today's releases.

| Option | Added size | OS | `Light` (data apart, runtime shared) | `Isolated` (data **and** process apart) |
| --- | --- | --- | --- | --- |
| WebView2 Evergreen (today) | 0 MB | Windows 10 1803+/11, x64 & ARM64 | yes, named profile (needs this repo's `tauri-runtime-wry` patch) | yes, own environment |
| WKWebView (today) | 0 MB | macOS, named store on 14+ | no, degrades to `Isolated` | yes on 14+; below that every app shares a store |
| WebKitGTK (today) | 0 MB | Linux, x64 & ARM64 | no, degrades to `Isolated` | yes, own `WebsiteDataManager` |
| WebView2 Fixed Version | ~150–180 MB | Windows only | yes | yes |
| CEF `minimal` | runtime `libcef.dll` alone measured 281 MB (§11) | Windows / macOS / Linux, x64 + ARM64 | yes, request context + `cache_path` | **no**, data only (§6.1) |
| CEF `standard` | same runtime; the extra is SDK-side | idem | idem | idem |
| Ultralight 2.0 (free) | ~10 MB | Windows / macOS / Linux desktop | single process by design | data only, no sandbox, and the free build has **no JavaScript JIT and no media playback** — real web apps do not run |
| Sciter (free) | ~4–8 MB (vendor) | Windows / macOS / Linux | single process | data only; own HTML/CSS engine that is explicitly not a browser, so third-party sites do not render |

Two conclusions that shaped the decision:

- **`minimal` vs `standard` is not a size decision for the product.** Both ship
  the same release runtime; the difference is debug binaries, symbol files and
  the sample/test apps in the SDK. The measured 281 MB `libcef.dll` is the same
  either way, so `standard` is the better working setup and `minimal` saves only
  developer disk.
- **The only cheaper options are the ones that cannot render the pages this app
  exists to render.** Ultralight and Sciter are smaller by two orders of
  magnitude and both fail the same test: WhatsApp Web, Gmail, AliExpress. That is
  not a licensing or effort trade-off, it is a capability gap, which is why they
  are not on the milestone path.

Servo, Blitz and Lightpanda were not in this table and are re-checked separately
in §14, together with the measured WebView2 comparison in §13.

## 13. Measured head-to-head against WebView2 (Windows, 2026-11-19)

§11 answered "does CEF render". It did not answer "what does a workspace of ten
apps cost", which is the question the engine choice actually turns on. Two
harnesses were used: `tools/cef-spike/src/bin/tabs.rs` (CEF, windowed browsers
over distinct origins so site isolation gives one renderer per tab) and a
WebView2 harness with the same tab count and origins. Both run with
`--disable-extensions` — this machine has four externally-registered extensions
that Chromium-based engines inject into pages, and leaving them on measures the
extensions rather than the engine. Process trees are walked by parent PID *and*
by the `--user-data-dir` / `--profile` tag, because `msedgewebview2.exe` is not a
child of the hosting process.

10 tabs, 45 s idle, RSS in MB:

| | WebView2 shared | CEF shared context | CEF per-tab contexts |
| --- | --- | --- | --- |
| RSS idle | 989.8 | 822.8 | 885.6 |
| RSS peak | 990.1 | 824.4 | 886.7 |
| Processes | 16 | 16 | 16 |
| Renderers | 10 | 11 | 11 |
| **RSS per renderer** | **61.5** | **55.7** | **55.9** |
| Browser process | 161.4 | 115.5 | 164.8 |
| GPU process | 80.1 | 39.8 | 40.3 |
| CPU (1 core) | 5.3 % | 5.9 % | 11.5 % |
| create, 10 tabs | 1092 ms | 667 ms | 951 ms |
| load | 165 ms | 796 ms | 1344 ms |

Three tabs, same method: WebView2 501 MB / 9 processes / 3.4 % CPU / 460–551 ms
to create; CEF 388 MB / 8 processes / 1.7 % / 232–253 ms.

**The 17 % difference is fixed cost, not per-tab cost.** RSS per renderer is
within 10 % of each other (61.5 vs 55.7 MB). The saving comes from the browser
process (−46 MB) and the GPU process (−40 MB) — one-off costs that do not grow
with the tab count. Tab eleven costs the same in both engines. The "CEF is
lighter once you have many tabs" hypothesis is therefore **not supported by
measurement**; the honest statement is "CEF starts lower and stays lower by a
constant".

> Later correction (§19.1): the GPU-process line in this table is not clean.
> CEF's GPU process crashes on this machine, so part of the −40 MB is a dead
> process rather than a leaner one. Re-measured on real applications with the
> rasterizer matched on both sides, the constant is **12 %**, and the conclusion
> — fixed cost, not per-tab — is unchanged.

### 13.1 Responsiveness under load (the reason §11 was incomplete)

§11 recorded that the host-pumped message loop never blocked: 120 heartbeat
samples, monotonic pump counts over 60 s. That measured the *pump*, not the
*windows*. A `SendMessageTimeout(WM_NULL, 5 s)` probe answers the question the
user actually experiences — is this window responding. 3 tabs, 40 s window,
three runs per configuration:

| Configuration | Run 1 | Run 2 | Run 3 |
| --- | --- | --- | --- |
| CEF, host-pumped (`do_message_loop_work`) | 1/672 | 1/660 | 1/676 |
| CEF, `multi_threaded_message_loop` | **0/800** | **0/792** | **0/788** |
| CEF, host-pumped + `--disable-gpu` | 1/672 | 1/668 | 1/676 |
| WebView2 | **0/197** | **0/199** | **0/200** |

The shape is deterministic rather than random: **every** host-pumped run is
unresponsive exactly once, at t ≈ 6.0–6.2 s, and never at any other time — 9 runs,
same point, single occurrence, 4 windows present. `--disable-gpu` does not remove
it, so the GPU process is not the cause. The only variable that removes it is who
owns the message loop.

So the pump can be healthy while a window is blocked, and §11's "the host loop was
never blocked" is not evidence that Option A is safe. What this does **not**
establish: the root cause at t ≈ 6 s, and whether a real Tauri event loop (which
pumps at a different cadence than the 5 ms probe loop) behaves the same. Both are
open, and macOS cannot use the configuration that measured clean anyway (§4).

## 14. Engine candidates re-checked (2026-11)

Re-run because the alternatives in §12 were written before Servo's embedder API
existed and before Blitz was a candidate at all. Everything here is from the
projects' own documentation and release notes — **none of it was measured on this
machine**, unlike §13.

| Candidate | What it actually is (2026-11) | Verdict for a tab that must render WhatsApp Web / Gmail |
| --- | --- | --- |
| **Servo** (`servo` crate 0.8.0, 2026-11-07; 0.1.0 on crates.io 2026-04-13, monthly releases, LTS offered) | Real engine, now genuinely embeddable. `multiprocess` and `bundled` on by default, media through GStreamer, storage in `servo-storage`, one storage profile per instance | **Not yet.** WebRTC code exists but is off by default (`servo#41396` "Enable WebRTC by default" is still open) and receiving remote streams does not work (`servo/media#210`, 2026-04: "the servo side does not yet implement playing streams"), so video calls fail; there is no EME/Widevine, so protected playback fails. Web-platform conformance is still short of Blink/WebKit and the embedder API breaks monthly. Storage isolation means one Servo instance per profile, not a per-tab partition |
| **Blitz** (DioxusLabs, beta) | HTML/CSS renderer built on Stylo + Taffy + Vello for *native app UI*. JavaScript is deliberately replaced by a Rust API; the README's own scope statement is "renders many popular **no-JS** websites (Wikipedia, old Reddit)" | **No — wrong category.** It is not a browser engine. Nothing that needs a JS framework runs on it |
| **Lightpanda** (1.0) | Zig, headless by design, **no layout, no rasterisation, no compositor** | **No — cannot display anything.** Genuinely useful as a headless agent target, which is a different feature |
| **Ultralight 2.0** | A single-process WebKit/WebCore fork (WebKit 615.1.18 ≈ Safari 16.4). Vendor feature matrix in §17: **no WebAssembly, no WebRTC, no MSE, no WebGL, no service workers, no notifications, no `<input type="file">` picker**. Media (VP9/Opus only) needs Indie or higher | No. It renders pages; it cannot run this class of app |
| **LibWeb** (Ladybird) | The only independent engine with real momentum. First alpha for macOS/Linux is still "coming later this year" as of 2026-11; no embedder API exists, and `libweb-gtk` is a proof of concept | No, and not for years |
| **CEF** | Chromium, embeddable everywhere, 281 MB `libcef.dll` | Yes for rendering; **no** for H.264/AAC and Widevine, which official builds do not ship (obtaining them needs a self-built CEF with `proprietary_codecs=true` plus patent-licence responsibility) |

The set of engines that can host a real third-party web app is closed: Blink
(CEF / WebView2 / Electron), WebKit (WKWebView / WebKitGTK / Ultralight's fork),
and Gecko (no desktop embedding API). Servo is the only plausible fourth, and it
is not there yet. That is a capability statement, not a preference.

UAParser.js's own list of detected engines makes the same point from the other
direction: it has 18 names, and of them only Blink and Gecko are living
general-purpose engines for third-party sites. The rest are dead (Amaya, EdgeHTML,
KHTML, Presto, Tasman, Trident), text-only (Links, Lynx, w3m), confined to a TV or
handset platform (ArkWeb, Flow, NetFront, iCab, Dillo, NetSurf, Goanna), or
aspirational (LibWeb, Servo). "There must be a small engine we could use" is a
reasonable intuition that this list answers: there is not, and the small ones are
small because they do not run the web.

### 14.1 The trilemma, stated plainly

The three goals pull against each other in a way no engine choice escapes:

- **Consistent across OS** requires shipping the same engine to all three, i.e.
  +281 MB and a browser security cadence of our own.
- **Lightweight** requires using the engine the OS already has, i.e. 0 MB and
  three different engines.
- **Runs real web apps** requires Blink or WebKit, which removes the other
  options from the table entirely.

Pick two. There is no fourth engine waiting to be found, and "combine Servo +
Ultralight + Lightpanda" is not a combination that renders anything: only the last
of the three renders at all, and it is the one that cannot.

What *is* separable, and this is the part worth designing around:

- **Session isolation is not the expensive part.** `ProfileMode::Light` already
  gives every app its own cookies, localStorage and IndexedDB inside one shared
  runtime. Isolation costs storage, not a process tree. The measured 61 MB per
  tab is the price of *rendering a modern web app*, and it is the same in every
  Chromium-based engine.
- **Consistency of behaviour** (shortcuts, tab model, notifications, proxy) is
  ours to define and is already engine-independent. Consistency of *rendering* is
  the only part that needs a bundled engine.
- **Stability** is where adding engines is actively harmful: the measured hang
  in §13.1 exists only in the configuration a Tauri host is forced into, and
  macOS cannot use the clean one.

### 14.2 Servo through Tauri exists — and rules itself out for this app

`tauri-runtime-verso` (versotile-org, MIT, 142 stars) is a Tauri v2 runtime that
replaces wry with Verso, i.e. Servo. It is close to a drop-in: disable tauri's
`wry` feature, call `tauri_runtime_verso::builder()` instead of `tauri::Builder`,
and ship `versoview` as an `externalBin` (prebuilt for x64 Linux/Windows/macOS and
arm64 macOS). This is the only path by which Servo could host a RustBox tab
without writing an embedder by hand, so it was worth checking rather than
dismissing.

Its own README rules it out for this product:

> "We currently hard coded the `Origin` header for the custom protocol IPC to
> work, but this means Tauri won't be able to check for if the URL is a remote URL
> or a local one for the capabilities, so right now, please don't use this to load
> arbitrary websites if you have related settings."

Loading arbitrary third-party websites *is* the product. RustBox's tab is a remote
origin by definition, and the capability system is what stops a remote page from
reaching Tauri commands; a runtime that cannot tell local from remote has to be
treated as a sandbox hole, not as a caveat. Add no devtools (Firefox remote
debugging instead of CDP, so `cdp.rs` and the MCP bridge would need a second
implementation), no per-window menus, pinned Tauri versions, and it is a research
branch rather than a backend.

That leaves the sequencing honest: Servo is the only engine that could one day
give consistency *without* Chromium's 281 MB, and the day it can is the day
`tauri-runtime-verso` can safely load a remote URL. Neither is true in 2026-11.

## 15. What "making Option C perfect" would actually take (2026-11)

Asked directly: can CEF be made to keep H.264/AAC/Widevine, never hang, and stay
lightweight? Three of the four are engineering with a known recipe. One is not
available at all, on one platform. Stated per requirement, with the cost attached.

| Requirement | Achievable? | What it actually requires |
| --- | --- | --- |
| **H.264 / AAC** | Yes | Build CEF from source: `is_official_build=true proprietary_codecs=true ffmpeg_branding=Chrome` (add `rtc_use_h264=true` if video calls are in scope). Well-trodden — Spotify, CefSharp users and several public build scripts do exactly this. The cost is a build farm: ~100 GB of checkout per configuration, hours per build, six binaries (Win/macOS/Linux × x64/arm64), rebuilt on every Chromium security release. Plus the patent question, which is a business decision, not a flag: the distributor of an H.264/AAC decoder is the one carrying the licence |
| **Widevine** | Yes, as a contract | `enable_widevine=true`, and `enable_cdm_host_verification` on Windows/macOS (`cef/BUILD.gn` asserts it there and only there). The CDM binary is not ours to redistribute without a Widevine licence, and persistent playback needs a VMP signature. castlabs is a Widevine 3PL partner and sells exactly this service for custom Chromium builds; their `electron-releases` fork is the working precedent. Two ceilings remain even when it is done right: desktop CDMs are L3 (software), so 4K is out — that needs L1 hardware plus PlayReady/HDCP 2.2, which is why Edge reaches 4K and Chrome stops at 1080p — and how a *custom* CEF app is tiered by each service is unmeasured. Also note `cef#3820`: Widevine has shipped broken in recent CEF versions even when built in |
| **Never hangs** | Windows/Linux yes, macOS **no** — and **not** for a Tauri host | `multi_threaded_message_loop` measured 0 hangs in 3/3 runs (§13.1). The CEF header is explicit about where that works: *"Set to true (1) to have the browser process message loop run in a separate thread. … This option is only supported on Windows and Linux."* macOS is therefore stuck on the pump model — the one that hung once per run, at t ≈ 6 s, 9 times out of 9. **Correction (see §18): the Windows/Linux half of this row does not apply to RustBox either.** A second thread pumping the Windows message queue competes with the WebView2 that hosts Tauri's own UI; that is exactly why Electrobun forces `external_message_pump` on Windows. Closing this needs either (a) finding and fixing the t ≈ 6 s block, or (b) a helper process that owns its own loop plus windowless rendering into a Tauri texture — which forfeits native input/IME/accessibility and is a multi-month project with no measurement behind it |
| **Still lightweight** | Partly, and the wrong axis | RAM: already 17 % below WebView2 (822.8 vs 989.8 MB at 10 tabs) and that is a fixed saving, not a per-tab one. Disk: 281 MB is the floor for an official build, but a self-built CEF with `symbol_level=0` has been reported at 90–160 MB (third-party forum figures, **not measured here**), at the cost of PGO. Per-tab RSS stays ~56 MB either way, because that is what a modern web app costs — the same number Electron pays |

So "perfect" has one hard wall and one soft one. The hard wall is macOS: the
configuration that measured clean does not exist there, and no flag changes that.
The soft wall is that a self-built, codec-complete, Widevine-signed CEF is a
*browser vendor* obligation — a permanent rebuild/re-sign/re-test pipeline across
six binaries. That obligation is precisely what Electron exists to absorb, and
what Rambox buys by being Electron. Rebuilding it in-house with fewer resources is
the honest description of Option C, and it is why the recommendation stays with
the OS webview plus a per-app CEF backend rather than a global switch.

## 16. Zebkit, and the Google sign-in constraint (2026-11)

`zebkit` (github.com/ikey4u, LGPL-2.1) is a new Chromium-as-a-library: Chromium
157 over the Content API, C ABI, ships Windows x64, macOS (arm64 + Intel) and
Linux x64. It is the closest thing to what Option C wants, so it was evaluated
against the four requirements rather than dismissed.

| Requirement | Zebkit |
| --- | --- |
| H.264 / AAC | **Included.** It builds its own FFmpeg (`src/ffmpeg/`, `scripts/ffmpeg-config.sh`) and the README lists "media with H.264 and AAC" in the shipped platform. This is the one thing it does that official CEF does not |
| Widevine | **Absent.** The README and the 0.6.0 release notes never mention it, and the author states it in the launch thread: "zebkit has no printing, web bluetooth, widevine and so on" |
| Lightweight | **Only against Electron.** Shipped 0.6.0 archives, unpacked: Windows x64 **255.7 MiB** (75.7 xz), Linux x64 187.0 (62.7), macOS 162.7–165.4 (48.7–50.3). Windows is therefore within 10 % of the official CEF runtime (281 MB) — against today's 14 MB installer this is a 5× increase. The project's own benchmark against Electron 45.2.1 puts ten popular sites at 4495 vs 4661 MB (**4 % less**), so bundled Chromium is bundled Chromium |
| Never hangs | **Unknown, and the shape is wrong.** The documented API for a host-owned loop is `zebkit_init()` + `zebkit_pump()` — the same shape as the configuration that hung once per run in §13.1. `zebkit_main()` owns the loop instead, which a Tauri host cannot give up. Zebkit is not CEF, so the root cause may not transfer, but nothing in its documentation claims otherwise and nothing here has measured it |
| Maturity | v0.6.0, released 2026-11, effectively one author (ikey4u), 117 points / 47 comments on the launch thread, which is openly sceptical that parts are machine-written. The engineering notes themselves are unusually rigorous — hashes per archive, controlled A/B comparisons, limits stated plainly — but a workspace app taking a months-old single-maintainer engine as its renderer is a dependency risk, not a technical one |

Net: Zebkit is a genuine improvement over CEF on codecs and size and still fails
Widevine and lightness. It does not change the decision.

### 16.1 The finding that matters more than the engine

Zebkit's own investigation notes are the best available evidence about a risk
that applies to **every** bundled-Chromium option, CEF included. Reproduced from
`docs/notes/google-signin.md`:

- A slim Chromium embedding was rejected by Google at email → Next with "This
  browser or app may not be secure" (`/v3/signin/rejected`, `rrk=46`), while a
  full Chromium build on the same machine reached the password page.
- The trigger was isolated to one missing legacy API: the renderer installed
  `chrome.loadTimes` and `chrome.csi` but not `chrome.app`. Restoring it made the
  same binary reach the password page, twice, on fresh profiles.
- Restoring FedCM alone did not help; disabling PDF viewing, `navigator.plugins`
  and DevTools did not cause it.
- The author is explicit that this is an observation, not a contract: "Google's
  private decision logic is not available; the tests do not identify a particular
  member or internal score." Full login, MFA and token persistence were never
  tested.

Two consequences for this ADR:

1. **Google sign-in is a hard gate for a bundled engine, and it is undocumented.**
   RustBox's product is a workspace of third-party web apps, and Gmail / Google
   Drive / YouTube sign-in is a first-class use case. A slim Chromium embedding
   has been *observed* to fail it, and the fix is a guess about private
   heuristics that Google can change without notice. This belongs next to the
   codec and signing costs as a reason Option C is not a free win.
2. **The OAuth popup problem is real and already documented by someone else.**
   Zebkit records that a host which replaced `View::on_new_window` with
   "report the URL, open a new tab" broke Google Identity Services: the
   `window.opener` relationship, the form POST body and the
   provider→site `postMessage` contract are not recoverable from a URL. Any
   windowless/OSR backend (Option B in §4) hits this on the first "Sign in with
   Google" button, which is a concrete cost that was previously only predicted.

**Now measured, on both engines.** The one-minute test above was run on
2026-11-19: load `accounts.google.com/signin/v2/identifier`, read the page, fill
the identifier field, click Next, read again. Both harnesses were extended for
it — `--urls`/`--probe` in the WebView2 harness, `--urls`/`--probe-file` plus a
`PROBE:` title channel in the CEF harness.

| | Step 0 | After Next |
| --- | --- | --- |
| WebView2 | `Sign in - Google Accounts`, `email:true`, `pw:true` | "Couldn't find this account" |
| Official CEF 156 | `Sign in - Google Accounts`, `email:true`, `pw:true` | "Couldn't find this account" |

Neither engine was walled. Google evaluated the (deliberately fake) address
instead of rejecting the browser, which is exactly the behaviour a real sign-in
would produce with a real address. Two consequences:

1. **The §16 risk is specific to slim Chromium embeddings, not to bundled
   Chromium as a class.** Official CEF — the only bundled option this ADR
   actually proposes — does not hit it. Zebkit's finding remains valid for
   Zebkit and for any build that trims the `chrome.*` surface.
2. **Today's default is not affected.** RustBox does not currently have a
   Google-sign-in bug caused by its engine. That rules out the one scenario that
   could have forced a CEF decision immediately.

Caveat: this exercises the identifier step only. Password submission, MFA and
token persistence are still unmeasured, and the probe used a fake address, so
"reaches the password page" is inferred from Google's own "Couldn't find this
account" response rather than observed directly.

## 17. Ultralight, and what its own docs settle (2026-11)

`ovsky/Ultralight-WebBrowser` was raised as evidence that Ultralight can be a real
browser. It is worth reading carefully, because it proves something narrower than
it appears to.

**What the repository is.** A C++/GLFW shell around the Ultralight SDK, GPL-3.0,
one author, created 2025-10, 22 stars, 2 forks, v0.9.x and labelled
"experimental / educational". Its README claims MIT while the repository licence
is GPL-3.0, claims "full web standards compliance", and marks its entire roadmap
complete — the recent commits are CI fixes merged from `cline/` branches. Those
are signals about review depth, not about the engine, so the engine was checked
against the vendor instead.

**What it does prove.** Ultralight genuinely renders ordinary web pages in a
23.3 MB portable build (Windows x64; 14.3 MB installer). Nothing else evaluated
here is within an order of magnitude of that. If the goal were "a small renderer
for our own HTML", this would be the answer.

**What it hides.** Its "DRM Content Support (Experimental)" is not Ultralight
doing DRM. The README states that DRM-protected content "cannot be rendered by
the Ultralight engine" and that the app switches to the **native platform webview**
(WebView2 / WKWebView / WebKit2GTK) when it detects DRM. The project's flagship
browser is therefore already the hybrid architecture this ADR recommends — OS
webview plus a second backend — with Ultralight in the role of the optional one.

### 17.1 The vendor's own feature matrix

From `ultralig.ht/docs/2.0/supported-web-features`, not paraphrased:

| Feature | Status |
| --- | --- |
| **WebAssembly** | **Not available** |
| **WebRTC and `getUserMedia()`** | **Not available** |
| **Media Source Extensions (MSE)** | **Not available** |
| **WebGL** | **Not available** |
| **Service workers** | **Not available** |
| **`crypto.subtle`** | **Not available** |
| **Notification** | **Not available** |
| **`alert` / `confirm` / `prompt`** | **Not available** |
| **`SharedWorker`** | **Not available** |
| **HTML drag and drop** | **Not available** |
| **Assistive technology / screen readers** | **Not available** |
| `<input type="file">` | Partial — **opens no picker** |
| `window.open` | Partial |
| HTML5 media | Partial — **Pro or higher**, WebM with VP9 video and Opus audio only |
| Web components, `<dialog>`, `contenteditable` | Supported |

That list is a map of what a workspace app needs. Concretely, for the apps this
product exists to host:

- **WhatsApp Web** — uses WebAssembly and Service Workers; calls need WebRTC. All
  three are "Not available".
- **YouTube** — playback requires MSE. "Not available" means no video at all, in
  every edition, because MSE is not an edition-gated feature.
- **Google Meet, Discord, any call** — WebRTC. Not available.
- **Google Maps, Figma** — WebGL, and Figma also WebAssembly.
- **Attaching a file in Gmail or WhatsApp** — `<input type="file">` opens no
  picker.
- **Desktop notifications**, which a multi-app workspace is largely *for* — not
  available.

H.264/AAC never appears in any edition: the media stack is WebM/VP9/Opus. So the
earlier §12 statement, "no JavaScript JIT and no media playback", was true of the
Free edition and imprecise overall: JIT and media are Indie-or-higher, and the
codecs are VP9/Opus regardless.

### 17.2 Licensing and platform coverage

- Free edition: $0, but limited to organisations under **$200K turnover and
  funding**, 60 FPS cap, **no JavaScript JIT**, no media playback, no custom user
  agent, no remote inspector.
- Indie $395/title, Pro $4,500/title, per title, with annual renewal for updates.
- Ultralight 2.0 ships for **Windows x64, macOS arm64, Linux x64** only — no
  macOS Intel and no Linux ARM64, both of which this project's §12 matrix assumes.

### 17.3 Verdict

Against the four requirements: session isolation **yes** (per-`View` session, so
cookies/localStorage/IndexedDB are separable — the data half of `ProfileMode`);
lightweight **yes, decisively**; consistency **partly**, with the platform gaps
above; stability **unmeasured**, and it is a single-process 2023 WebKit fork with
no sandbox; running real web apps **no**, by the vendor's own matrix.

It is worth being precise about what would change this. Measurement cannot: no
amount of testing adds WebAssembly, MSE or WebRTC. Only the vendor shipping those
would.

## 18. Electrobun: the architecture exists in production, and it is not copyable (2026-11)

`blackboardsh/electrobun` is the strongest external check on this ADR, so it was
read at source level rather than from the README. MIT, Zig core with a
TypeScript/Bun API, 13.1k stars, created 2024-02, pushed 2026-11-15, 178 open
issues.

### 18.1 It independently arrived at the shape recommended here

- **System webview by default.** Release artifacts separate
  `electrobun-core-*` from `electrobun-cef-*`. Core for Windows x64 is **11.6 MB**
  compressed — the same league as RustBox's current 14 MB installer.
- **Bundled Chromium as an opt-in flag**, in the README's own words: a
  "`bundleCEF` flag to bundle and pin Chromium for those that want that tradeoff
  of consistency over file size". The CEF tarball for Windows x64 is **177.2 MB**
  compressed. So enabling it multiplies the installer ~15×, which is the number
  §8 predicted from `libcef.dll` and this is the first real shipped figure for it.
- **Isolated webviews composited into the UI** via `<electrobun-webview>` and
  `partition_context.h` — the same concept as RustBox's `ProfileMode`, one
  Chromium context per app.
- **CEF is official upstream**: `CEF.md` downloads the `minimal` distribution from
  `cef-builds.spotifycdn.com` and pins 147.0.10. That means `bundleCEF` inherits
  the same gap as §15: no H.264/AAC and no Widevine. The repository has no issue,
  document or flag mentioning either.

That an independent project with 13k stars converged on "OS webview by default,
bundled Chromium behind a flag" is the best available evidence that this is the
right shape — better than any argument in this document.

### 18.2 Why its solution does not transfer to a Tauri host

Read from `package/src/native/{win,macos,linux}/nativeWrapper.*`:

| Platform | CEF configuration | Why |
| --- | --- | --- |
| Windows | `multi_threaded_message_loop=false`, `external_message_pump=true`, pumped from `OnScheduleMessagePumpWork` | The in-source comment: "With external_message_pump=true, CEF does NOT internally pump Windows messages, **preventing it from stealing WebView2 messages**" |
| macOS | `multi_threaded_message_loop=false`, `windowless_rendering_enabled=true`, `CefRunMessageLoop` owns the main thread | Single-threaded loop is the only supported option |
| Linux | `CefRunMessageLoop` on a dedicated thread | In-source note: driving `CefDoMessageLoopWork` from a `gtk_main` timer "leaves it perpetually ready and spins an idle CPU core" |
| All | `no_sandbox = true`, `windowless_rendering_enabled = true` (OSR) | Simplicity; OSR is required for compositing into their UI |

Two consequences that change earlier conclusions in this ADR:

1. **§15 overstated the Windows escape hatch.** It said "Windows/Linux: use
   `multi_threaded_message_loop`". Electrobun's comment explains why that is not
   available to any host that keeps a WebView2 UI: a second thread pumping the
   Windows message queue competes with WebView2's own pump. RustBox's shell is a
   WebView2 UI, so the conflict applies. The hang-free configuration is therefore
   not merely unavailable on macOS — it is likely unavailable on Windows too. Only
   Linux has a clean answer, and it costs a dedicated thread running
   `CefRunMessageLoop`.
2. **Electrobun could only make those choices because it owns `main()`.** For
   macOS it hands the main-thread loop to CEF entirely. Tauri's `tao` event loop
   cannot do that. This is the structural reason "just do what Electrobun does"
   is not an option: adopting their loop strategy means adopting their
   framework.

Also worth recording as a hypothesis, not a finding: their OSR configuration may
be why this hang has not been reported there. §13.1 probed a *windowed* CEF
browser with `SendMessageTimeout` on its `HWND`; with `windowless_rendering_enabled`
there is no per-tab HWND, so a blocked Chromium UI thread would present as a
render freeze rather than an unresponsive window. Nobody has measured that.

### 18.3 Adoption verdict

As a framework: not adoptable. Electrobun is Zig + TypeScript with a custom
toolchain ("Hutch", "Cottontail"), and adopting it means rewriting the shell, not
swapping an engine. RustBox is ~20 Rust modules including `tabs.rs` (143 KB),
`cdp.rs`, `mcp.rs`, `session.rs`, `proxy_auth.rs`; none of that transfers.

As a reference implementation: extremely valuable, and the only production
codebase found that solves the concrete problems a CEF backend would face —
WebView2 coexistence, per-context partitions, OSR compositing, per-platform
process helpers, and the three-way loop strategy above.

## 19. Three real applications, and the rasterizer confound (Windows, 2026-11-19)

§13 measured eight blank local pages. Blank pages are the best case for any
engine: no scripts churning, no network, nothing to composite. RustBox runs
WhatsApp Web, Gmail and YouTube, so the ranking had to be re-checked against
pages that never go idle.

Method: three tabs, 45 s idle sampling, same three URLs in the same order on both
sides, three runs per configuration, `--disable-extensions` everywhere. URLs are
logged-out (`web.whatsapp.com`, `mail.google.com/mail/u/0/#inbox`,
`www.youtube.com`), so no account data or credentials are involved. Logged-out
is also the harder case for memory: each page runs its full unauthenticated
front-end instead of a synced shell.

### 19.1 The confound that invalidates a naive comparison

This machine has an RTX 3090 and an Intel UHD 770, but CEF's GPU process
**dies at startup on every run**: `GPU process exited unexpectedly:
exit_code=-2147483645` (STATUS_BREAKPOINT), followed by
`ContextResult::kTransientFailure`. CEF therefore rasterizes in software.
WebView2 logs no such error and keeps a 137–139 MB GPU process doing real work.

That asymmetry is worth roughly 90 MB and, more importantly, all of the CPU
difference: a hardware-accelerated WebView2 against a software CEF is a
measurement of the rasterizer, not of the engine. Every number below is given in
both configurations — as-shipped, and with `--disable-gpu` forced on **both**
sides so the comparison is like-for-like.

### 19.2 Results

RSS in MB, 3 tabs, median of three runs:

| | WebView2 as-shipped | CEF as-shipped | WebView2 `--disable-gpu` | CEF `--disable-gpu` |
| --- | --- | --- | --- | --- |
| RSS idle (median) | 1132.5 | 918.9 | 1045.5 | 918.8 |
| Run spread | 966.4 – 1139.7 | 877.1 – 932.7 | 847.5 – 1054.6 | 909.4 – 948.5 |
| Processes | 11–12 | 12–13 | 10–12 | 12–13 |
| Renderer RSS (sum) | 500 – 663 | 607 – 657 | 471 – 664 | 621 – 661 |
| GPU process | 137.1 – 139.6 | 27.2 – 27.6 (dead) | 52.4 – 59.5 | 47.2 – 47.6 |
| CPU, % of one core | 4.5 – 8.1 | 10.4 – 13.4 | 2.0 – 5.2 | 7.9 – 12.0 |
| create 3 tabs | 473 – 510 ms | 244 – 264 ms | 508 – 525 ms | 233 – 246 ms |
| load | 1555 – 5351 ms | 1406 – 1645 ms | 1742 – 1817 ms | 1388 – 1836 ms |

**At matched rasterizer the RSS advantage shrinks to 12 %** (918.8 vs 1045.5 MB)
and the CPU advantage disappears entirely. The 17–19 % seen when comparing
default configurations is real as an end-user observation on *this* machine, but
it is not "CEF is leaner": part of it is a dead GPU process on the CEF side.
The honest claim, consistent with §13, remains: **CEF starts ~12 % lower and
stays lower by a constant, not by a per-tab amount.** Renderer RSS is the same
within noise (621–661 vs 471–664 MB for three tabs).

### 19.3 The CPU number is mostly the harness, not the engine

§13.1 already showed the host-pumped loop is the wrong mode. On real apps the
same split appears in CPU, so one run was repeated with
`--multi-threaded` (CEF owns the loop):

| CEF `--disable-gpu` | RSS | CPU, % one core |
| --- | --- | --- |
| host-pumped (`do_message_loop_work`, 5 ms tick) | 918.8 MB | 11.8 % |
| `multi_threaded_message_loop` | 947.1 MB | **7.4 %** |

Roughly 4 points of the gap was the 200 wakeups/second harness pump. What
remains — 7.4 % against WebView2's 5.1 % — is 0.23 % versus 0.16 % of a
32-thread machine. In wall-clock terms this is not a user-visible difference,
and it is the *wrong* mode being compared: `multi_threaded` is also the mode that
removed the t≈6 s freeze (§13.1). The CPU objection to CEF does not survive
being measured in the mode CEF would actually ship in.

### 19.4 Isolation, at equal meaning

§13 compared WebView2 `shared` with CEF `contexts`, which is one browser process
with separate storage — not the same thing as WebView2 `isolated`, which forks a
whole browser tree per app. Three tabs:

| | RSS idle | Processes | CPU % one core |
| --- | --- | --- | --- |
| WebView2 `isolated` (3 browser trees) | 1557.8 – 1764.1 | 23–24 | 3.5 – 9.8 |
| CEF `contexts` (1 tree, storage split) | 1012.8 | 14 | 12.4 |
| CEF 3 independent clients (3 trees) | 1432.3 | 28 | 14.7 |

Relative to its own shared baseline, per-app isolation costs WebView2
+47–59 % RSS and CEF +56 % — the same order. CEF `contexts` looks 40 % cheaper
only because it is not isolating processes; it must not be quoted against
WebView2 `isolated`.

### 19.5 What this settles

- The §13 result replicates on real applications. CEF's saving is real, fixed and
  modest (~12 % at equal rasterizer), not the 17–40 % that a careless reading of
  the raw table suggests.
- The apparent CPU penalty is mostly an artifact of the harness's pump loop plus
  a dead GPU process. In the shipping loop mode the remaining gap is 0.23 % vs
  0.16 % of a 32-thread machine — 0.07 percentage points.
- CEF creates tabs roughly twice as fast (250 vs 483 ms for three) and loads
  no slower; WebView2's load times are far more variable (1555–5351 ms vs
  1388–1836 ms), which matches the "page is up but not painted" complaints that
  motivated this ADR.
- None of this changes the decision. At 12 % fixed memory saving with a
  ~500 MB per-platform runtime and two backends to maintain, opt-in CEF remains
  correct and remains not-yet-worth-shipping (§2, §18.3).

### 19.6 What remains unmeasured

Windows only, one machine, logged-out pages. YouTube was not playing video;
WhatsApp Web was sitting on the QR screen. A playing video and a synced inbox
are different workloads, and the GPU-process crash means the hardware path has
never been measured for CEF on this host at all — the 12 % figure is a
software-rasterizer comparison, which is the *pessimistic* case for both sides
but not the common one. macOS and Linux remain entirely unmeasured.

## 20. Electron, and why "it is more stable" is true but does not transfer (2026-11-20)

The obvious question after §13.1: if CEF's host-pumped loop is the configuration
that hangs, and Electron is observed to be stable, why not use Electron? It was
measured rather than argued about.

### 20.1 What Electron actually is, in this comparison

Electron 45.4.1 ships **Chromium 156.0.8078.100**; the CEF built here is
**156.0.8078.88**. These are the same engine, to within three patch numbers. Any
difference measured below is therefore shell and integration, not rendering.

That is exactly why the comparison is worth making — and exactly why it is a
trap. Electron is not a library that a host embeds. It *is* the application:

| | Who owns the main thread and its message loop |
| --- | --- |
| Electron | **Electron.** It calls `BrowserMainLoop` itself and pumps libuv from inside it, so Node and Chromium share one loop with one owner |
| WebView2 | **Tauri.** The webview is created on Tauri's thread; `msedgewebview2.exe` runs its own browser process |
| CEF in a Tauri host | **Split.** Either CEF pumps (impossible — Tauri owns the thread) or Tauri pumps for CEF (`external_message_pump`), which is the mode that hangs 9/9 runs in §13.1 |

Electron appears stable because it never has the split. There is no second owner
to fail to pump. `multi_threaded_message_loop` on the CEF side reproduces that
same single-owner property, and §13.1 measured **0/800, 0/792, 0/788** hung
probes in that mode — the same cleanliness Electron shows.

So the observation is correct and the mechanism is understood, but it does not
transfer: **Electron cannot be adopted as an engine inside a Tauri app.** There
is no embedding API. `WebContentsView` composes views inside Electron's own
`BaseWindow`; it does not accept a foreign HWND. The only way to get Electron's
stability is to become an Electron app — i.e. throw away `src-tauri/`: `tabs.rs`
(143 KB), `cdp.rs`, `mcp.rs`, `session.rs`, `proxy_auth.rs`, and the Rust shell.
That is the same conclusion §18.3 reached for Electrobun, by a different route.

There is a second-order reason the comparison flatters Electron: it has no
WebView2 coexisting next to it. Electrobun's own Windows source comment (§18)
records that it must run CEF with `external_message_pump=true` specifically to
avoid "stealing WebView2 messages" — the host-pumped mode — and Electrobun is
thus exposed to the same class of problem this ADR measured. Running both
engines in one process is where the hang lives.

### 20.2 Measured footprint, 3 real apps, 45 s idle

Same three URLs, same sampler, same `--disable-extensions` as §19.

| | WebView2 | CEF 156 | **Electron 45.4.1** |
| --- | --- | --- | --- |
| RSS idle, median of 2–3 | 1132.5 MB | 918.9 MB | **1285 MB** (1102 / 1469) |
| RSS, software rasterizer | 1045.5 MB | 918.8 MB | **1285 MB** (1320 / 1250.6) |
| Processes | 11–12 | 12–13 | **7–9** |
| Renderer RSS (sum) | 500–663 MB | 607–657 MB | **605–1060 MB** |
| GPU process | 137–140 MB | 27 MB (dead) | **188–247 MB** |
| CPU, % one core | 4.5–8.1 | 10.4–13.4 (host-pumped) | 3.3–14.2 |
| create 3 tabs | 473–510 ms | 244–264 ms | **224–263 ms** |
| load | 1555–5351 ms | 1406–1645 ms | 1272–10054 ms |
| **Installed runtime, unpacked** | 0 (OS component) | **281 MB** `libcef.dll` | **356.9 MB** (151 MB zip) |

Electron is **not** lighter than either alternative. It is the heaviest of the
three on RSS and on disk, and it carries a Node.js runtime that a browser tab
does not need. Fewer processes, more memory per process — the GPU process alone
is ~190–250 MB because it is hardware-accelerating on this machine's RTX 3090,
which is the one thing CEF could not do here (§19.1).

### 20.3 The codec result, measured on all three

This is the finding that matters most for a Rambox-style workspace, and it was
measured in each engine's own JS context rather than read from documentation:

| | H.264 | AAC | HEVC | Widevine |
| --- | --- | --- | --- | --- |
| WebView2 (Chrome 157 / Edg 157) | `probably`, MSE yes | `probably`, MSE yes | `probably` | not measured here |
| **Official CEF 156** | **`""` — cannot play** | **`""` — cannot play** | `""` | **no API** |
| Electron 45.4.1 | `probably`, MSE yes | `probably`, MSE yes | `probably` | unavailable (needs castlabs fork) |

Official CEF returns the empty string from `canPlayType` for both H.264 and AAC
and `false` from `MediaSource.isTypeSupported`. That is not a nuance: **YouTube
video, WhatsApp voice notes, Instagram/X clips and most MP4 content simply do
not play** in the CEF configuration this ADR has been measuring. §15 said this
from `proprietary_codecs=false` in the build flags; it is now confirmed in the
engine's own answer.

Electron ships `USE_PROPRIETARY_CODECS` and gets H.264/AAC for free. WebView2
already has them and has them today — which is the strongest single argument for
leaving the default engine alone. The gap is not "CEF is worse", it is "the
default already wins on the axis users notice fastest".

Widevine is absent in CEF and in Electron as shipped; Electron needs the castlabs
fork, CEF needs a self-built + VMP-signed binary (§15). WebView2 was not probed
for Widevine in this session, so nothing is claimed about it beyond the codecs
that were measured.

### 20.4 What this changes in the decision

Nothing about the default. It removes Electron from the candidate list on
grounds that are architectural rather than empirical, and it adds one hard
requirement to any future CEF work:

- **A bundled CEF must be self-built with `proprietary_codecs=true`**, or it is
  demonstrably worse than the engine it replaces for the app categories RustBox
  exists to host. The 281 MB official runtime is not merely "missing DRM" — it
  cannot decode the most common video format on the web.
- **Electron is not a candidate**, not because it is bad but because adopting it
  is a rewrite, and §20.2 shows it is heavier anyway.
- **The stability question has an answer already**: the hang is a dual-loop
  problem, and the fix is to not split the loop. For a Tauri host with WebView2
  UI that is genuinely hard (§18.2, §19.3), which is the strongest remaining
  argument against CEF on Windows — stronger than memory, stronger than disk.

### 20.5 Stability, measured on all three

The §13.1 `SendMessageTimeout` probe was extended to Electron (`probe-hang.ps1
-Engine electron`, same 40 s window, 3 tabs, 3 runs). Result:

| Configuration | Run 1 | Run 2 | Run 3 | Worst response |
| --- | --- | --- | --- | --- |
| CEF, host-pumped (`external_message_pump`) | 1/672 | 1/660 | 1/676 | >5000 ms |
| CEF, `multi_threaded_message_loop` | 0/800 | 0/792 | 0/788 | — |
| **WebView2 (today's default)** | **0/197** | **0/199** | **0/200** | <10 ms |
| **Electron 45.4.1** | **0/465** | **0/462** | **0/459** | **8 ms / 2 ms / 3 ms** |

Electron is clean, and so is **WebView2**. Both are single-owner message loops;
only the host-pumped CEF configuration hangs, and it hangs deterministically at
t≈6 s. Re-confirmed the same day: CEF host-pumped **1/476** hung probes, Electron
**0/465, 0/462, 0/459**.

This is the most important line in this ADR for the current decision: **the
stability problem the user reports is a property of the CEF configuration, not a
property WebView2 lacks.** Migrating to Electron to obtain stability would be
paying a shell rewrite, +150 MB RSS and +357 MB of runtime for something the
current default already delivers. If stability is the driver, the cheap action
is to *not ship CEF* — which is what §2 already decided.

Electron earns its stability the same way `multi_threaded_message_loop` does: it
owns the loop. Neither is evidence that Chromium-in-a-Tauri-host can be made
equally clean, and neither is reachable from inside `src-tauri/`.

### 20.6 The configuration the user actually asked for, and it works

The request was: stay in Rust, but get Electron's stability, and keep both
WebView2 and an opt-in embedded Chromium. Electron's stability comes from owning
the message loop (§20.1). The same property is available to a Rust host through
CEF's `multi_threaded_message_loop`, where CEF runs its own UI thread instead of
being pumped by the host.

That was tested rather than assumed. `tabs.rs` gained `--host-loop`, which
creates a real Win32 window on the main thread and runs a standard
`GetMessage`/`DispatchMessage` loop in it — the exact shape Tauri has — while CEF
services its browsers on its own thread. Three real applications, 40 s window,
`SendMessageTimeout` at 250 ms:

| Configuration | Probes | Hung | Worst response |
| --- | --- | --- | --- |
| CEF host-pumped + real apps (§20.5) | 476 | **1** | >5000 ms |
| CEF `multi_threaded`, real apps | 619 | 0 | 5 ms |
| **CEF `multi_threaded` + host Win32 loop, real apps** | **765** | **0** | **3 ms** |
| Electron 45.4.1 | 459–465 | 0 | 2–8 ms |

**The coexistence works.** A Rust host can own a message loop, keep WebView2 for
the UI, and run CEF windows on their own loop, and the result is
indistinguishable from Electron on the responsiveness probe. The architectural
objection in §18.2 ("a Tauri host cannot give up the main thread") turns out to
be surmountable: the host does not have to give it up, because CEF does not need
it.

This removes the last strong argument against CEF on Windows and changes the
recommendation's shape, not its content:

- The opt-in design of §2 is now **implementable as measured**, not just as
  designed.
- The `--multi-threaded` requirement is not optional. Any CEF backend that pumps
  from the host thread reproduces the t≈6 s freeze, and that includes the OSR
  shape in §4 Option B if the host drives it.
- It does **not** make official CEF shippable: H.264/AAC are still absent
  (§20.3), which is disqualifying for a workspace hosting YouTube and WhatsApp
  voice notes.

### 20.7 Codecs: the one blocker left, and it is not free

§20.3 measured official CEF returning `""` for H.264 and AAC. The question that
decides whether an opt-in CEF is worth building is whether that can be fixed
without a build farm.

The recipe is public and small. `amenophis/cef-codecs` builds CEF from source
with exactly these GN arguments and nothing else changed:

```
is_official_build=true proprietary_codecs=true ffmpeg_branding=Chrome
```

That is the whole difference between a CEF that plays YouTube and one that does
not. What it costs is not the flags:

| Route | Reality |
| --- | --- |
| `amenophis/cef-codecs` | Correct recipe, correct patches, but **macOS arm64 only**. Nothing for Windows or Linux |
| `psmux/godwinmix-cef` | Six platforms, official layout, `index.json` + sha1/sha256. **Measured working on Windows (§24)**: real H.264 decodes, AAC supported. Single maintainer, no CI, no attestation |
| Build it yourself | `automate-git.py` at the CEF commit + Chromium build number; ~100 GB checkout, hours per configuration, one per platform/arch, rebuilt on every Chromium security release |
| Electron | Has them for free — and is not embeddable (§20.1) |

So the honest state of the option is:

- **On Windows, opt-in CEF no longer has to cost video.** §24 measured a
  codec-complete build that decodes H.264 at the exact CEF version the Tauri 3
  CEF runtime pins. What remains is supply-chain trust, the Windows sandbox
  (§21.3), a GPU process that dies on this host, and the absence of Widevine.
  The default WebView2 still has codecs *and* measured stability (§20.5), so
  the opt-in keeps its narrow justification: escaping the WebView2 skew class of
  ADR 0006, for users whose app breaks on the installed runtime.
- **On macOS and Linux the stable configuration does not exist.** CEF requires
  the main thread there; `multi_threaded_message_loop` is Windows-only. A Tauri
  host cannot give up the main thread, so those platforms fall back to the
  host-pumped shape that hangs, or to OSR.

That asymmetry is the real answer to "just make it like Electron everywhere": the
part that makes it work is Windows-specific.

### 20.8 Unmeasured

One machine, Windows only, logged-out pages, and the Electron hang parity probe
was prepared (`probe-hang.ps1 -Engine electron`) but not yet run under the same
40 s window as §13.1. The claim "Electron is stable" here rests on the user's
observation plus the architectural argument in §20.1, not on this session's
`SendMessageTimeout` data.

## 21. `tauri-runtime-cef`: the official runtime, and why the hand-built seam is cancelled (2026-11-20)

Everything in §2, §5 and §20.6 assumed RustBox would build the CEF seam itself:
`EmbeddedBackend::{System, Chromium}` inside `os-webview`, a `Surface::Chromium`
variant beside `Surface::System(WebviewWindow)`, and a hand-rolled pump. That
work is unnecessary. Tauri 3.0 ships an official CEF runtime.

### 21.1 What it is, verified from the crate rather than the announcement

`tauri-runtime-cef` on crates.io:

| | |
| --- | --- |
| Publisher | `tauri-bot`, from `github.com/tauri-apps/tauri` — **first-party, not a community fork** |
| Licence | Apache-2.0 OR MIT (same as Tauri) |
| Size | 15,455 lines of Rust across 67 files; 239 KB crate |
| Version | `3.0.0-alpha.7`, published 2026-11-06, 8,777 downloads |
| Pins | `cef =154.5.0`, `tauri =3.0.0-alpha.6`, Rust 1.95, edition 2024 |
| Platforms | Windows, macOS, Linux; mobile keeps wry |

The documentation states it plainly: *"The CEF runtime is new in the Tauri 3.0
alpha and is not available in Tauri 2.0. Its APIs may still change before the
stable release."*

It is a **webview runtime**, peer to `tauri-runtime-wry`. That means it replaces
the whole `Surface` seam rather than sitting inside it: windows, webviews, menus,
tray, events, commands and plugins all route through the runtime trait. RustBox's
`Surface::System(WebviewWindow)` already exists (commit `021af33`) and is exactly
the abstraction this swaps out.

### 21.2 The message loop: better than the harness, and still unmeasured

The runtime uses `external_message_pump` — the host-pumped family that hangs in
§13.1 — **not** `multi_threaded_message_loop`. But its pump is not the harness's
pump:

| | Harness (`tabs.rs`, §13.1) | `tauri-runtime-cef` |
| --- | --- | --- |
| Trigger | `do_message_loop_work()` every 5 ms, unconditionally | `OnScheduleMessagePumpWork(delay)` posts `WM_HAVE_WORK` to a message-only window; a `WM_TIMER` covers the delay |
| Re-entrancy | none | detected and reposted, per cefclient |
| Source | written for the spike | a port of upstream cefclient's `main_message_loop_external_pump_win.cc` |
| Nested loops | not handled | the timer rides winit's own loop, so CEF keeps painting during modal move/resize |

The 1/476 hang in §20.5 was measured on the naive pump. The reference
implementation may well not exhibit it, and the crate's own comment explains why
it should not: the pump is driven by CEF's work notifications rather than a
blind 200 Hz poll. **This is unmeasured and must not be assumed either way** —
it is the first thing to test if RustBox ever takes this path.

### 21.3 The three blockers, in order of severity

**1. It is a whole-app choice, not a per-app choice.** The runtime is selected by
the Cargo manifest and by the `runtime(..)` call on the builder. From the docs:
*"A binary can only run one runtime."* When both crates are linked the CLI
assumes CEF, because it is the one that must be shipped. There is no API to give
one webview WebView2 and another CEF. The "user picks WebView2 or CEF" shape
therefore becomes **two builds of the app**, not one build with a switch:

- Default installer: wry, ~14 MB, WebView2, codecs, sandboxed, today's stability.
- Optional CEF build/download: pinned Chromium, ~300 MB+, consistent across OS.

That is a legitimate product shape — arguably a better one, since it keeps the
default download small — but it is not the per-tab seam ADR 0005 §2 designed.
Worth noting the runtime swap is otherwise cheap: Tauri 3 defaults to
`DynRuntime`, so app code (commands, `AppHandle`, `WebviewWindow`) does not change
at all, and swapping runtimes is "a change to `Cargo.toml` and to the `runtime`
call". RustBox's `Surface` seam becomes unnecessary rather than impossible.

**2. Windows runs unsandboxed, and the crate says so.** From `src/sandbox.rs`:
*"Windows currently runs unsandboxed, whatever the policy says."* Since Chromium
M138 the sandbox broker can only be linked by a binary built with Chromium's own
toolchain; CEF ships `bootstrap.exe` hosts for that, and a Tauri app is an
executable, not a bootstrap-hosted DLL. `SandboxPolicy::Required` **fails to
start** on Windows rather than quietly pretending.

For a workspace that loads arbitrary third-party web apps, this is a genuine
security regression against WebView2, which is sandboxed today. It is a packaging
problem rather than a code problem, but it is not solved.

**3. Codecs: absent from the official distribution, solved by a third-party
one.** The runtime pins `cef = "=154.5.0"` → CEF `154.0.34`, and the official
distribution of that version has no `proprietary_codecs`, so §20.3's measurement
carries over unchanged: no H.264, no AAC. A prebuilt at the *same* version with
codecs exists and was measured working — see §24, which supersedes this point.
The crate's own guide never mentions media codecs at all.

Plus a version constraint: RustBox is on Tauri **2.12.4** stable. Taking this
means upgrading to a **3.0 alpha**, against the docs' own advice to keep
production on 2.x — and re-doing the vendored `tauri-runtime-wry` patch
(`vendor/tauri-runtime-wry`, added for WebView2 multi-profile) against a new
runtime API.

One more structural fact worth recording, because it closes the door on any
hybrid: **on Linux, wry is GTK 3 and CEF is GTK 4, and the two cannot be
initialized in the same process.** The `tauri` crate picks its GTK bindings at
compile time via the runtime crate. So even a hand-written per-tab seam could not
mix them on Linux; "two builds" is not merely the documented shape, it is the
only possible one there.

### 21.4 What it would give RustBox, if the blockers clear

Worth stating, because it is more than an engine swap:

- **CDP as a first-class API** — `send_dev_tools_message` and
  `on_dev_tools_protocol`. RustBox's `cdp.rs` (32 KB) exists to get this through
  WebView2 by hand.
- **Per-webview permissions** — `on_permission_request` with a real
  `PermissionKind`, including the FedCM / storage-access cases that third-party
  sign-in flows need (§16.1).
- **Popups with a correct opener** — `features.opener().as_cef_window_opener()`,
  which is precisely the `window.opener` / POST-body problem §16.1 recorded as
  unrecoverable in a URL-reporting design.
- **Frame lifecycle events**, console messages, and `with_browser_settings` for
  the fields Tauri exposes no attribute for.
- **An opinionated secure default**: release builds ignore Chromium switches on
  their own command line, so a user cannot relaunch the shipped binary with
  `--disable-web-security` or `--remote-debugging-port`.

### 21.5 Decision

**Do not hand-build the CEF seam.** ADR 0005 §2's `EmbeddedBackend`, §5's
`Surface::Chromium`, and the pump work in §20.6 are superseded by a first-party
runtime that does all of it, better, and will be maintained by the Tauri team.
Writing them now would be building a private fork of a shipped crate.

Revised path:

1. **Now:** keep WebView2 as the only engine. It is measured stable (§20.5),
   has codecs (§20.3), and is sandboxed. No work.
2. **Now:** do not ship CEF. Its two hard blockers — codecs and the Windows
   sandbox — are properties of the official distribution, not of any
   integration RustBox could write.
3. **When Tauri 3.0 goes stable:** re-evaluate `tauri-runtime-cef` as a second
   build target. Re-run the §13.1/§20.5 probe against *its* pump before
   believing either the hang or its absence.
4. **Before any CEF build ships:** resolve H.264/AAC (self-built or trusted
   prebuilt distribution) and accept or fix the unsandboxed Windows renderer.

This replaces the M2 plan in the previous recommendation. The correct next action
is not to write `Surface::Chromium`; it is to track Tauri 3 and re-test then.

### 21.6 Unmeasured

Whether `tauri-runtime-cef`'s external pump reproduces the t≈6 s freeze. Whether
its default configuration plays H.264 (expected no, from the pinned distribution,
but not measured on that crate). macOS and Linux entirely. Tauri 3.0's stability
as a platform, which is alpha.

## 22. Can the runtime be a global setting, and what happens to sessions (2026-11-20)

Two questions that decide whether the "user picks the engine" feature is
implementable at all.

### 22.1 No in-process switch. "A binary can only run one runtime."

The runtime is chosen twice, both at build time: the Cargo dependency selects the
crate, and `tauri::Builder::runtime(..)` selects the attrs type (`Wry` or `Cef`).
Tauri 3 defaults to `DynRuntime`, which keeps *application* code portable — but
the dispatch target is fixed per binary. Calling a wry-only method on a CEF build
returns `RuntimeTypeMismatch` at runtime; the build does not become a hybrid.

Two further facts close the door:

- When both crates are linked, the Tauri CLI assumes CEF, because CEF is the one
  that must be bundled. There is no "either" build.
- On Linux, wry is GTK 3 and CEF is GTK 4. The `tauri` crate picks its GTK
  bindings at compile time, and the two toolkits cannot be initialized in one
  process.

So a settings toggle that swaps the engine inside a running app is **not
implementable**, and that is also the architecturally correct answer: the two
loops, two compositors and two storage roots coexisting in one process is exactly
the configuration that produced the dual-loop hang in §13.1 and that Electrobun
works around by never letting CEF pump (§18.2).

**What is implementable**, and is what "a global setting" should mean here:

| Shape | Mechanism | Cost |
| --- | --- | --- |
| Two executables, one config | `rustbox.exe` (wry) and `rustbox-cef.exe` share `config.json`; changing `engine` writes the file and relaunches into the other binary | Both must be installed, or CEF is an optional download; requires a restart |
| Two distributions | Default installer (wry, 14 MB) and a separate "Chromium edition" | User chooses at download time, not in settings |
| Settings toggle, one binary | **Impossible** (§22.1) | — |

A per-app engine choice is also ruled out by the same fact: RustBox's `Engine`
enum is per-app today, but the runtime is per-process. Choosing Chromium for one
app while another stays on WebView2 cannot be done in a single run. The setting
must be global, and the per-app `OsWebview` / `Chromium` distinction collapses
into "which build am I running".

### 22.2 Storage: what survives a switch, and what does not

RustBox's on-disk layout today (from `src-tauri/src/paths.rs`):

```text
rustbox\
  config.json             workspace config        <- engine-independent
  session.json            open tabs + last URLs   <- engine-independent
  EBWebView\              main-window WebView2 data
    WV2Profile_<app-id>\  per-app profile (ProfileMode::Light)
  profiles\<app-id>\      per-app profile (ProfileMode::Isolated)
```

Under `tauri-runtime-cef` the equivalent is a different tree entirely:

- Root: `{user cache}/{identifier}/cef` by default (`Cef::root_cache_path`),
  holding Chromium's user data directory — `Local State`, every profile, caches.
- Per app: a separate request context created from `WebviewAttributes::
  data_directory`, resolved relative to the root. Verified in
  `request_context_from_webview_attributes`, so `ProfileMode::Isolated` maps
  cleanly onto CEF.

| Item | Survives a runtime switch? |
| --- | --- |
| `config.json`, `session.json` | **Yes.** Plain JSON, no engine involvement |
| Tab URLs, window layout | **Yes**, via `session.json` |
| Cookies, saved logins | **Cookies: yes, since 2026-11-22** (§25.1). Saved logins (`Login Data`) are not migrated. |
| `localStorage`, IndexedDB | **Yes, since 2026-11-23** (§25.5): copied as files when the incoming profile has no session of its own. Cache Storage, service workers and Session Storage are not carried. |
| Granted permissions | **No** |
| `ProfileMode::Light` | **No equivalent.** CEF has no "named profile inside a shared environment"; it falls back to per-webview request contexts, i.e. `Isolated`, at roughly 2× the memory |

The encryption detail is the one that made this non-negotiable rather than
inconvenient. Chromium's `os_crypt` encrypts cookies and saved passwords against
a key stored per profile. The runtime's own `SecretStorage` docs state the
consequence for any key change: cookies encrypted under the previous key become
unreadable, **"logging users out"**. Moving between WebView2 and CEF is exactly
such a key change. The same section also warns that `SecretStorage::Auto` uses a
mock keychain in development and the OS store in release, so even dev and release
builds of the *same* engine do not share a session.

That is still true, and it is why the migration does not share a key: it
*re-encrypts* every row under the incoming engine's key (§25.1).

There is a second trap for pinned builds. `downgrade.rs` documents that Chromium
migrates a profile forward only, and its own warning text is explicit: when a
newer milestone has written the profile, "Cookies, local storage, IndexedDB and
granted permissions are gone." A CEF build pinned to Chromium 154 that meets a
profile written by 156 loses the session. WebView2 escapes this only because it
tracks the installed Edge runtime.

### 22.3 What this means for the feature

1. **The setting is a build selector, not a runtime selector.** Say so in the UI:
   changing it requires a restart and switches which executable runs.
2. **Never share a profile root between engines.** Keep
   `profiles/wry/<app-id>` and `profiles/cef/<app-id>` (or equivalent) so a
   switch is reversible and cannot corrupt the other engine's profile.
3. **Switching used to mean signing in again, everywhere.** That warning was
   honest when it was written. Since 2026-11-22 the cookies are carried across at
   startup, and since 2026-11-23 the two stores a chat app keeps its device
   pairing in (`IndexedDB`, `Local Storage`) are carried with them (§25.5), so a
   switch is usually invisible again. What is *not* carried is Cache Storage,
   session storage and service-worker state - derived data the browser rebuilds.
   A CDP export/import (`Network.getAllCookies` → `Network.setCookie`) would need
   a running webview on both sides and would still miss those stores, which is
   why the migration works on the files instead.
4. **`ProfileMode::Light` has no CEF meaning.** The settings UI must hide or
   re-label it per engine rather than offering a choice that silently costs 2×
   memory.

### 22.4 Unmeasured

Whether the CEF runtime's `data_directory` handling behaves identically for a
profile written by a different CEF version on the same milestone. Nothing here
was tested on macOS or Linux.

Which apps keep their session somewhere the migration still does not carry. The
site-storage copy (§25.5) is measured for WhatsApp only: it was a cookie-only
switch that failed for that app, and it succeeds now. Whether a store written by
one Chromium milestone is readable by a much older one is not established either;
the two that were measured are 157 (WebView2) and 154 (CEF).

## 23. The download-on-demand design, and what it costs (2026-11-20)

The proposed shape — wry by default, "CEF mode" as an option, payload downloaded
when first selected — is **implementable**, but not as a DLL that appears inside
an existing process. The constraint is in the binary itself.

### 23.1 Why it cannot be one binary plus a downloaded DLL

`tabs.exe` (the CEF harness) has `libcef.dll` in its **PE import table**. Verified
by parsing the import directory: 36 imported DLLs, `libcef.dll` among them.

That means the Windows loader resolves `libcef.dll` **before `main()` runs**. A
CEF-built executable cannot start, cannot print an error, and cannot fall back if
the DLL is absent — it dies at load time. `cef_load_library()` does not help: it
is compiled only `#if defined(OS_MAC)` (checked in
`include/wrapper/cef_library_loader.h`), so the runtime-load path exists on macOS
and nowhere else.

Combined with §22.1 ("a binary can only run one runtime"), the shape that works
is **two executables**, and the CEF one arrives as a complete payload:

```text
%LOCALAPPDATA%\rustbox\
  config.json                     "engine": "wry" | "cef"
  session.json                    open tabs, last URLs
  rustbox.exe                     wry build — always installed, 14 MB installer
  engines\cef\
    156.0.8078.88\                <- one directory per CEF build
      rustbox-cef.exe             CEF build (imports libcef.dll)
      libcef.dll                  281.0 MB
      resources.pak                23.5 MB
      icudtl.dat                   10.3 MB
      dxcompiler.dll               24.8 MB
      vk_swiftshader.dll            5.1 MB
      d3dcompiler_47.dll            4.5 MB
      chrome_elf.dll, v8_context_snapshot.bin, *.pak ...
      locales\                    220 files, 49.8 MB
      .ready                      marker written only after verification
```

Measured payload: **368.6 MB unpacked** across 16 top-level files plus 220
locale files; **166.9 MB** as the official `tar.bz2`. Trimming to a handful of
locales and dropping `CREDITS.html` / `bootstrap*.exe` takes it to roughly
**311 MB / ~140 MB compressed** — worth doing, not transformative.

### 23.2 The flow, end to end

1. **Install.** Installer unchanged: wry only, 14 MB, WebView2, today's
   behaviour. Nothing about the default user's experience moves.
2. **Select.** Settings → Engine → "Chromium (unduh ~140 MB)". This is the
   global setting from §22.3, and the UI must say it needs a restart.
3. **Download.** Fetch a signed manifest (version, URL, SHA-256, size) and the
   payload. Extract into `engines\cef\<version>.partial\`.
4. **Verify.** SHA-256 against the manifest, then **Authenticode on
   `rustbox-cef.exe` and `libcef.dll`** using the same certificate as the app. A
   hash from a manifest fetched over the same channel is not a trust boundary; a
   signature is.
5. **Commit.** Atomic rename `<version>.partial` → `<version>`, write `.ready`.
   A crash mid-download leaves a `.partial` directory that is deleted on next
   launch, never a half-installed engine.
6. **Handoff.** Write `config.json: engine=cef`, spawn
   `engines\cef\<ver>\rustbox-cef.exe`, then exit the wry process. The new
   process must not start until the old one has released `session.json` and the
   window, so this is a "spawn after exit" handoff, not a concurrent one.
7. **Health check.** If `rustbox-cef.exe` does not report ready within a bound
   (say 30 s), or exits within that window, revert `config.json` to `wry`,
   relaunch the wry binary, and show the captured stderr. This is the only thing
   that makes a 140 MB download safe to enable: a bad payload must degrade to
   the working app, not to a dead one.

RustBox already has most of the primitives: `reqwest` (with `blocking`, `socks`,
`native-tls`), `handle.restart()` in `tabs::restart_after_runtime_swap`, and a
debounced single-writer `session.json`. What is missing is a downloader, a
signature check, and the engine directory.

### 23.3 The costs that do not go away

| Cost | Detail |
| --- | --- |
| **~140 MB per Chromium security release** | `libcef.dll` is pinned to the Chromium version `rustbox-cef.exe` was compiled against. Exe and runtime are one unit; the DLL cannot be updated alone. Every CEF bump is a rebuild, a re-sign and a full re-download |
| **No H.264/AAC** | §20.3, measured. The downloaded payload is the official distribution. Users switching to CEF lose YouTube, WhatsApp voice notes and most MP4 |
| **Everyone logs out** | §22.2. Different profile tree, different `os_crypt` key |
| **No per-app choice** | §22.1. One runtime per process, so "this app on Chromium, that one on WebView2" is impossible in one run |
| **Windows sandbox off** | §21.3. The CEF build runs its renderers unsandboxed |
| **Signing burden** | Two executables and a 281 MB DLL to sign, notarize and keep notarized per release |
| **Per-platform packaging** | macOS: a second `.app` inside the first is awkward; Linux: a second binary plus `libcef.so` in deb/AppImage. Only Windows has a measured design |
| **AV / SmartScreen** | A freshly downloaded 281 MB DLL with no installer history is exactly what heuristics flag. Signed, it is fine; unsigned, the feature is unusable |

### 23.4 Is there a cheaper way to get the same thing?

Worth stating, because the goal behind "CEF mode" is usually *"stop the engine
changing under me"*, not *"I want Chromium specifically"*:

- **WebView2 runtime pinning (ADR 0006, already implemented).**
  `src-tauri/src/webview2_skew.rs` (32 KB) plus the `BrowserExecutableFolder`
  policy reader already pin the WebView2 runtime to a known-good version. That
  delivers the actual benefit — a fixed Chromium, no surprise update breaking a
  tab — with **zero download**, and keeps H.264/AAC and the sandbox.
- **What pinning does not give:** macOS/Linux consistency. WebView2 is Windows
  only, so a user on macOS still gets WKWebView. That is the one requirement CEF
  genuinely satisfies and pinning cannot.

So the download mechanism is worth building **only if cross-OS consistency is
the requirement**. If the requirement is stability on Windows, ADR 0006 already
answers it for free.

### 23.5 Verdict

Implementable, and the design above is the one to build — but not yet, and not
before the codec question is answered. Shipping "Chromium mode" that cannot play
video is a worse product than not offering it, because the user pays 140 MB and a
full logout to get a browser that fails on YouTube.

Order of work if it is pursued:

1. Tauri 3 stable (it is alpha today; `tauri-runtime-cef` is `3.0.0-alpha.7`).
2. ~~A codec-complete CEF distribution, self-built or trusted prebuilt (§20.7).~~
   **Done on Windows (§24), measured.**
3. Decide the supply-chain route: rebuild from the repo's scripts, or vendor the
   prebuilt pinned by SHA-256 (§24.5).
4. Re-run the §20.5 hang probe against `tauri-runtime-cef`'s own pump.
5. Then the downloader, signature check and handoff above.

### 23.6 Unmeasured

Whether the CEF build starts and renders when launched from
`engines\cef\<version>\` rather than beside the app — the directory layout is
inferred from how CEF resolves resources relative to the executable, not tested.
Whether Authenticode verification of a 281 MB DLL is fast enough to run on every
launch or must be cached in `.ready`. The macOS and Linux packaging shapes. The
30 s health-check bound.

## 24. Does a codec-complete prebuilt exist, and does it work (2026-11-20)

§20.7 left two open questions: is there a prebuilt CEF with H.264/AAC, and can
the Tauri 3 CEF runtime be pointed at it. Both were answered by measurement, not
by reading.

### 24.1 The official index has no codec variant

Downloaded `https://cef-builds.spotifycdn.com/index.json` (10.4 MB, the CDN
`download-cef` uses by default) and searched it: **zero** occurrences of
`codec` and zero of `spotify`. Every distribution is `minimal`, `client`,
`debug_symbols` or `release_symbols` — the codec dimension does not exist in the
official build matrix. So "just pick the codec build from CEF" is not a thing
that exists to pick.

### 24.2 The version coincidence that makes this work

`tauri-runtime-cef` `3.0.0-alpha.7` pins `cef = "=154.5.0"` and
`cef-dll-sys = "=154.5.0"` exactly. Resolved against the crates.io index:

| Crate | Pinned version | Resolves to CEF |
| --- | --- | --- |
| `cef` | `=154.5.0` | `154.5.0+154.0.34` |
| `cef-dll-sys` | `=154.5.0` | `154.5.0+154.0.34` |

`psmux/godwinmix-cef` publishes exactly `cef-154.0.34`. The prebuilt is not
"close enough" to the pinned version — it is the same version, which matters
because `cef_api_hash` rejects a `libcef` whose API version differs from the
one compiled in. A 155 or 156 codec build would not work with that crate.

### 24.3 Built and run against it

Downloaded the Windows archive, verified it, extracted it, and built the same
`tabs.rs` harness the rest of this ADR uses, with only the dependency changed:

```toml
[dependencies]
cef = { version = "=154.5.0", features = ["build-util"] }
```

```
CEF_PATH=%TEMP%\cef-codecs
CEF_DOWNLOAD_URL=https://github.com/psmux/godwinmix-cef/releases/download/cef-154.0.34
cargo build --release --bin tabs
```

Result: `Compiling cef-dll-sys v154.5.0+154.0.34`, `Compiling cef
v154.5.0+154.0.34`, build succeeded. The crate fetched from the third-party
release using only its documented `CEF_DOWNLOAD_URL` mechanism — no patching, no
vendoring, no forked crate. Archive layout, naming and `index.json` are
interchangeable with the official ones, as claimed.

Verified archive integrity independently rather than trusting the downloader:

| Check | Result |
| --- | --- |
| Downloaded size | 165.7 MB |
| Published SHA-256 vs computed | **match** (`af1f1887…c884ce`) |
| `include/cef_version.h` | `154.0.34+g14c5a08+chromium-154.0.8037.98` |
| `Release/libcef.lib` present | yes — linkable, not just runtime files |
| `Release/libcef.dll` | 278.7 MB |

### 24.4 The codecs actually decode

`canPlayType` only reports intent, so both distributions were asked for a
decoded frame: a `<video>` pointed at an MP4, polled until `readyState >= 2`
(HAVE_CURRENT_DATA) or error. Same probe script, same host, same harness.

| Distribution | `readyState` | `videoWidth × height` | `duration` | `error` |
| --- | --- | --- | --- | --- |
| psmux CEF 154.0.34 | **4** (HAVE_ENOUGH_DATA) | **320 × 176** | **10 s** | — |
| official CEF 156.0.26 | 0 | 0 × 0 | NaN | **code 4** (SRC_NOT_SUPPORTED) |

Reported capabilities on the psmux build:

```json
{"h264":"probably","h264_mse":true,"aac":"probably","aac_mse":true,
 "hevc":"probably","ua":"…Chrome/154.0.0.0…"}
```

Official CEF 156 returns `""` for H.264 and AAC with MSE false (§20.3). So the
codec blocker in §20.3 is **real, measured, and fixable by choosing a different
distribution of the same CEF version**.

Widevine is still absent: `{"widevine":"unavailable: no-api"}`. That is
independent of codecs — the CDM is a separate binary shipped under a Google
licence that CEF does not carry in any distribution. Netflix, Spotify and other
DRM services stay unavailable on either build. Electron needs the castlabs fork
for the same reason (§20.2).

### 24.5 What this does not fix

| Gap | Status |
| --- | --- |
| **Supply chain** | Single maintainer, 3 stars, created 2026-10-15, two releases, **no CI workflows**, no build attestation. `libcef.dll` is **`NotSigned`** — as is the official one, so signing is not a differentiator, but provenance is |
| **Patent licence** | The repo states plainly that H.264/AAC are covered by Via LA pools and that these builds "come with no patent licence of any kind", leaving the decision to whoever ships them |
| **Windows sandbox** | Unchanged (§21.3). The runtime runs renderers unsandboxed whatever the policy says |
| **GPU process** | Still dies every run on this host: `GPU process exited unexpectedly: exit_code=-2147483645`, then `Failed to create shared context for virtualization`. Same as §19.1 — codecs do not touch the rasterizer |
| **macOS / Linux** | Archives exist for `macosarm64`, `macosx64`, `linux64`, `linuxarm64`, but nothing was run there. The stable message-loop configuration is still Windows-only (§20.7) |
| **Per-release cost** | Still a rebuild + re-sign + full re-download per Chromium security release (§23.3) |

The supply chain question is the honest one, and it has a cheap answer that does
not depend on trusting the maintainer: **you can rebuild it yourself with the
same two scripts.** The repo ships `build.sh` and `publish.sh`, and the only
inputs are the CEF version string. That converts "trust a stranger's binary" into
"trust your own build host", at the cost of the ~100 GB checkout and build hours
already noted in §20.7. The prebuilt is then a convenience, not a dependency.

Size, measured on the extracted tree: 418.2 MB deployable (17 top-level files
369.8 MB + 220 locales 48.5 MB), 165.7 MB as `tar.bz2`. That is larger than the
156 minimal distribution measured in §23.1 because this one ships `libcef.lib`,
headers and CMake files. Trimming to a runtime payload — `libcef.dll`,
`resources.pak`, `icudtl.dat`, the three compiler DLLs, a handful of locales —
lands back near the §23 figures, roughly 300 MB / ~140 MB compressed.

### 24.6 Effect on the verdict

§23.5 said "not before the codec question is answered" and listed a
codec-complete CEF as work item 2. **That item is now done, on Windows.** The
remaining order of work becomes:

1. Tauri 3 stable (still alpha; `tauri-runtime-cef` is `3.0.0-alpha.7`).
2. Decide the supply-chain route: republish our own build from `build.sh`, or
   vendor this one pinned by SHA-256, or vendor nothing.
3. Re-run the §20.5 hang probe against `tauri-runtime-cef`'s own pump.
4. Then the downloader, signature check and handoff of §23.2.

What does not change: the default stays WebView2, CEF stays opt-in, the switch
still logs everyone out (§22.2), and one binary still cannot run both runtimes
(§22.1).

### 24.7 Unmeasured

Whether `tauri-runtime-cef` itself renders video with this distribution inside a
Tauri app — the measurement above is the `tabs.rs` harness, which uses the same
`cef` crate at the same version but not the Tauri runtime. Whether the crate's
`Cef` builder accepts a non-official `CEF_PATH` at *build* time (expected yes,
since `cef-dll-sys` is what resolves the path, and it succeeded here). macOS and
Linux builds of the same archives. Whether `build.sh` reproduces a byte-identical
archive, or merely a functionally equivalent one.

## 25. Where the data lives, and letting the user move it (2026-11-22)

Two questions that came up with the engine setting: does switching engines keep
a session, and can the data folder itself be moved.

### 25.1 Both engines are Chromium on disk, and that still does not make storage portable

Measured on this machine (Windows, 2026-11-22):

| Engine / mode | Cookie database | `os_crypt.encrypted_key` |
| --- | --- | --- |
| WebView2 `Isolated` | `%LOCALAPPDATA%\rustbox\profiles\<app>\EBWebView\Default\Network\Cookies` | own key per app, DPAPI description `Microsoft Edge` |
| WebView2 `Light` | `%LOCALAPPDATA%\rustbox\EBWebView\WV2Profile_<app>\Network\Cookies` | inherits the shared environment's `EBWebView\Local State` |
| CEF | `<root>\cef\<app>\Network\Cookies` | one key for the whole engine root, in `<root>\cef\Local State`, description `Chromium` |

The formats are the same family (WebView2 *is* Chromium), but three things stop a
plain copy from carrying a login:

1. The layout differs (`Default\Network\Cookies` vs `Network\Cookies`), and the
   `Local State` that holds the key sits at a different level in each tree (per
   app under `EBWebView`, once per engine under `cef`).
2. `encrypted_key` is per user-data directory, and the cookie values are AES-GCM
   under that key. Copy the DB without the matching `Local State` and every row
   is undecryptable - and Chromium does not merely ignore those rows, it
   **deletes them** on the next run (measured: a copied jar went from one row to
   zero, and the page saw no cookie).
3. The two jars are not the same schema: WebView2 adds `is_edgelegacycookie` and
   `browser_provenance` (22 columns) where CEF has 20. CEF tolerates the extra
   columns on read, but nothing maintains them.

**The DPAPI description is a label, not a barrier.** `CryptUnprotectData` on a
key whose description says `Microsoft Edge` succeeds in a process that is
neither Edge nor Chromium, and re-wrapping the same 32 bytes under a description
of `Chromium` unwraps to the same key. So a key *can* be carried across engines;
what it cannot do is decrypt rows written under a different key (`InvalidTag`).

**Therefore a login can be carried across engines, and it was measured.** In a
controlled test (local HTTP server, one app):

- copy the WebView2 jar into a CEF profile and leave CEF's own key: the cookie
  is not served and the rows are deleted;
- copy the same jar and seed `cef\Local State` with the WebView2
  `os_crypt.encrypted_key`: CEF serves `probe=alive` to the page.

The alternative, re-encryption, was also proven: decrypt a row with the source
key, re-encrypt with the target key (`v10` + 12-byte nonce + AES-256-GCM over
`SHA-256(host_key) || value`), and the target key reads it back while the source
key rejects the new blob.

What is *not* encrypted - `localStorage`, IndexedDB, Cache Storage, service
worker registrations - can be copied outright, because there is no key to
translate; §25.5 is that copy, measured.

**Consequence, and what the UI says:** a switch is normally a one-time login in
the other engine. Each engine keeps its own stable profile (`profiles\<app>` /
`cef\<app>`, both derived from the app id), so switching back to the engine used
before restores that engine's session untouched.

**Built, and measured end to end (2026-11-22).** `src-tauri/src/cookie_migrate.rs`
runs at startup, immediately after `data_root::startup` and before any webview
exists, and re-encrypts every embedded app's rows into the incoming engine's jar
with that engine's key (`bcrypt.dll` for AES-256-GCM, `crypt32.dll` for DPAPI).
Two things had to be true for it to work, and both were only known by running it:

1. **The target `Local State` has to exist with a key in it.** When the incoming
   engine has never run, the module generates a 32-byte key, DPAPI-wraps it, and
   writes `os_crypt.encrypted_key` into the target's `Local State` before writing
   any row. That is what Chromium does on its own first run.
2. **WebView2 will not adopt a CEF-shaped jar.** It does not read the rows and
   ignore what it does not know - it replaced the whole file with an empty one
   (measured: 1 row in, `no such table: cookies` out). Adding the two columns is
   not enough; the copied jar also needs `meta.edge_version_number` and
   `meta.compatible_edge_version_number` (both `3`), which a jar WebView2 builds
   for itself carries. With the columns and those two rows, WebView2 reads the
   migrated rows.

The direction that needs no schema work is the other one: CEF reads a 22-column
jar happily, so WebView2 → CEF is a straight re-encrypt. Both directions were
verified against real engines and a local HTTP server:

- WebView2 → CEF: `carried 1 cookies from the OS webview to Chromium`, and the
  CEF page received `seeded=yes`.
- CEF → WebView2: `carried 1 cookies from Chromium to the OS webview`, and the
  WebView2 page received `seeded=yes`.

The row that the second test migrates is not hand-built: phase 1 lets WebView2
write a cookie for a local page, and the test reuses that row's column values, so
the only invented part is the key. Cookies are not the whole session for every
app - see §25.5 for the stores that had to follow them.

### 25.2 Moving the data root: the pointer lives outside it

`config.json` cannot name its own folder and still be found, so the location is
recorded in a separate one-line file at the default location
(`%LOCALAPPDATA%\rustbox\data-root.txt`, `paths::LOCATOR`). `paths::root()`
resolves once per process: locator if it names an existing absolute directory,
otherwise the default, with the reason surfaced to the settings page.

The move cannot run when the user asks for it. WebView2, CEF and every profile
hold open handles on `Cookies`, `Local State` and `libcef.dll`; copying an open
SQLite database yields a torn copy. So the request is recorded
(`Config::pending_data_root`) and applied first thing in `main`, before a single
profile is opened:

1. copy the tree and check every file arrived at its full length (`manifest` vs
   `verify`, file count and byte total);
2. save the config in the new folder with the request cleared;
3. write the locator - the moment the new folder becomes authoritative;
4. delete the old folder, best effort.

A failure in step 1 changes nothing: the locator still names the old folder, the
process keeps using it, and the reason is written to `data-move-error.txt` beside
the locator for the settings page. Refusals are checked before scheduling, where
the user can answer: not absolute, inside the current root, a parent of it, a
file, not writable, or already holding files (merging two workspaces is not a
move).

**Scheduling used to write a second copy of the config and nothing else (fixed
2026-11-23).** `schedule` re-read `config.json` from disk, set the field on that
fresh parse and saved it, leaving the live `AppState::config` untouched. Two
consequences, both user-visible:

1. `data_move_state` reads the live struct, so the pane reported "nothing
   queued" right after a successful save. The radios then fell back to "Default
   folder", which reads as *"I picked a custom folder, saved, and it still points
   at the default one."*
2. Every unrelated save (`save_apps` after editing an app, `set_favorites`,
   `bump_usage`, the settings form) serializes the whole live struct, so it
   overwrote `config.json` and **erased the queued move**. The restart then
   simply did nothing.

`schedule` is now split: `plan(target)` does the filesystem checks and returns
the target, `schedule(to, &mut cfg, path)` sets the field on the live config and
saves. The lock is taken for the second call only - the checks include a write
probe, which on a network share is not something to hold the config mutex
across. The pane also treats a queued move as the user's answer, so the radio
lands on "Custom folder" with the target filled in, instead of snapping back.

**A move could also fail after the restart, with a message that named the wrong
problem (fixed 2026-11-23).** The check at scheduling time was "writable", not
"big enough". Measured: a 12.34 GiB data root was accepted for `F:\rustbox`, a
drive with 7.00 GiB free; the copy wrote 0.76 GiB in 2,570 files and then died on
`cef\<app>\Network\Cookies` with `os error 32` (sharing violation). Three
separate defects hid behind that one error line:

1. **No space check.** `probe_writable` writes two bytes and succeeds on a full
   drive. `plan` now measures the tree and asks the target volume
   (`GetDiskFreeSpaceExW`) whether it fits, with **2 GiB of headroom**, and
   refuses with the numbers in the message ("needs 12 GB for the data plus 2.0 GB
   of headroom, and that drive has 7.0 GB free (7.4 GB short)"). `move_root`
   repeats the check, because free space can change between scheduling and the
   next launch.
   
   The margin is 2 GiB rather than the megabytes a byte-for-byte copy needs, for
   two reasons. A Chromium profile is a live database that grows while the user
   browses: between picking the folder and the next start, a chat app can write
   hundreds of megabytes of IndexedDB, so a move that fits by 50 MB at scheduling
   time does not fit by then. And a drive filled to the last block is a broken
   drive for everything else on it. Refusing with 2 GiB to spare costs a folder
   choice; accepting and running out costs a half-copied tree on a full volume.
2. **A half-copied tree was kept.** `free_for_use` refuses any target holding
   files, so a failed move left a folder that made *every* retry answer "already
   contains files" until the user deleted 0.76 GiB by hand. `move_root` now
   rolls the target back to empty on failure, keeping only the locator.
3. **A transient lock was treated as fatal.** The move runs at the start of the
   next launch, but the previous instance may not have finished exiting - closing
   the window does not wait for Chromium's ~50 child processes, and its profile
   files stay open until they are gone. `copy_file` now retries a sharing or lock
   violation (`os error 32`/`33`) for ~3.5 s before failing, and its error says
   "another RustBox window still has the file open" instead of the bare errno.

**The move is now all-or-nothing (2026-11-23).** The order was already the safe
one - copy, verify, write the locator, and only then delete the source - but three
paths could still leave the target holding files that the next attempt would be
refused for. All three now roll back:

- **A failed copy.** Covered above: `rollback(to)` empties the target.
- **A failed locator write, after a successful copy.** The target was left full
  and the source intact, so the *data* was safe, but every later attempt was
  answered with "already contains files". `apply` now rolls the target back on
  that path too, and on a failed config write.
- **A source that was still being written to.** A new consistent-copy rule:
  the tree is measured before the copy and again after. If the numbers moved, the
  attempt is discarded and retried (3 attempts, 2 s apart), because a file count
  that matches says nothing about *consistency*. Two leveldb files copied a
  second apart can each be whole and still describe different moments.

`rollback` keeps the locator, since it describes a location rather than being
data. That is what makes the "move back to the default folder" retry work.

`plan` also moved off the IPC thread (`spawn_blocking`), because measuring the
tree walks 110k files and took 3.1 s here - a sync command would freeze the
settings pane for that long.

### 25.3 The shell window has to be told, and Tauri only accepts one particular hook

Tauri derives the shell webview's data directory from
`app_local_data_dir()` and forces it on Windows/Linux
(`manager/webview.rs`). That path comes from the bundle identifier, so after a
move the shell - and every `Light` profile, which lives *inside* the shell's
WebView2 environment - would keep writing to the old folder while every `Isolated`
profile moved, and `Light` apps would come back logged out.

Two hooks exist; only one works:

- `app.windows[].dataDirectory` / `WebviewWindowBuilder::data_directory` with an
  **absolute** path is silently ignored by Tauri 3 alpha
  (`webview/mod.rs`: `"data_directory ... is not a relative path, ignoring
  config"`). Measured: the shell kept writing to the old location.
- `app.appDirectoriesOverride.localData` redefines `app_local_data_dir()`. Set on
  the `Context` in `main` before `.build(context)`, it moves the shell with
  everything else. This is what ships.

Verified end to end (debug build, `RB_DATA_ROOT` + `RB_DATA_ROOT_LOCATOR`):

- queued move: `moved 2 files`, old folder deleted, new folder holds
  `config.json`, `profiles\`, and a fresh `EBWebView\`; the locator names the new
  folder; `pending_data_root` is `null`;
- next start with only the locator: window opens, data read from the new folder,
  the default location untouched;
- same with `rustbox-cef.exe`: `cef\`, `profiles\`, `engine-ready.json` and
  `config.json` all in the new folder;
- refused move (target already holds a file): source intact, no locator written,
  error recorded, request cleared.

Not measured: a release-build move (same code path, no `RB_DATA_ROOT`), the move
of a 400 MB engine payload plus profiles on a slow disk (it is a full copy, so it
takes time proportional to the data), and macOS/Linux, where there is no native
folder picker yet - the text field is the only route there.



### 25.4 One CEF profile per app: the path has to be a direct child of the engine root

The first port put app profiles at `cef\profiles\<app>`, mirroring the WebView2
`profiles\<app>` layout. That silently did nothing: Chrome's profile manager
only accepts a profile directory whose parent *is* the user data directory, and
refuses anything deeper (`cef.log`: `Cannot create profile at path
...\cef\profiles\<app>`). The tab still opened - on the shared `Default`
profile - so every CEF app ended up in one cookie jar, which is exactly the
isolation the per-app profile was supposed to provide.

Measured after the fix (`cef\<app>`, a direct child):

- `cef\<app>\Network\Cookies` is created and holds the app's rows;
- `cef\Default\Network\Cookies` stays empty, i.e. the app is not sharing the
  shell's jar;
- a cookie set by the page survives a graceful close (the row is still there
  afterwards), while a hard kill leaves the DB without it - CEF flushes cookies
  on shutdown, not on every write.

## Consequences

- **The hand-built CEF seam is cancelled (§21).** Tauri 3.0 ships
  `tauri-runtime-cef` as a first-party webview runtime: Apache-2.0/MIT, from
  `tauri-apps/tauri`, 15,455 lines. `EmbeddedBackend` inside `os-webview` and a
  `Surface::Chromium` variant would be a private fork of a shipped crate.
- **The engine cannot be a runtime toggle (§22.1).** A binary runs one runtime;
  Linux GTK 3 vs GTK 4 makes a hybrid impossible there. The "global setting"
  can only select which executable is launched, which means a restart and either
  both binaries installed or an optional Chromium download. It also ends the
  per-app engine choice: the runtime is per-process, so `Engine::Chromium`
  cannot be per-app.
- **Switching engines carries the session: cookies, and since 2026-11-23 the
  stores that hold a device pairing (§22.2, §25.1, §25.5).**
  `config.json` and `session.json` survive a switch, and so do cookies: on
  Windows, `cookie_migrate` re-encrypts every embedded app's rows into the
  incoming engine's jar at startup, verified in both directions against the real
  engines (§25.1). `IndexedDB` and `Local Storage` are copied as files, because a
  chat app keeps its pairing there and a cookie-only switch left WhatsApp asking
  for a QR code. Cache Storage, service-worker state and granted permissions are
  *not* carried; they are derived data the browser rebuilds. Profiles must still
  never be shared between engines. The rest of the platforms have no equivalent:
  the DPAPI/CNG path is Windows-only.
- **Each CEF app gets its own profile, as a direct child of `cef\` (§25.4).**
  A nested `cef\profiles\<app>` is refused by Chrome and silently shares
  `Default`, which would put every Chromium app in one cookie jar.
- **The data folder is relocatable, on the next start (25).** A pointer file at
  the default location names the real root, because `config.json` cannot say
  where it lives and still be found. The move is deferred to startup - a running
  browser holds its cookie database open - and only becomes authoritative once
  the copy has been verified and the locator written. The shell window follows
  through `appDirectoriesOverride.localData`, not through the window
  `dataDirectory`, which Tauri silently ignores for absolute paths.
- **An on-demand download is implementable, as two executables (§23).** Verified:
  `libcef.dll` is in the CEF binary's PE import table, so a CEF build cannot start
  without its ~311–369 MB payload — it is not a DLL that can be dropped into a
  running process. The viable shape is a wry installer (14 MB) plus an optional,
  signed `engines\cef\<version>\` payload (~140 MB compressed) with a health
  check that reverts to wry on failure. ADR 0006's WebView2 runtime pinning
  already delivers the "engine stops changing" benefit for free on Windows; the
  download buys cross-OS consistency only.
- CEF is consequently a **whole-app build variant**, not a per-app toggle: the
  dependency in the manifest selects the runtime for the entire toolchain. The
  product shape becomes "default installer (WebView2, 14 MB) plus an optional
  Chromium build", which is defensible but is not the per-tab seam §2 designed.
- A RustBox tab can be Chromium on macOS for the first time, and on Windows can
  escape the WebView2 skew class — but only in that optional build.
- The Windows coexistence question is settled (§20.6): a Tauri host keeps its
  message loop, WebView2 keeps the UI, and CEF runs on its own thread with
  Electron-class responsiveness (0/765 hung probes, worst 3 ms). It is therefore
  known-implementable — though `tauri-runtime-cef` uses the external-pump family
  instead, whose own behaviour is unmeasured.
- That result is **Windows-only**. CEF needs the main thread on macOS/Linux, so
  the `multi_threaded` shape does not exist there.
- One of the two distribution blockers is **solved, measured** (§24): a prebuilt
  CEF at the exact pinned version decodes real H.264 (`readyState` 4, 320×176)
  where the official build returns `SRC_NOT_SUPPORTED`. What replaces it is not a
  code problem but a policy one — an unaudited single-maintainer binary, no build
  attestation, and no patent licence for H.264/AAC, so the honest route is to
  rebuild it from the repo's own scripts rather than vendor a stranger's DLL. The
  other blocker, **an unsandboxed Windows renderer** (stated by the runtime's own
  `sandbox.rs`, not fixable without packaging the app as a bootstrap-hosted DLL),
  remains and is not RustBox's to solve.
- RustBox starts carrying a browser: its own Chromium security cadence, hundreds
  of MB per platform, and a signing requirement that does not exist today.
- ADR 0001's "Embedded engine: OS webview" becomes "OS webview by default,
  optionally bundled Chromium".
- `ProfileMode` stops meaning the same thing on both backends: under CEF it is
  storage isolation only, because one CEF client is one process tree (§6.1). The
  UI has to say so rather than offering a choice that behaves identically.
- Every future tab feature must be implemented for two backends or abstracted.
  That is the recurring cost, not the one-time work.

### 25.5 Carrying site storage, because cookies are not the whole session (2026-11-23)

§25.1 carries the cookie jar and nothing else. That is enough for Gmail, whose
login is a cookie, and **not** enough for WhatsApp, which keeps its device pairing
in IndexedDB (`model-storage`, and the `wawc` / `wawc_db_enc` databases) and asks
for a QR code when it finds them empty. The user hit exactly that: Gmail came
across signed in, WhatsApp did not.

Measured on this machine, one app (`app-mvixi8yk-ck5w5`):

| Store | WebView2 profile | CEF profile before the copy |
| --- | --- | --- |
| `IndexedDB` | 403,004,098 B / 310 files (WhatsApp origin 385.8 MB + 10.4 MB blob) | 449,857 B |
| `Local Storage` | 417,727 B / 11 files | 2,452 B |
| `Service Worker` | 517,663,664 B / 3,896 files | - |

The CEF IndexedDB log said it plainly: `existing version for the database:
model-storage is null`, i.e. the site built its database from scratch. After
copying `IndexedDB` + `Local Storage` and nothing else, the same log shows a
21,604-key compaction and the app came up signed in - verified through its own
MCP bridge, `{"title":"(81) WhatsApp","qr":false,"pane":true}`, 81 unread.

**These stores need no key.** Unlike the jar, they are LevelDB the browser reads
as-is, so the copy is a file copy; the Chromium milestone difference (157 on
WebView2, 154 on CEF) did not matter, because the database names and versions the
site checks are the site's own.

**What is copied:** `IndexedDB` (both the `.leveldb` and `.blob` directories - the
records point at the blob store, so one without the other is a database pointing
at nothing) and `Local Storage`.

**What is not, and why:** `Cache`, `Code Cache`, `GPUCache`, `Service Worker`,
`Shared Dictionary` are derived data that the browser rebuilds; copying them would
add 518 MB per WhatsApp profile and buy nothing. `Session Storage` is cleared when
the browser closes, so it carries nothing across the restart a switch requires.

**When it refuses.** The copy runs per store, and only when the incoming engine's
store is not worth keeping: `target_bytes > 16 KB && target_bytes * 4 >=
source_bytes`. A directory that holds Chromium's scaffolding, or the small
database a site writes when it finds no session (449 KB against a 386 MB source),
is a stub and is **replaced**, not merged - two leveldbs cannot be merged by
copying files, and the stub is what produced the QR code. A target of comparable
size is a session the user has been using on the new engine, and overwriting it
would sign them out to fix a problem they may no longer have. That also makes the
switch back safe: returning to WebView2 finds its own 403 MB store intact and
leaves it alone.

`src-tauri/src/cookie_migrate.rs` implements this (`carry_site_storage`,
`worth_keeping`), runs it from the same startup hook as the cookies, and reports
what it did in the same note. Eight unit tests cover the cases that matter: an
empty target is filled, a stub is replaced, a comparable target is untouched,
scaffolding does not block the copy, nested leveldb directories keep their shape,
an unknown profile root is skipped rather than guessed, and the size boundary.

### 25.6 Unmeasured

Whether the copy helps an app other than WhatsApp; it was measured for one. Whether
a store written by a much newer Chromium is readable by a much older one (157 →
154 is fine; 160 → 140 is not established). Whether an interrupted copy - power
loss mid-`copy_tree` - leaves a profile the browser accepts; the copy is per
directory and a torn leveldb is normally recovered by LevelDB's own log replay,
but that was not tested.

### 25.7 A "logged out" app that was never logged out (2026-11-23)

After the switch to CEF, all three Gmail apps opened on Google's account chooser,
which reads as "the migration lost my sessions". It had not. The cookies were
intact and byte-identical to the WebView2 jar: `SID`, `HSID`, `SSID`, `APISID`,
`SAPISID` and all four `__Secure-*PSID*` rows decrypt to the same values on both
sides, all `v10`, all unexpired, and Google answers `301 -> /mail/` for them.
Pointing one app back at `mail.google.com` showed `Inbox (47,624)` with no
sign-in at all.

The real cause was **session restore**: a tab had been remembered while it sat on
`accounts.google.com/v3/signin/accountchooser?continue=...`, and that URL was
restored on every launch. The chooser does not navigate on its own - it waits for
a click - so the app came back on a page that says "choose an account" and stayed
there. Saving it again made the state permanent. `session::settle` now rewrites a
sign-in page back to the page it was returning to (`continue` for Google,
`redirectTo` for better-auth, `next`/`return_to`/`followup` for the rest), on both
the save and the restore path, so an existing profile repairs itself on the next
launch. A sign-in page with no return target is left alone.

Three checks worth keeping, because each one rules out a different theory:

- **Not app-bound encryption.** Every cookie is `v10`; `v20` (App-Bound) never
  appears, so `read_key`'s DPAPI path is the right one for both engines.
- **Not DBSC.** CEF creates a `Device Bound Sessions` SQLite file and WebView2
  does not, but its `dbsc_session_tbl` is empty: nothing was ever bound.
- **Not the migration.** The rows' `creation_utc`/`last_update_utc` are identical
  across engines, which is only true if they were copied rather than reissued.

The apps that *were* genuinely signed out (`9Router`, `Vault`, four `dtakah`
staff accounts) were signed out on WebView2 as well, with the same tokens:
`dtakah`'s own `better-auth` recorded `{"trigger":"signout"}` on 2026-11-14, nine
days before the switch. Nothing was lost there and nothing was recoverable from
the old profile - the old profile holds the same dead token. The one account that
still worked (`superadmin@kemhan.go.id`) proves the domain was reachable, not
that the engine was at fault.

### 25.8 Unmeasured

Whether `settle` is the right rule for a site whose sign-in page *is* the page the
user wants to keep (a deliberate "log in as someone else" tab). It is rewritten to
the destination, so that tab comes back signed-in-or-redirected rather than on the
form. Whether other engines' auth flows use a parameter outside the five listed.

## 26. Shipping it: one installer, two executables (2026-11-23)

§23 concluded that an on-demand engine download has to be **two executables**, and
that conclusion was implemented as a launcher plus a separately downloaded engine.
The installer, however, still shipped only the launcher. That produced a broken
state rather than a missing feature: the payload downloaded and unpacked, `.ready`
was written, `installed_versions()` reported the engine as installed, the settings
page offered a switch to it - and `rustbox-cef.exe` had never been installed, so
the hand-off had nothing to start. Nothing repaired it either, because the only
startup repair path treats "no Chromium build beside the launcher" as a normal
state (a source build). Verified on the affected install: `engines\cef\154.0.34\`
held 362 MB of payload and no binary.

### 26.1 Why both binaries are in the installer

The alternatives were considered and rejected on measurement, not preference.

| Option | Shape | Why not |
| --- | --- | --- |
| A (chosen) | One installer, `rustbox.exe` + `rustbox-cef.exe` + `rustbox-mcp.exe`; payload still downloaded on demand | - |
| B | Offline installer with the 173 MB payload embedded | +173 MB for every user, including the majority who never pick Chromium, to solve a problem the download already solves |
| C | One `.exe` that switches engine at run time | **Impossible**, measured below |

C is not a packaging question. `libcef.dll` is a *load-time* import of the
Chromium build: without it the process dies in 0.4 s with `0xC0000135`. Relinking
with `/DELAYLOAD:libcef.dll` does work at the PE level - `libcef.dll` moves from
STATIC to DELAY in the import directory - but the process then dies at 5.7 s with
`0xC06D007E` at the first CEF call, because `engine::apply` always constructs the
CEF runtime. Delay-loading moves the failure, it does not remove it. On the Tauri
side, `Builder::runtime` is a compile-time generic over `runtime::Runtime`, which
has associated types and `Sized`; a runtime `dyn` switch does not type-check, and
merging the two builds would need two monomorphisations across 67
`#[cfg(feature = ...)]` sites, 78 `#[tauri::command]`s and 184
`AppHandle`/`WebviewWindow`/`State<>` mentions.

A costs one extra executable in the installer (33 MB) and keeps the "one shortcut,
one config" property, because `handoff.rs` starts whichever binary the config
names. That is what shipped.

### 26.2 `.ready` no longer means "installed"

The bug above was a contract bug, not a download bug. The fix makes the binary
part of the definition:

- `payload_complete_in` - the payload is unpacked, hashed and marked `.ready`.
  This is the weaker precondition: what `stage_binary_in` accepts, and the
  re-entry guard in `install_into`.
- `is_installed_in` - `payload_complete_in` **and** `rustbox-cef.exe` present.
  This is what `installed_versions()` reports.
- `unrunnable_versions()` - payload complete but binary missing. Reported
  separately so the UI can offer **Repair** (which skips the 173 MB download and
  repeats only the copy) instead of **Use this** (which the backend would reject).

`install()` now stages the binary as its last step, so an `Ok` means the engine can
actually start. A staging failure leaves the payload on disk, which the re-entry
guard recognises: the retry finishes the copy rather than re-downloading 173 MB to
recover from a 33 MB copy.

### 26.3 Two build constraints found while making the installer

Both were discovered by the release path failing, and both are recorded here
because neither is obvious from the code.

**The Chromium build needs the MSVC environment.** It is the only part of the
project that compiles C++: `cef-dll-sys` drives CMake over 123 wrapper translation
units. CMake's Ninja generator passes the MSVC include and library paths through
the *environment* instead of baking them into the build files, so a shell without
`vcvars64.bat` fails with `fatal error C1083: Cannot open include file:
'stdint.h'` - which reads like a broken checkout. `scripts/build-installer.mjs`
now checks `INCLUDE` and says so explicitly. Ordinary `cargo build` of the wry app
does not need it: rustc finds its own linker and system libraries.

**The Chromium build must not live at a deep path.** Each object is written under
`<target>/release/build/cef-dll-sys-<hash>/out/build/libcef_dll_wrapper/
CMakeFiles/libcef_dll_wrapper.dir/cpptoc/test/<name>.cc.obj` - 52 characters of
cargo scaffolding plus 114 for the deepest CEF source. Measured on a nested
checkout, `src-tauri/target/cef` put the longest object path at **262** characters
against a 260 limit, and cl.exe reported it as `fatal error C1083: Cannot open
compiler generated file: ''`, naming neither the file nor the limit. The same
sources compiled without complaint in a 105-character target dir (object paths
217), which is what identified the cause. `LongPathsEnabled=1` was set and did not
help: cl.exe and Ninja are not long-path aware. The script therefore builds into
`<temp>/rustbox-cef-target` (230-character worst case) and refuses to start if a
caller-supplied `RB_CEF_TARGET` would still be too deep.

### 26.4 Unmeasured

Whether the bundler places `rustbox-cef.exe` beside `rustbox.exe` in the produced
NSIS installer - the staging and `--config` plumbing are written and the artifact
has been inspected (three executables at the archive root, 66.5 MB uncompressed),
but the *installed* result has not been verified from a clean machine. Whether an
upgrade install replaces a stale `rustbox-cef.exe` rather than leaving the old one.
macOS and Linux, which have no published payload and so ship the launcher only.

What *is* measured: the startup repair path, end to end, against a sandboxed data
root (`RB_DATA_ROOT`) holding a payload-only engine directory and a stand-in
`rustbox-cef.exe` beside the launcher. Launching the debug build staged the binary
into `engines\cef\154.0.34\` with the sibling's content and no download - the exact
transition that was previously impossible. The user's real engine directory was
left untouched by that test.

## 27. Disk, after the engine switch: what it was, and the two changes that came out of it (2026-11-23)

The report was "disk C: is busy, I am worried about the SSD". Measured instead of
guessed, and the shape of the answer is worth recording because most of it was
*not* something to fix in code.

Attribute, over a 240-300 s window, by diffing file fingerprints under the data
root while sampling `\PhysicalDisk(1 c:)\Disk Write Bytes/sec` in parallel:

| State | Disk C: writes | RustBox share |
|---|---|---|
| WhatsApp visible | 4.7-10.9 MB/s (peak 27) | 66 % |
| WhatsApp navigated to `about:blank` | 0.98 MB/s | - |
| Reloaded | 6.2 MB/s | - |

The decisive test is the first two rows: unloading *one* app, without touching
anything else, divides total disk writes by ten. Edge (96 processes, 12 tabs)
was the other large writer at ~5.7 MB/s, so RustBox was not the only cause.

The storm is WhatsApp's IndexedDB under CEF, not the engine generally:

| | WebView2 | CEF |
|---|---|---|
| `https_web.whatsapp.com_0.indexeddb.leveldb` | 94.6 MB, 53 `.ldb` | 203 MB, 98-129 `.ldb` |
| `MANIFEST` | 940 KB | 17.8 MB |
| `Compacting` lines in `LOG` | 132 | 1,204 |
| `Generated table` lines | 483 | 7,446 |

Two other suspects were ruled out by measurement. Cookie-jar flushes are 18
events / 120 s across 22 profiles (~24 KB/s at 160 KB each). The 3,322 files with
an mtime in **2097** are not rewrites: Chromium stamps extension assets with a
far-future mtime as an anti-tamper marker, so a naive "files with mtime newer than
T" scan counts them as written on every pass. Excluding them is what made the
attribution in the table above trustworthy.

SSD: C: is a TEAM TM8FPK001T (1 TB, Phison E21, Micron 176L TLC, 600 TBW).
`Wear = 0`, `HealthStatus = Healthy`, 50-53 °C against a 90 °C maximum. At the
measured 3 MB/s sustained (~250 GiB/day) that is years, not months; at 10 MB/s
24/7 (~860 GiB/day) it falls to roughly two years. Worth stopping, not an
emergency.

### 27.1 D - external extensions were installing themselves into every profile

Every CEF profile contained the same seven Web Store extensions, 22 copies each,
2.97 GB total. All seven carried `disable_reasons = 8192`
(`DISABLE_EXTERNAL_EXTENSION`), i.e. already inert. Their `location` split 6 / 1
between `EXTERNAL_PREF_DOWNLOAD` and `EXTERNAL_REGISTRY`, which looked like an
`external_extensions.json`; there is none. The source is the Windows registry that
Chromium reads before the profile exists:

- `HKLM\SOFTWARE\WOW6432Node\Google\Chrome\Extensions` - four keys with only
  `update_url` (Avira Password Manager, Avira Safe Shopping, Adobe Acrobat, one
  unknown).
- `HKCU\SOFTWARE\Google\Chrome\Extensions` - three keys, one of which carries a
  `path` to `ChromeExtensionCache\autoclaw-chrome-0.1.6.crx`.

The registry/`location` correlation is exact: the six `update_url`-only keys are
the six `EXTERNAL_PREF_DOWNLOAD` entries, and the one key with a `path` is the
`EXTERNAL_REGISTRY` entry. `--disable-external-extensions` does not exist in this
CEF build (`libcef.dll` carries `disable-extensions-except` and
`allowlisted-extension-id`, and the latter only *adds* permission allowlist
entries - it cannot block anything), so the only lever is the source.

Applied: both key sets exported to
`%LOCALAPPDATA%\rustbox\backup-ext-registry-<stamp>\*.reg`, the seven keys
removed, and the 22x7 directories deleted with the app closed. `%LOCALAPPDATA%\rustbox\cef`
went from 4.10 GB to 1.17 GB. Re-launched and re-checked: zero extension
directories came back. The removal covers Chrome as well, but every one of the
seven was already `disable_reasons = 8192` in the user's Chrome profiles, so
nothing functional was lost; the `.reg` backup makes it reversible.

### 27.2 E - a ceiling on the HTTP disk cache

Chromium has no upper bound on its HTTP disk cache; it grows to ~320 MiB per cache
backend and stays there, and RustBox gives every app its own backend. `engine.rs`
now appends `--disk-cache-size=134217728` (128 MiB) for the CEF build only. It is
deliberately not in `webview_args::BASE`: that constant is mirrored into
`tauri.conf.json` and pinned by a test, and WebView2 already caps its own budget.

128 MiB is a ceiling, not a target - measured profiles average ~14 MB and peak
near 98 MB, so a healthy profile is untouched. Two tests pin it: the switch must
not appear in `BASE`, and the value must be below Chromium's default (or it would
never apply) and above 64 MiB (or eviction would write more, not less).
`--media-cache-size` is *not* used: `findstr` shows it absent from this
`libcef.dll`.

### 27.3 What D and E do not do

Neither stops the WhatsApp storm. D reclaimed disk that was already inert; E bounds
a cache that is not the churning store. The storm is a site IndexedDB, and it only
stops by giving that site a fresh database (re-link WhatsApp) or moving the data
root to another disk.

### 27.4 Unmeasured

Whether CEF applies `--disk-cache-size` per request context rather than only to the
default backend. The switch is browser-process-only, so it does not appear in any
child `CommandLine`, and `Browser.getBrowserCommandLine` refuses without
`--enable-automation`; the value is therefore verified in code and by the two
tests, not read back from the running process. Whether the seven registry keys
reappear when Avira/Adobe/NordVPN installers run again. Whether removing them
removes the extensions from Chrome profiles or merely stops future provisioning.

One thing that is *not* a regression from D or E, checked because three Gmail apps
came back on `accounts.google.com/v3/signin/accountchooser`: their Google cookies
are present, decrypt to the same 153-byte values as the WebView2 jars, and expire
in 371 days. Google reports the accounts as "Signed out", which is a server-side
invalidation, the same class as 25.7. Removing extension directories and registry
keys cannot touch a cookie jar.
