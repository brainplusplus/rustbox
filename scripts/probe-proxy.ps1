# Proves that a per-app proxy configured for an embedded tab actually reaches
# WebView2, and that it is absent when not configured.
#
# Method: the probe app points at `rb-probe.invalid`, a reserved name that can
# never resolve. The only way a request for it can reach the local logging proxy
# is if WebView2 was really launched with that proxy. The proxy answers with a
# fixed page, so the test needs no internet access.
#
# The real workspace config is backed up and restored around the run.
param(
  [string]$Exe = "$PSScriptRoot\..\src-tauri\target\release\rustbox.exe",
  [int]$Port = 8899,
  [int]$WaitSec = 25
)

$ErrorActionPreference = 'Stop'

$cfgPath = Join-Path $env:LOCALAPPDATA 'rustbox\config.json'
$backup = Join-Path $env:TEMP 'rb-config-backup.json'
$work = Join-Path $env:TEMP ('rb-proxy-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work -Force | Out-Null
$log = Join-Path $work 'proxy.log'

function Write-Config([bool]$WithProxy) {
  $proxy = if ($WithProxy) {
    '"proxy": { "scheme": "http", "host": "127.0.0.1", "port": ' + $Port +
      ', "username": null, "password": null },'
  } else {
    '"proxy": null,'
  }
  @"
{
  "apps": [
    {
      "id": "app-proxytest",
      "name": "ProxyTest",
      "url": "http://rb-probe.invalid/",
      "icon": null,
      "engine": "os-webview",
      "external_browser_path": null,
      "session": null,
      "profile_mode": "isolated",
      "fingerprint": null,
      "external_args": [],
      $proxy
      "user_agent": null,
      "enabled": true,
      "muted": false,
      "notifications": false
    }
  ],
  "favorites": [],
  "usage": {},
  "version": 3
}
"@ | Set-Content $cfgPath -Encoding UTF8
}

# Launch detached: a plain Start-Process child dies with this shell's job object,
# which would close the tab before WebView2 makes its first request.
#
# The variable is set inside a cmd wrapper rather than in this process because
# Win32_Process.Create runs under the WMI provider host and does not inherit our
# environment (verified: `%VAR%` comes through unexpanded otherwise).
function Start-App {
  $line = 'cmd.exe /c "set RB_PROBE_OPEN=1&& "' + $Exe + '""'
  $r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{
    CommandLine = $line
  }
  if ($r.ReturnValue -ne 0) { throw "Win32_Process.Create failed ($($r.ReturnValue))" }
  return [int]$r.ProcessId
}

function Stop-App($id) {
  if ($id) { Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }
  Get-Process rustbox -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
}

function Read-Log {
  if (-not (Test-Path $log)) { return @() }
  return @(Get-Content $log | Where-Object { $_.Trim() })
}

$proxyProc = $null
$appPid = 0
$results = @()

try {
  # The probe config lives under the app's real data root, so the workspace must
  # be backed up before it is overwritten.
  if (Test-Path $cfgPath) { Copy-Item $cfgPath $backup -Force }

  # No stdin redirection: a detached child gets a closed stdin, and node exits as
  # soon as that stream ends. The proxy stops on SIGTERM instead.
  $proxyProc = Start-Process -FilePath 'node' `
    -ArgumentList @((Join-Path $PSScriptRoot 'proxy-probe.mjs'), $Port, $log) `
    -NoNewWindow -PassThru
  $listening = $false
  for ($i = 0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 150
    if (Test-NetConnection -ComputerName '127.0.0.1' -Port $Port -InformationLevel Quiet -WarningAction SilentlyContinue) {
      $listening = $true
      break
    }
  }
  if (-not $listening) { throw "proxy did not start listening on port $Port" }

  # --- negative control: no proxy configured -------------------------------
  Write-Config $false
  Set-Content $log '' -Encoding UTF8
  $appPid = Start-App
  Start-Sleep -Seconds $WaitSec
  Stop-App $appPid; $appPid = 0
  Start-Sleep -Milliseconds 800
  $without = Read-Log
  $results += [pscustomobject]@{
    check = 'no proxy configured: nothing reaches the proxy'
    ok    = ($without.Count -eq 0)
    detail = if ($without.Count) { "$($without.Count) request(s) leaked" } else { 'log empty' }
  }

  # --- positive: proxy configured ------------------------------------------
  Write-Config $true
  Set-Content $log '' -Encoding UTF8
  $appPid = Start-App
  $deadline = (Get-Date).AddSeconds($WaitSec)
  $hits = @()
  while ((Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 700
    $hits = @(Read-Log | Where-Object { $_ -match 'rb-probe\.invalid' })
    if ($hits.Count) { break }
  }
  Stop-App $appPid; $appPid = 0

  $results += [pscustomobject]@{
    check = 'proxy configured: the tab request goes through the proxy'
    ok    = ($hits.Count -gt 0)
    detail = if ($hits.Count) { $hits[0] } else { 'no request seen' }
  }

  # The reserved name proves routing; this proves it was the app's own URL.
  $results += [pscustomobject]@{
    check = 'proxy saw the configured host'
    ok    = [bool]($hits | Where-Object { $_ -match '"host":"rb-probe\.invalid"' })
    detail = if ($hits.Count) { 'host header matches' } else { 'no request' }
  }
}
finally {
  Stop-App $appPid
  if ($proxyProc -and -not $proxyProc.HasExited) { Stop-Process -Id $proxyProc.Id -Force -ErrorAction SilentlyContinue }
  if (Test-Path $backup) { Copy-Item $backup $cfgPath -Force }
}

$fail = 0
foreach ($r in $results) {
  if (-not $r.ok) { $fail++ }
  '{0}  {1}  [{2}]' -f ($(if ($r.ok) { 'PASS' } else { 'FAIL' })), $r.check, $r.detail
}
''
'{0}/{1} checks passed' -f ($results.Count - $fail), $results.Count
if ($fail -gt 0) { exit 1 }
