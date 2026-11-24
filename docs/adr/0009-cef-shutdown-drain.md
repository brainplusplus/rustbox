# ADR 0009: Vendored `tauri-runtime-cef` patch — drain CEF before shutdown

**Status:** Accepted, implemented, verified on Windows x64 (CEF release build).
**Date:** 2026-11-24.
**Relates to:** ADR 0005 (CEF as embedded backend), ADR 0008 (data-folder move —
the feature whose acceptance run kept being interrupted by this crash).

## 1. Problem

Every CEF quit crashed: Windows recorded an `APPCRASH` for `rustbox-cef.exe` with

```
Faulting module: libcef.dll  version 154.0.34.0
Exception code: 0xc0000005
Fault offset:   0x412eb0
```

The headline symptom was data loss. A quit at 17:37 crashed, LevelDB for the
WhatsApp app profile was left torn, and the next launch at 18:15 logged

```
Got corruption for { origin: https://web.whatsapp.com }
IndexedDB recovering from a corrupted (and deleted) database
```

which dropped the device pairing and demanded a QR scan again. The data-folder
move feature was suspected first and cleared: the crash predates it, reproduces
with the feature's code paths never entered, and does not touch profile teardown.

### 1.1 What was ruled out

| Hypothesis | Evidence against |
| --- | --- |
| Random / flaky | The crash is reproducible on demand through the normal close path. |
| Skew between `cef` and the shipped DLL | `cef`/`cef-dll-sys` are both `154.5.0+154.0.34`; `cef\Last Version` is `154.0.8037.98`; the deployed exe is byte-identical to the build whose PDB we had. |
| Our own Rust code | The faulting frame is a trivial getter inside `libcef` (`48 8b 41 18` / `c3`), i.e. a method called on an already-freed object. |
| A specific app | Blank fixtures crash, and a real profile alone does not. |
| Too many browsers | 23 blank apps shut down cleanly — twice. |
| Hangs while idle | 180 s idle with 23 real apps: zero crashes. |

### 1.2 The actual cause

The crash needs **two conditions at once**: real app profiles *and* enough of
them. On a copy of the real data root, 4 apps quit cleanly and 6 crashed, but
the same 5-app config crashed only ~3 runs out of 8 — so it is a race, not a
count threshold.

Instrumenting the runtime pinned the stage. With the crash:

```
exit_if_done: exiting the loop (windows=0 closing_windows=0 live_browsers=0)
app dropped:  ... live_browsers=0
loop exited
<<< never returns: crash inside cef::shutdown() >>>
```

Every browser was already closed, so the upstream "browsers still open during
shutdown" story (115jon/yap#138) is *not* the mechanism here. With an empty data
root the same build printed `cef::shutdown returned` and exited cleanly. So the
crash is inside `CefShutdown`, and it involves profiles.

The reason is the external message pump. `tauri-runtime-cef` runs
`Settings::external_message_pump = true`: CEF does not own the loop, it asks the
host to call `cef::do_message_loop_work()` through `OnScheduleMessagePumpWork`.
The host delivers that via a `WM_TIMER` / private `WM_USER` message on a
message-only window, i.e. through winit's own `GetMessage`/`DispatchMessage`
loop. Destroying a browser or a request context is *posted* to CEF's UI thread,
and the only thing that ever runs posted UI-thread tasks is that pump.

`run()` exited winit's loop the moment the last window closed, then called
`cef::shutdown()` immediately. Teardown work for the per-app request contexts and
their browsers was still queued with nothing left to pump it, so `CefShutdown`
walked into profiles that still owned live browsers and Chromium's
`ProfileDestroyer` read through a freed pointer. Fewer profiles means less queued
work, which is exactly why the crash needed several real apps to appear — and why
it looked random.

## 2. Decision

Vendor `tauri-runtime-cef` the same way `tauri-runtime-wry` is already vendored,
and drain CEF's queued UI-thread work after winit's loop returns, before
`cef::shutdown()`.

```toml
[patch.crates-io]
tauri-runtime-wry = { path = "../vendor/tauri-runtime-wry" }
tauri-runtime-cef = { path = "../vendor/tauri-runtime-cef" }
```

The vendor directory is the published `3.0.0-alpha.7` source with three touched
files and no other change, so a future upgrade is a re-copy plus re-applying the
drain. Its `Cargo.lock` is the crate's own (regenerated, since the published one
pinned crates that are no longer cached); only the crate's standalone
`cargo test` uses it — the app build resolves through `src-tauri/Cargo.lock`, where
the patch simply drops the registry source and checksum for this package.

### 2.1 The drain

`CefExternalPump::drain_before_shutdown()` pumps `cef::do_message_loop_work()`
until CEF has asked for **no immediate work** for `SHUTDOWN_QUIET` (150 ms), or
until `SHUTDOWN_BUDGET` (5 s) runs out. `DrainOutcome` reports which happened; a
timeout logs a warning and shuts down anyway, because a quit that hangs forever is
worse than one that may crash.

Two details make the "is CEF idle" test honest:

- **Immediate requests only.** The pump re-arms itself with
  `K_TIMER_DELAY_PLACEHOLDER` after every pass; that means "nothing to do, wake me
  later", not "work is pending". Counting every scheduling request would never
  observe idle. `PumpState::immediate_requests` therefore counts only
  `delay_ms <= 0` requests.
- **A quiet window, not one idle tick.** Teardown chains re-post themselves, so a
  single empty pass is not evidence the queue is empty. The tracker requires 150 ms
  of no new immediate requests, and any new request restarts that window.

The drain uses `PumpState::pump_once()`, not `do_work()`. `do_work()` also re-arms
the platform timer, which posts messages to a window whose loop is gone; only the
CEF side of the pump is wanted at this point. `run()` holds its own clone of the
pump across the drain so the platform window outlives the loop.

The timing rule lives in `DrainTracker`, a plain struct with no CEF dependency, so
it is unit-tested (`external_message_pump::drain_tests`, 4 tests: stops when
quiet, keeps going while CEF asks for work, a late request restarts the window,
the budget always wins).

### 2.2 What was *not* done

- **No `process::exit` shortcut.** Exit codes and the existing `RunEvent::Exit`
  handling stay intact.
- **No `cef::shutdown()` skip.** It still runs, unconditionally, after the drain.
  Skipping it would move the leak, not fix the teardown.
- **No change to browser closing.** `close_all_browsers`, `exit_if_done` and
  `live_browsers` are untouched; they already reached zero before the crash.

## 3. Verification

Unit level (vendored crate, `cargo test --offline`: 97 passed) and clippy clean.

End to end, quitting through the real path (`CloseMainWindow` → winit close →
`RunEvent::Exit` → drain → `cef::shutdown`), against a copy of the real 8.6 GB data
root (23 apps) on the **release** build:

| Fixture | Runs | Clean quits | New `APPCRASH` |
| --- | --- | --- | --- |
| 5 real apps, pre-patch | 8 | 5 | 3 |
| 5 real apps, post-patch (debug) | 16 | 16 | 0 |
| 12 real apps, post-patch (debug) | 5 | 5 | 0 |
| 23 real apps, post-patch (debug) | 3 | 3 | 0 |
| 23 real apps, post-patch (release) | 16 | 16 | 0 |

Every clean quit finished in ~1 s with zero leftover `rustbox-cef.exe` processes;
crashed quits took 7–8 s because Windows Error Reporting holds the process while
it writes a dump. The website is unchanged: `cargo check` (default WebView2 build)
and the app suite (313 + 5 tests) still pass.

The instrumented trace also confirmed the mechanism directly: after the drain,
`cef::shutdown` returns instead of faulting, on the same fixture and config that
crash 3-in-8 without it.

## 4. Consequences

- **Quit now takes up to 150 ms longer** on the normal path (the quiet window),
  and up to 5 s in the pathological case where a renderer never settles — after
  which the old behaviour resumes.
- **The patch must be re-applied on every `tauri-runtime-cef` upgrade.** The
  touched surface is small and self-contained: `src/external_message_pump/mod.rs`
  (drain, tracker, counter, tests) and `src/runtime.rs` (`run()` call site plus one
  import).
- **Upstream is still affected.** `tauri-runtime-cef` `3.0.0-alpha.7` is the
  current release and has no fix; the same shutdown order applies to any host that
  uses `external_message_pump` and exits its loop before calling `cef::shutdown()`.
  This is worth reporting upstream with the trace above.
- **Symptom-level confirmation is still pending.** The fix removes the crash that
  tore WhatsApp's LevelDB. It should be re-checked by leaving WhatsApp paired
  across several quits, which is the real acceptance test for the data-loss story.

## 5. Rejected alternatives

- **A — `process::exit(0)` right after the exit callback.** Avoids the fault by
  never running CEF teardown. Leaks the profile lock and leaves dirty LevelDB
  state, which is the failure being fixed.
- **C — accept the crash.** It corrupts app data on every few quits; that is the
  original bug report.
- **Wait for upstream.** No fix exists in the current release, and the crash
  destroys session data in the meantime.
