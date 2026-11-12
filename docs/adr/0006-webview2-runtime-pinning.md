# ADR 0006: Pin the WebView2 runtime so tabs survive a Windows update

**Status:** Accepted (2026-11-12)

## Context

Reported symptom: after a while, opening a tab fails with

```
could not open the tab: Windows updated the WebView2 runtime to 157.0.4341.46
while RustBox was running on 157.0.4341.42. A running app cannot move to the
new runtime, so new tabs will fail until RustBox restarts.
```

Three facts were measured on the reporting machine (2026-11-12) rather than
assumed:

| Fact | Value |
| --- | --- |
| `RustBox.exe` start | 8 Nov 09:06 |
| Its browser process | `157.0.4341.42` |
| Updater wrote | `157.0.4341.46`, 12 Nov 03:29 |
| Old folder on disk | still present, ~920 MB |

So the old runtime had **not** been deleted. The failure is not "the binary
went away"; it is "the loader picked a different one".

`wry` 0.57 calls

```
CreateCoreWebView2EnvironmentWithOptions(null, <data dir>, …)
```

with a `null` browser executable folder (`wry-0.57.0/src/webview2/mod.rs:349`).
With no explicit folder, the WebView2 loader resolves the **newest** runtime
folder on every call. Every RustBox app shares one user-data folder
(`%LOCALAPPDATA%\rustbox\EBWebView`, with a named profile inside it), and that
folder already has a live browser process on `.42`. The new environment therefore
asks the live process for a version it is not running, and creation fails with
`HRESULT(0x80010108)` / `RPC_E_DISCONNECTED`.

Measured, not inferred (2026-11-12, on the affected machine, using a throwaway
user-data folder and the environments kept alive exactly as RustBox keeps them):

| Step | Pin | Result |
| --- | --- | --- |
| create environment, then controller | old `.42` | OK — runs `.42` |
| create environment, then controller | none | environment OK, **controller fails `0x80010108`** |
| create environment, then controller | old `.42` | OK — runs `.42` |

So the failure is in **controller** creation, not environment creation: the
environment for `.46` creates fine, and only fails when it reaches for the live
browser process. The controller step is the one that has to stay on the running
runtime, which is exactly what the pin controls. Repeating the sequence gave the
failure in 4 of 5 runs; the pass is the known race where the old browser process
exits before the mismatched request.

The previous mitigation (`src-tauri/src/webview2_skew.rs` before this ADR) hid the
request and offered a restart. That treated an informational event as a blocker:
the restart is the only way to *use* the new runtime, not a precondition for
opening a tab.

## Decision

**Pin the runtime at startup, and offer the update instead of demanding a restart.**

1. `webview2_skew::pin_runtime_to_startup_version()` runs from `main` before the
   Tauri builder creates the first webview. It resolves the newest installed
   runtime folder and exports `WEBVIEW2_BROWSER_EXECUTABLE_FOLDER` — the
   documented loader override, i.e. WebView2's "fixed version" mode. Every
   environment created afterwards, including the shell window, resolves the same
   folder, so a mid-session update cannot affect tab creation.

2. Detection is kept, but its role shrinks to a notice. `current_skew()` still
   compares folder timestamps against process start; the result now drives one
   per-session, informational strip with a **Restart now** action and no blocking
   behaviour. The tab that triggers the notice opens normally.

3. The pin is validated on every tab open. If the pinned folder disappeared, the
   pin is dropped and the loader picks the installed runtime again, so a deleted
   version cannot make every tab fail for the rest of the session.

### Why this mechanism and not another

**Not a per-folder environment object.** Building environments explicitly and
passing them via `WebviewWindowBuilder::with_environment` would also work, but
RustBox has already been bitten by two environments disagreeing: WebView2 rejects
any webview whose environment options differ from those in use for the same
user-data folder (`HRESULT 0x8007139F`, see `src-tauri/Cargo.toml` and
`src/webview_args.rs`). Anything that constructs a second environment has to keep
language, scroll-bar style, extension settings and browser args byte-identical to
the shell's. The environment variable applies at creation time for every
environment, so there is nothing to keep in sync.

**Not reusing the shell's environment.** It is the smallest change, but it only
covers tabs that share the shell's folder. `ProfileMode::Isolated` apps use their
own folder and would need their own environment anyway, so the pin is both
smaller and uniform.

### Administrator policy is honoured

Before pinning, the registry is checked for
`Software\Policies\Microsoft\Edge\WebView2\BrowserExecutableFolder` (HKLM and
HKCU). If any app-id value is non-empty, the pin is skipped: the policy is a
deliberate deployment decision, the loader applies it regardless, and overriding
it would leave IT believing the app runs a runtime it does not.

### Why an update still needs a restart

WebView2 runs one browser process per user-data folder. Moving that process to a
different runtime means ending it, and RustBox's own UI is a webview in the same
folder. There is no narrower restart to offer, which is why the notice says
"restart RustBox" rather than "restart this tab". What changed is that the restart
is now optional and can be postponed indefinitely.

## Consequences

- Tabs keep opening across a runtime update. The user is told once per session
  that a newer runtime is waiting.
- A restart is the only way to adopt the new runtime, and it costs extra tabs
  inside an app (they start over); logins persist because sessions are on disk.
- The process environment is process-global and inherited by children. This is
  intended: a helper process must resolve the same runtime. It is also why the pin
  must be set before any webview exists, which the call site in `main` enforces.
- If a future Windows build ignores `browserExecutableFolder`, tabs fail again and
  `RPC_E_DISCONNECTED` returns. The detection and message survive that outcome,
  and the fallback would be the environment-object approach rejected above.
- The pin is checked before every build (`pinned_folder_missing`), so a runtime
  deleted mid-session downgrades to "let the loader choose" instead of failing
  every tab.
- ADR 0005's claim that CEF escapes this failure class still holds; this ADR makes
  escaping it less urgent rather than obsolete.

## Verification

- `cargo test --bins` in `src-tauri`: 214 pass, covering pin selection (newest by
  default, explicit version honoured, uninstalled version ignored, numeric rather
  than lexicographic ordering), the notice decision, and two tests that drive the
  real loader on Windows:
  - `the_pin_changes_what_the_loader_resolves` asks WebView2 which version it
    resolves with the variable set, and requires it to differ from the newest;
  - `the_startup_pin_wires_the_folder_and_remembers_it` runs the entry point
    `main` calls and requires the variable and the remembered pin to agree.
  Both fail if the pin is disabled, so neither is a no-op check.
- Manual, on the machine that reported the bug, with a throwaway user-data folder:
  the three-step sequence in the table above reproduces `0x80010108` unpinned and
  succeeds pinned, for both `.42` and `156`.
- Stale-pin fallback: pinning to a version that is not installed fails with
  `ERROR_FILE_NOT_FOUND`, and clearing the pin then succeeds on the installed
  runtime — which is the recovery `pinned_folder_missing` triggers before a build.
- `node scripts/check-ui.js` parses the edited inline script.
