# RustBox

A multi-app desktop workspace: every service you use daily gets its own tab,
its own storage, its own proxy, and it all lives in one window instead of forty
browser tabs.

Tauri shell in Rust, UI as plain HTML/CSS/JS (`ui/`, no bundler). Electron-shaped
problem, no Electron.

![platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-informational)
[![release](https://img.shields.io/github/v/release/brainplusplus/rustbox)](https://github.com/brainplusplus/rustbox/releases)

## What it does

- **App tabs** - each app is a sidebar entry with its own cookie jar, localStorage
  and IndexedDB (`profile_mode: isolated`), so two Gmail accounts are two apps.
- **Sub-tabs per app** - several pages inside one app without leaving it, with the
  open set remembered across restarts (ADR 0007).
- **Three ways to open a page** - the OS webview, an external browser (with the
  right Chromium proxy flags), or the app's own Chromium build. Chosen per app.
- **Per-app proxy** - applied at the native back-end so it covers the whole
  profile, not just the first request. Socks5 included.
- **Catalog** - ready-made presets for common services, plus add-by-URL with
  favicon discovery.
- **Your own data folder** - the workspace can live on any drive. Moving it runs
  in a window you can watch, cancel and retry, and only a verified copy ever
  replaces the old folder (ADR 0008).
- **AI router + chat panel** - send a prompt against several providers, record
  latency and token stats, keep the loop out of the UI thread.
- **Agent access over MCP** - the app exposes a local MCP server so a coding agent
  can drive tabs, read console/network and take screenshots. See
  [`docs/mcp.md`](docs/mcp.md).

## Two engines, two executables

The default engine is the OS webview: WebView2 on Windows, WKWebView on macOS,
WebKitGTK on Linux. Nothing extra to install, smallest binary.

The Chromium build is the opt-in second engine, and it is a **separate
executable** rather than a runtime switch for a hard reason: `libcef.dll` sits in
the PE import table, so the loader resolves it before `main()` runs. One binary
cannot fall back from Chromium to WebView2 - without the payload beside it, it
does not start at all. So the installer ships both, the launcher reads the
configured engine and hands off to `rustbox-cef.exe` when it is asked for. One
shortcut, one data folder, two binaries (ADR 0005 §26).

The Chromium build is Windows-only today, because the payload is a `windows64`
CEF distribution.

## Requirements

| | |
|---|---|
| Rust | 2024 edition toolchain (`rustup` stable) |
| Node | 20+, only for the installer wrapper and the UI gates |
| Windows | WebView2 runtime (present on Windows 11); C++ toolset only if building the Chromium variant |
| macOS / Linux | system WebKit packages as usual for Tauri |

## Build and run

Default (OS webview) engine:

```powershell
cd src-tauri
cargo run --release
```

Chromium engine (Windows):

```powershell
# CEF_PATH must point at an extracted CEF 154.0.34 distribution.
$env:CEF_PATH = "D:\cef\cef_binary_154.0.34+chromium-154.0.8037.98_windows64_minimal"
cd src-tauri
cargo build --release --no-default-features --features cef --bin rustbox-cef
```

The two features are mutually exclusive by construction: they pull different
`tauri-runtime-*` crates and two GTK majors into one graph, so a build picks one.

Installer (ships the app, the MCP shim and the Chromium binary):

```powershell
npm run build:installer
```

Run that from a "Developer PowerShell for VS" on Windows - the Chromium build
compiles C++ through CMake, which reads the toolchain paths from the environment.
Two things are checked with an explicit message before anything is built:
`CEF_PATH`, and a non-empty `INCLUDE`. Details in [`docs/mcp.md`](docs/mcp.md).

## Tests and gates

```powershell
cd src-tauri; cargo test --release          # Rust: 313 unit tests + 5 for the MCP shim
node scripts/check-ui.js                    # UI wiring, without a browser
powershell -File scripts/probe-all.ps1      # real UI, headless Chrome
```

`check-ui.js` is a build gate, not a style check: it compiles the inline script
without running it, and asserts the wiring that a screenshot cannot catch (every
settings tab has a pane, every pane has a builder, the lists reorder from pointer
events, and so on).

Two probes need a real mouse and a running app on Windows:

- `scripts/probe-drag-real.ps1` - what a real cursor drag does to the page. This
  is what proved the Chromium engine never delivers `drop` (ADR 0005 §28.2).
- `scripts/probe-pointer-real.ps1` - drives the shipping reorder function with a
  real cursor and checks the resulting order.

Both move the cursor and raise the window for a moment; they never send
keystrokes, and they refuse to press unless `WindowFromPoint` confirms the pixel
belongs to RustBox.

## Repository layout

```
src-tauri/src/       the app: engine, tabs, workspace, proxy, chat, MCP, CEF bridge
ui/                  the whole front end; no bundler, no build step
scripts/             installer wrapper, UI gates, probes
docs/adr/            the decisions, with the measurements behind them
docs/mcp.md          the agent bridge and its client setup
vendor/              one patched crate, with the patch explained in Cargo.toml
tools/cef-spike/     the spike that answered "can this ship CEF at all"
```

## Documentation

The ADRs are the design record, and they carry the numbers rather than
adjectives:

| | |
|---|---|
| [0001](docs/adr/0001-engine-selection-architecture.md) | shell and engine architecture |
| [0002](docs/adr/0002-session-isolation-and-proxy.md) | session isolation and proxy |
| [0003](docs/adr/0003-catalog-installer-and-verification.md) | catalog, installer, verification |
| [0004](docs/adr/0004-mcp-bridge.md) | the MCP bridge |
| [0005](docs/adr/0005-embedded-chromium-cef.md) | the Chromium engine, start to finish |
| [0006](docs/adr/0006-webview2-runtime-pinning.md) | pinning the WebView2 runtime |
| [0007](docs/adr/0007-remember-open-sub-tabs.md) | remembering open sub-tabs |
| [0008](docs/adr/0008-data-folder-move-window.md) | moving the data folder, with a window that shows it |

## Status

`0.1.2`. Windows is the platform the Chromium engine ships on; macOS and Linux
build with the OS webview. The known limits are written down where they were
found - notably what the Chromium engine does *not* do (ADR 0005 §28.3) and
what was never measured (§28.4).
