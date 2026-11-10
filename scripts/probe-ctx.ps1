# Probes the right-click menu (ui/ctx.html) headlessly, for both an embedded and
# an external app, and prints the PASS/FAIL rows the probe wrote into
# <pre id="probe">.
#
# This exists because the menu is a separate popup window whose height used to
# be a hardcoded constant in Rust. When the menu gained a row the constant did
# not, and "Remove" was clipped and unreachable. The probe asserts that the page
# reports its real height and that every row fits inside it.
param(
  [string]$UiDir = "$PSScriptRoot\..\ui",
  [string]$ProbeScript = "$PSScriptRoot\probe-ctx.js",
  [int]$TimeoutSec = 120
)

$ErrorActionPreference = 'Stop'

$chrome = @(
  'C:\Program Files\Google\Chrome\Application\chrome.exe',
  'C:\Program Files (x86)\Google\Chrome\Application\chrome.exe',
  'C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe',
  'C:\Program Files\Microsoft\Edge\Application\msedge.exe'
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) { throw 'No Chrome/Edge found.' }

$src = Join-Path $UiDir 'ctx.html'
if (-not (Test-Path $src)) { throw "missing $src" }
if (-not (Test-Path $ProbeScript)) { throw "missing $ProbeScript" }

$page = Join-Path $UiDir '__probe-ctx.html'
$work = Join-Path $env:TEMP ('rb-ctxprobe-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null

try {
  # Stub the Tauri globals the page calls. `ctx_menu_fit` records what the page
  # asked for, which is exactly what the assertions read back.
  $stub = @'
<script>
window.__fits = [];
window.__invoked = [];
window.__TAURI__ = {
  core: { invoke: async (cmd, args) => {
    window.__invoked.push({ cmd: cmd, args: args || {} });
    if (cmd === 'list_apps') return [
      { id: 'emb', name: 'Embedded app', engine: 'os-webview', enabled: true, muted: false, notifications: true },
      { id: 'ext', name: 'External app', engine: 'external', enabled: true, muted: false, notifications: true }
    ];
    if (cmd === 'ctx_menu_fit') { window.__fits.push(args.height); return args.height; }
    return null;
  } }
};
</script>
'@

  $probe = [System.IO.File]::ReadAllText($ProbeScript, [System.Text.Encoding]::UTF8)
  $html = [System.IO.File]::ReadAllText($src, [System.Text.Encoding]::UTF8)
  $sink = '<pre id="probe" style="position:fixed;left:-9999px">pending</pre>'

  $html = $html -replace '(?s)(<head>)', "`$1`n$stub"
  $html = $html -replace '(?s)(</body>)', "$sink`n<script>`n$probe`n</script>`n`$1"
  [System.IO.File]::WriteAllText($page, $html, (New-Object System.Text.UTF8Encoding($false)))

  $total = 0
  $failed = 0
  foreach ($id in @('emb', 'ext')) {
    $outFile = Join-Path $work "dom-$id.html"
    $url = 'file:///' + $page.Replace('\', '/') + "?app=$id"
    $args = @(
      '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
      '--allow-file-access-from-files', '--hide-scrollbars', '--window-size=196,420',
      "--user-data-dir=$work\profile-$id",
      '--virtual-time-budget=12000',
      '--dump-dom', $url
    )
    $proc = Start-Process -FilePath $chrome -ArgumentList $args -NoNewWindow -PassThru `
      -RedirectStandardOutput $outFile -RedirectStandardError "$work\err-$id.txt"
    if (-not $proc.WaitForExit($TimeoutSec * 1000)) {
      $proc.Kill()
      throw "headless run timed out after ${TimeoutSec}s"
    }

    # Chrome children can still hold the file open for a moment.
    $dom = $null
    for ($i = 0; $i -lt 60; $i++) {
      try {
        $fs = [System.IO.File]::Open($outFile, 'Open', 'Read', 'ReadWrite')
        $sr = New-Object System.IO.StreamReader($fs)
        $dom = $sr.ReadToEnd()
        $sr.Close(); $fs.Close()
        break
      } catch { Start-Sleep -Milliseconds 150 }
    }
    if (-not $dom) { throw "could not read probe output for $id" }

    $m = [regex]::Match($dom, '(?s)<pre id="probe"[^>]*>(.*?)</pre>')
    if (-not $m.Success) {
      $err = if (Test-Path "$work\err-$id.txt") { Get-Content "$work\err-$id.txt" -Raw } else { '' }
      throw "probe element missing for $id (DOM $($dom.Length) bytes). stderr: $err"
    }
    $payload = $m.Groups[1].Value
    if ($payload -eq 'pending') {
      $err = if (Test-Path "$work\err-$id.txt") { Get-Content "$work\err-$id.txt" -Raw } else { '' }
      throw "probe did not finish for $id. stderr: $err"
    }
    $payload = $payload.Replace('&lt;', '<').Replace('&gt;', '>').Replace('&quot;', '"').
      Replace('&#39;', "'").Replace('&amp;', '&')

    foreach ($line in @($payload -split "`n" | Where-Object { $_ -match '^(PASS|FAIL)\|' })) {
      $parts = $line -split '\|', 3
      $total++
      $ok = $parts[0] -eq 'PASS'
      if (-not $ok) { $failed++ }
      $detail = if ($parts.Count -gt 2 -and $parts[2]) { "  [$($parts[2])]" } else { '' }
      '{0}  {1} ({2}){3}' -f $parts[0], $parts[1], $id, $detail
    }
  }

  ''
  '{0}/{1} checks passed' -f ($total - $failed), $total
  if ($failed -gt 0) { exit 1 }
}
finally {
  Remove-Item $page -Force -ErrorAction SilentlyContinue
  Remove-Item $work -Recurse -Force -ErrorAction SilentlyContinue
}
