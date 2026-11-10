# CEF spike (milestone M0)

Answers one question before any RustBox code depends on the answer:

> Can CEF open a window and render a page on this machine, and does its message
> loop work when the host drives it instead of handing CEF the thread?

Context: [`docs/adr/0005-embedded-chromium-cef.md`](../../docs/adr/0005-embedded-chromium-cef.md).

This crate is not part of the app. It pulls a CEF distribution of about 135 MB
(`minimal`) and is only ever built by hand.

## macOS

CEF on macOS must live inside a signed `.app` bundle whose `Contents/Frameworks/`
holds the framework **and five helper bundles** (`Helper`, `Helper (GPU)`,
`Helper (Renderer)`, `Helper (Plugin)`, `Helper (Alerts)`). Running the bare
binary fails in ways that look like CEF being broken, and a missing helper makes a
page load hang with no error at all.

Two details from the `cef` crate's own bundler (`src/build_util/mac.rs`) that are
easy to get wrong:

- on macOS the crate does **not** copy the CEF runtime beside the binary (it does
  on Windows and Linux), so the framework comes from the build script's output
  directory, not `target/release`;
- a helper resolves the framework through a different relative path than the main
  binary, which is why `main.rs` detects the helper case from `--type=`.

`run-macos.sh` does the whole job: build, locate the distribution, assemble the
bundle, sign it inside out with an ad-hoc identity, verify the signature, clear
any quarantine flag, run.

```sh
./run-macos.sh                  # host-pumped loop (what RustBox needs)
./run-macos.sh --multi-threaded # for comparison only; not supported on macOS
```

`--multi-threaded` is kept so the same command exists on every platform, but CEF's
own header says `multi_threaded_message_loop` is *"only supported on Windows and
Linux"*. On macOS the flag is **refused** (exit 2) rather than attempted, because
attempting it produces a process that never paints — which reads exactly like
"the host-pumped loop failed too". That matters for what M0 can conclude: a stalled
host loop on macOS has no same-thread fallback left.

**No Apple Developer ID is needed** for this. An ad-hoc signature (`codesign -s -`)
is enough to run locally; the limits of that for distribution are in
[`SIGNING-WITHOUT-DEVELOPER-ID.md`](SIGNING-WITHOUT-DEVELOPER-ID.md).

## Windows

```powershell
cargo run --release
```

The CEF runtime files (DLLs, `.pak`, `locales`) are copied next to the executable
by the crate's build script, so a release build runs from `target/release`
without extra steps. Debug builds do the same into `target/debug`.

## What to look for

The spike prints a heartbeat every 500 ms:

```
[spike] host pumps CEF; timer thread above does it
[spike] browser created: true
[spike] t= 500ms pumps=98 browser=true
[spike] t=1000ms pumps=197 browser=true
```

The heartbeat lines above are from the real Windows run.

- **`pumps` keeps growing and a window shows the page** — the windowed design in
  the ADR is viable. This is the outcome that lets M2 proceed.
- **`pumps` stops growing** — the host loop blocked. That is the failure the
  spike exists to find. On Windows the fallback is `--multi-threaded`; on macOS
  there is no such fallback (CEF documents `multi_threaded_message_loop` as
  Windows/Linux only), so a stalled loop there means off-screen rendering.
- **`create_browser returned 0`** — CEF refused to start. The severity-INFO log
  says why; usually a missing bundle or framework path on macOS.
- **Heartbeat grows, window stays blank** — the loop is fine and rendering is
  not. Look for `[page] title=…` to tell "page loaded but not painted" from "page
  never loaded".

The spike stops itself after 60 s so it cannot be left running.

## Windows result (measured 2026-11-10)

| Question | Result |
| --- | --- |
| Host-pumped loop works? | Yes, 120 samples, monotonic 24 → 10538, no stall |
| Page loaded? | Yes: `create_browser returned 1`, `[page] title=Example Domain` |
| Shutdown | Clean: `browser closing` then `clean shutdown`, exit 0 |
| Signed? | Not needed on Windows |
| Size | `target/release` 371 MB, `libcef.dll` 281 MB |
| GPU | Process exited and fell back to software rendering (headless session) |

## Record the Mac result

Fill this in when it runs, and copy it into the ADR:

| Question | Mac answer |
| --- | --- |
| Host-pumped loop works? | |
| Window renders a page? | |
| `pumps` value when the page title appeared | |
| Ad-hoc signature verified? (`== 5/6` step) | |
| Startup to first paint (`t=` line) | |
| CPU while idle | |

`--multi-threaded` is not in that table on purpose: on macOS the spike exits 2
instead of running it, because CEF does not support that loop there. There is
nothing to record beyond "refused", which is what the ADR now says. |
