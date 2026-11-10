# ADR 0003: App Catalog, Installer Layout, and Verification Harness

**Status:** Accepted (2026-10-22)

## Catalog

The app catalog is generated from the Ferdium recipe set
(https://github.com/ferdium/ferdium-recipes) by `scripts/build-catalog.ps1`, and
covers all 441 recipes. Three shapes exist, and each is handled explicitly rather
than being dropped:

| Shape | Count | Handling |
| --- | --- | --- |
| Plain | 369 | `url` is used as-is. |
| Templated | 43 | `url` contains placeholders (`{teamId}`, `{subdomain}`, ...). Emitted as a template plus a `ph` list of `{n, k}` label/key pairs. |
| Self-hosted | 29 | Recipe declares `hasCustomUrl` and carries no service URL. Emitted with an empty URL and `cu: true`; the user supplies their own instance address. |

Placeholder labels are derived from the placeholder's position in the URL, with
per-recipe overrides where the position is ambiguous (twitch, planitpoker,
zenhub, facebookpages, whereby, clubhouse, azure-devops). Labelling them "Value 1,
Value 2" would be worse than useless.

Oversized icons (37 recipes, mostly SVG-wrapped base64 rasters, one 4.2 MB) are
rasterised to 160px PNG by `scripts/rasterize-icons.mjs` rather than shipped.

### Icon normalisation

`scripts/normalize-icons.mjs` measures each icon's real ink in headless Chromium,
then rewrites the root `viewBox` so the drawing fills 78% of a square canvas. It
runs after rasterisation because `build-catalog.ps1` overwrites `ui/icons`.

Two dark-mark problems are handled differently, because they need opposite fixes:

- **Thin dark glyphs (30 icons)** are inverted with an SVG `feComponentTransfer`
  filter. A filter acts on rendered pixels, so hollow strokes stay hollow and
  multi-tone marks keep their tonal steps, which a `fill` override would destroy.
- **Solid dark plates (9 icons)** cannot be inverted without turning the whole
  tile into a glaring white square. They get a light rounded pad and are scaled
  to 84% on top of it.

The pad group must close *inside* the root element. Appending it after `</svg>`
produces malformed XML that renders as nothing at all — the browser shows a
broken-image placeholder, which is how this was caught.

## Installer

`bundle.windows.nsis.installMode` is `perMachine`. This is not cosmetic: with the
default per-user mode the installer targets `%LOCALAPPDATA%\<ProductName>`, and on
Windows that path is the *same directory* as the data root `%LOCALAPPDATA%\rustbox`
(directory names are case-insensitive). The result was `rustbox.exe` and
`uninstall.exe` sitting next to `config.json` and `profiles\`, and the uninstaller
deleting the user's entire workspace along with the binaries. Verified by running
it: the directory disappeared.

With `perMachine` the binaries go to `C:\Program Files\RustBox` and uninstall
leaves `%LOCALAPPDATA%\rustbox` untouched.

## Verification harness

Three scripts, each proving a different layer. All three are meant to be run, not
trusted:

- `scripts/probe.ps1` + `scripts/probe-catalog.js` — drives the real UI in
  headless Chrome. Copies `ui/index.html` next to itself, injects a stub for the
  Tauri globals and the probe script, then asserts on what the page actually
  does (catalog size, setup tags, template substitution, refusal to save an
  incomplete template, proxy payload, Light-mode warning). 16 checks.
- `scripts/probe-proxy.ps1` — proves the per-app proxy reaches WebView2. Points
  an app at `rb-probe.invalid` (a reserved name that cannot resolve) and logs
  what arrives at a local proxy. Includes a negative control with no proxy
  configured. 3 checks.
- `scripts/shots.ps1` + `scripts/shot-states.js` — captures each UI state to
  `shots/` for visual review.

Both page-injecting scripts read `index.html` with an explicit
`[System.Text.Encoding]::UTF8`. Windows PowerShell's `Get-Content` falls back to
the ANSI codepage for BOM-less files, which turns every em dash in the UI into
mojibake; the screenshots caught it.

## Backend tests

`cargo test --manifest-path src-tauri/Cargo.toml` — 21 passed, 1 ignored.

## Toolbar

One bar, two identities, driven by state in `updateToolbar()` (ui/index.html).
Before, it rendered as a browser toolbar and a launcher toolbar at the same
time, plus a third band underneath for the address field — so the catalog
screen showed four disabled nav buttons, a dead address row, and two search
fields asking the same question in two shapes. Now:

- `mode-tab` (an embedded tab is open): history cluster, tab title, address
  field (sharing the row, like a browser), "All apps" switcher.
- `mode-catalog` (no tab open): wordmark, search, result count, create button.

The catalog body no longer renders its own search field or `+ Create Custom App`
button; both live in the toolbar, so the search input survives a catalog
re-render mid-keystroke and the create action is one click from any state.
The standalone `Default URL` button is gone: the Home button in the history
cluster already goes to the app's start page.
