# Runs every headless UI probe and reports one combined result.
#
# Each probe covers a different surface, so they are kept separate files and
# driven from here rather than duplicated:
#   probe-catalog.js  catalog, modal, browser picker, proxy hints
#   probe-startup.js  launch landing (fresh / remembered / nothing openable)
#                   plus sidebar drag-reorder and its persistence
#   probe-settings.js settings dialog + chat panel
#   probe-subtabs.js  sub-tab strip: pinned anchor, drag-reorder wiring
#
# Exits non-zero if any probe has a failing check, so it can gate a build.
$ErrorActionPreference = 'Stop'

$root = Split-Path $PSScriptRoot -Parent
$runner = Join-Path $PSScriptRoot 'probe.ps1'

# A 1x1 PNG stands in for real favicons: the sidebar only renders an <img> when
# an icon exists, and the drag assertions are about the <img> being present.
$png = 'data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg=='

function New-App($id, $name, $url, $engine, $enabled) {
  [pscustomobject]@{
    id = $id; name = $name; url = $url; engine = $engine
    enabled = $enabled; notifications = $true
    profile_mode = 'isolated'; icon = $png
  }
}

# ConvertTo-Json unwraps a one-element array into a bare object, so the brackets
# are restored by hand; list_apps must return an array or the sidebar breaks.
function ConvertTo-AppJson($items) {
  $parts = @($items | ForEach-Object { $_ | ConvertTo-Json -Depth 5 -Compress })
  '[' + ($parts -join ',') + ']'
}

$three = @(
  (New-App 'app-a' 'Alpha' 'https://a.test' 'os-webview' $true)
  (New-App 'app-b' 'Beta' 'https://b.test' 'os-webview' $false)   # disabled: must be skipped
  (New-App 'app-c' 'Gamma' 'https://c.test' 'os-webview' $true)
)
$externalOnly = @((New-App 'app-x' 'Ext' 'https://x.test' 'external' $true))

$suites = @(
  @{ Name = 'catalog'; Script = 'probe-catalog.js'; Args = @{} }
  @{ Name = 'startup: fresh launch'; Script = 'probe-startup.js'
     Args = @{ ListAppsJson = (ConvertTo-AppJson $three) } }
  @{ Name = 'startup: remembered tab'; Script = 'probe-startup.js'
     Args = @{ ListAppsJson = (ConvertTo-AppJson $three); LastActiveId = 'app-c' } }
  @{ Name = 'startup: external-only workspace'; Script = 'probe-startup.js'
     Args = @{ ListAppsJson = (ConvertTo-AppJson $externalOnly); ExpectWelcome = $true } }
  @{ Name = 'startup: empty workspace'; Script = 'probe-startup.js'
     Args = @{ ListAppsJson = '[]'; ExpectWelcome = $true } }
  @{ Name = 'settings + chat'; Script = 'probe-settings.js'
     Args = @{ ListAppsJson = (ConvertTo-AppJson $three); LastActiveId = 'app-a' } }
  @{ Name = 'sub-tab strip'; Script = 'probe-subtabs.js'
     Args = @{ ListAppsJson = (ConvertTo-AppJson $three) } }
)

$failed = 0
foreach ($suite in $suites) {
  "--- $($suite.Name) ---"
  $p = @{
    ProbeScript = Join-Path $PSScriptRoot $suite.Script
  } + $suite.Args
  # Output is captured rather than streamed so a failure in one suite does not
  # abort the run under $ErrorActionPreference = 'Stop'.
  $out = & $runner @p 2>&1 | Out-String
  $code = $LASTEXITCODE
  $out.TrimEnd() -split "`n" | Select-Object -Last 2 | ForEach-Object { "  $_" }
  if ($code -ne 0) {
    $failed++
    $out.TrimEnd() -split "`n" | Where-Object { $_ -match '^FAIL' } | ForEach-Object { "  $_" }
  }
  ''
}

if ($failed) { "FAILED: $failed suite(s)"; exit 1 }
"all $($suites.Count) suites passed"
