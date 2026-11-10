# CEF spike (milestone M0)

Answers one question before any RustBox code depends on the answer:

> Can CEF open a window and render a page on this machine, and does its message
> loop work when the host drives it instead of handing CEF the thread?

Context: [`docs/adr/0005-embedded-chromium-cef.md`](../../docs/adr/0005-embedded-chromium-cef.md).

This crate is not part of the app. It pulls a CEF distribution of about 135 MB
(`minimal`) and is only ever built by hand.

## macOS

CEF on macOS must live inside an `.app` bundle with the framework in
`Contents/Frameworks/`. Running the bare binary is what CEF's own documentation
warns against, and it fails in ways that look like CEF being broken.

```sh
cargo build --release

# Lay out the bundle CEF expects.
APP=build/RustBoxCefSpike.app
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Frameworks"
cp target/release/cef-spike "$APP/Contents/MacOS/"
cp -R target/release/*.framework "$APP/Contents/Frameworks/" 2>/dev/null || true
cp -R target/release/Chromium\ Embedded\ Framework.framework "$APP/Contents/Frameworks/" 2>/dev/null || true

# Helper apps: CEF resolves its sub-processes through them.
cp -R target/release/*\ Helper*.app "$APP/Contents/Frameworks/" 2>/dev/null || true

"$APP/Contents/MacOS/cef-spike"
```

`run-macos.sh` does exactly this and then runs it; prefer it over copying the
commands by hand.

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

- **`pumps` keeps growing and a window shows the page** — the windowed design in
  the ADR is viable. This is the outcome that lets M2 proceed.
- **`pumps` stops growing** — the host loop blocked. That is the failure the
  spike exists to find; it means the off-screen design (Option B) or a separate
  helper process, and it changes the plan before any code depends on it.
- **`browser created: false`** — CEF refused to start. The INFO log says why;
  usually a missing bundle or framework path on macOS.
- **Heartbeat grows, window stays blank** — the loop is fine and rendering is
  not. Look for `[page]` console output to tell "page loaded but not painted"
  from "page never loaded".

The spike exits by itself after 60 s so it cannot be left running.

## Comparing the two loop modes

```sh
cargo run --release                      # host pumps (what RustBox needs)
cargo run --release -- --multi-threaded  # CEF owns a thread (the fallback)
```

If the first works, RustBox keeps one loop and no second thread owns the UI. If
only the second works, the plan changes and the ADR records why.

## Record the result

Fill this in when it is run, and copy it into the ADR:

| Question | Mac answer | Windows answer |
| --- | --- | --- |
| Host-pumped loop works? | | |
| `--multi-threaded` works? | | |
| Window renders a page? | | |
| `minimal` distribution enough? | | |
| Startup to first paint (`t=` line) | | |
| CPU while idle | | |
