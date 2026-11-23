# Real-input drag probe.
#
# Driving the same page over CDP enters Chromium through the renderer's input
# pipeline, and there the whole drag sequence arrives. A mouse enters through the
# browser's window procedure instead, and on Windows that is the path that has to
# start an OLE drag loop. "Dragging does not work" lives on that path, so it is
# measured here with real cursor input. On the CEF engine this probe is how the
# missing `drop` was found: `dragstart`, `drag` and `dragover` arrive, the drop
# never does.
#
# The probe injects a throwaway div into one tab, drags it with the real cursor,
# and reads back which events the page saw. Nothing is navigated and the div is
# removed afterwards.
#
# Safety: the cursor is only pressed after `WindowFromPoint` confirms the pixel
# belongs to this app's own top-level window, so a minimized or covered window
# can never turn the probe into a click on whatever happens to be in front. It
# never sends keystrokes.
#
# It does move the mouse, raise the window and steal focus for a second.
#
#   powershell -File scripts/probe-drag-real.ps1 -AppId app-xxxx
#
#   -AppId    app to probe (required)
#   -TabId    sub-tab id, default main
#   -SkipDrag inject and report only (no cursor movement)

param(
  [Parameter(Mandatory = $true)][string]$AppId,
  [string]$TabId = 'main',
  [switch]$SkipDrag
)

$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class RWK {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int z, uint flags);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
$null = [RWK]::SetProcessDPIAware()

function Send-Mcp {
  param([string]$Method, [hashtable]$Params)
  $ep = Get-Content (Join-Path $env:LOCALAPPDATA 'rustbox\mcp-endpoint.json') -Raw | ConvertFrom-Json
  $client = New-Object System.Net.Sockets.TcpClient('127.0.0.1', $ep.port)
  $stream = $client.GetStream()
  $writer = New-Object System.IO.StreamWriter($stream)
  $reader = New-Object System.IO.StreamReader($stream)
  $body = @{ id = 1; token = $ep.token; method = $Method; params = $Params } |
    ConvertTo-Json -Compress -Depth 8
  $writer.WriteLine($body)
  $writer.Flush()
  $line = $reader.ReadLine()
  $client.Close()
  $reply = $line | ConvertFrom-Json
  if (-not $reply.ok) { throw "$Method failed: $($reply.error)" }
  return $reply.result
}

function Find-AppWindow {
  param([int]$ShellPid, [string]$Needle, [switch]$Exact)
  $script:found = [IntPtr]::Zero
  $script:biggest = 0
  $cb = [RWK+EnumProc]{
    param($h, $l)
    $ownerPid = 0
    [RWK]::GetWindowThreadProcessId($h, [ref]$ownerPid) | Out-Null
    if ($ownerPid -ne $script:shellPid) { return $true }
    $class = New-Object Text.StringBuilder 256
    [RWK]::GetClassNameW($h, $class, 256) | Out-Null
    if ($class.ToString() -ne 'Tauri Window') { return $true }
    $title = New-Object Text.StringBuilder 256
    [RWK]::GetWindowTextW($h, $title, 256) | Out-Null
    # A minimized window is still the right window: it gets restored below. What
    # matters is that the title names this app, not that it is already on screen.
    # With -Exact the shell itself is matched ('RustBox'), which is what has to be
    # restored first: while it is minimized the tab windows are parked off screen.
    if ($script:exact) {
      if ($title.ToString() -ne $Needle) { return $true }
      $r = New-Object RWK+RECT
      [RWK]::GetWindowRect($h, [ref]$r) | Out-Null
      $area = ($r.R - $r.L) * ($r.B - $r.T)
      if ($area -gt $script:biggest) { $script:biggest = $area; $script:found = $h }
    } elseif ($title.ToString() -like "*$Needle*") {
      $script:found = $h
    }
    return $true
  }
  $script:shellPid = $ShellPid
  $script:exact = [bool]$Exact
  [RWK]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
  return $script:found
}

$setup = @'
(() => {
  document.getElementById('__drag_probe')?.remove();
  const box = document.createElement('div');
  box.id = '__drag_probe';
  box.style.cssText = ['position:fixed','left:40px','top:40px','width:140px','height:100px',
    'background:#2b6','color:#fff','font:12px sans-serif','z-index:2147483647',
    'display:flex','align-items:center','justify-content:center','user-select:none'].join(';');
  box.textContent = 'drag me';
  box.draggable = true;
  const seen = [];
  window.__dragProbe = seen;
  for (const t of ['mousedown','mousemove','dragstart','drag','dragenter','dragover','drop','dragend','mouseup']) {
    box.addEventListener(t, (e) => {
      seen.push(t + '@' + Math.round(e.clientX) + ',' + Math.round(e.clientY));
      // A drop only happens on a target that accepts it, so the probe has to
      // accept: without preventDefault the engine ends the drag with `dragend`
      // and no `drop`, which would look like a missing drop event.
      if (t === 'dragover' || t === 'dragenter') e.preventDefault();
    });
    document.addEventListener(t, () => seen.push('doc:' + t));
  }
  document.body.appendChild(box);
  // A separate target further along the drag path, so a `drop` can land. The
  // source div is left behind as soon as the drag moves, so without this the
  // only possible outcome is `dragend`.
  document.getElementById('__drop_zone')?.remove();
  const zone = document.createElement('div');
  zone.id = '__drop_zone';
  zone.style.cssText = ['position:fixed','left:240px','top:100px','width:200px','height:140px',
    'background:#48f','color:#fff','font:12px sans-serif','z-index:2147483647',
    'display:flex','align-items:center','justify-content:center','user-select:none'].join(';');
  zone.textContent = 'drop here';
  for (const t of ['dragenter','dragover','drop']) {
    zone.addEventListener(t, (e) => {
      seen.push('zone:' + t);
      if (t !== 'drop') e.preventDefault();
    });
  }
  document.body.appendChild(zone);
  return {
    dpr: window.devicePixelRatio,
    focused: document.hasFocus(),
    visibility: document.visibilityState,
    screenX: window.screenX, screenY: window.screenY,
    outerW: window.outerWidth, outerH: window.outerHeight,
    innerW: window.innerWidth, innerH: window.innerHeight,
  };
})()
'@

$teardown = @'
(() => {
  const seen = window.__dragProbe || [];
  document.getElementById('__drag_probe')?.remove();
  document.getElementById('__drop_zone')?.remove();
  delete window.__dragProbe;
  return seen;
})()
'@

$cleanup = {
  try {
    $null = Send-Mcp -Method 'eval' -Params @{ appId = $AppId; tabId = $TabId; expression = $teardown }
  } catch {
    Write-Host "cleanup failed: $($_.Exception.Message)"
  }
}

$null = Send-Mcp -Method 'activate_tab' -Params @{ appId = $AppId; tabId = $TabId }
Start-Sleep -Milliseconds 500

$appName = (Send-Mcp -Method 'get_app' -Params @{ appId = $AppId }).app.name
# The browser process, not the largest `rustbox-cef` process: the others are
# renderers and own no top-level windows.
$shellPid = (Get-Content (Join-Path $env:LOCALAPPDATA 'rustbox\mcp-endpoint.json') -Raw |
  ConvertFrom-Json).pid

# Restore the shell first: while it is minimized Windows parks the tab windows at
# -32000,-32000, where no screen coordinate can reach them.
$shell = Find-AppWindow -ShellPid $shellPid -Needle 'RustBox' -Exact
if ($shell -ne [IntPtr]::Zero) {
  if ([RWK]::IsIconic($shell)) { [void][RWK]::ShowWindow($shell, 9) }
  [void][RWK]::ShowWindow($shell, 5)
  Start-Sleep -Milliseconds 600
}

$win = Find-AppWindow -ShellPid $shellPid -Needle $appName
if ($win -eq [IntPtr]::Zero) {
  $cleanup.Invoke()
  throw "no visible window for '$appName'; bring it on screen first"
}

if ([RWK]::IsIconic($win)) { [void][RWK]::ShowWindow($win, 9) }  # SW_RESTORE
[void][RWK]::ShowWindow($win, 5)                                  # SW_SHOW

# Foreground stealing is refused unless the caller is already the foreground
# thread, so tapping ALT first makes Windows treat this process as one. Retry a
# few times: the point is only to give the tab real keyboard focus so the OS
# sends it the drag, and a failed attempt must not leave the probe pressing into
# a window it did not activate.
$info = $null
for ($attempt = 1; $attempt -le 5; $attempt++) {
  [void][RWK]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)          # ALT down
  [void][RWK]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)          # ALT up
  [void][RWK]::SetForegroundWindow($win)
  [void][RWK]::BringWindowToTop($win)
  [void][RWK]::SetWindowPos($win, [IntPtr]::Zero, 0, 0, 0, 0, 0x0003)  # NOSIZE|NOMOVE|NOZORDER
  Start-Sleep -Milliseconds 250
  # Ask the app to focus the tab's webview *after* the window is foreground: the
  # foreground change itself clears DOM focus, so activating first only loses it.
  $null = Send-Mcp -Method 'activate_tab' -Params @{ appId = $AppId; tabId = $TabId }
  Start-Sleep -Milliseconds 250
  $info = Send-Mcp -Method 'eval' -Params @{ appId = $AppId; tabId = $TabId; expression = $setup }
  if ($info.focused) { break }
  "attempt ${attempt}: foreground=$([RWK]::GetForegroundWindow()) target=$win focused=$($info.focused)"
}
"app=$appName dpr=$($info.dpr) focused=$($info.focused) visibility=$($info.visibility)"
"window: screen $($info.screenX),$($info.screenY) outer $($info.outerW)x$($info.outerH) inner $($info.innerW)x$($info.innerH)"

if (-not $info.focused) {
  # The window can be foreground while its webview reports no DOM focus, so use
  # the OS answer as the second opinion: if the window really is the foreground
  # window, the cursor lands on it and the drag still exercises the real path.
  $fg = [RWK]::GetForegroundWindow()
  if ($fg -ne $win) {
    $cleanup.Invoke()
    throw 'the tab does not have focus; not pressing the mouse'
  }
  "note: window is foreground but the page reports no DOM focus; continuing"
}

if ($SkipDrag) {
  $cleanup.Invoke()
  return
}

$dpr = [double]$info.dpr
# Derive the press point from the window's real client origin, not from the
# page's screenX/screenY: those are stale while the window was parked off screen
# by a minimize.
$origin = New-Object RWK+POINT
$origin.X = 0
$origin.Y = 0
[RWK]::ClientToScreen($win, [ref]$origin) | Out-Null
# The probe div sits at client 40,40 and is 140x100; the drop zone at 240,100 is
# 200x140. Aim at the middle of each.
$x0 = $origin.X + [int](110 * $dpr)
$y0 = $origin.Y + [int](90 * $dpr)
$x1 = $origin.X + [int](340 * $dpr)
$y1 = $origin.Y + [int](170 * $dpr)

# The one check that makes this safe: the pixel under the press must belong to
# this app's own top-level window. A minimized window reports stale screen
# coordinates, and pressing anyway is how a probe clicks a stranger's window.
# The desktop is shared, so another window can take the foreground between the
# bring-to-front above and the press below; retry the bring-to-front instead of
# trusting one attempt.
$verified = $false
for ($attempt = 1; $attempt -le 6; $attempt++) {
  $pt = New-Object RWK+POINT
  $pt.X = $x0
  $pt.Y = $y0
  $top = [RWK]::GetAncestor([RWK]::WindowFromPoint($pt), 2)
  if ($top -eq $win) { $verified = $true; break }
  [void][RWK]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
  [void][RWK]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
  [void][RWK]::SetForegroundWindow($win)
  [void][RWK]::BringWindowToTop($win)
  [void][RWK]::SetWindowPos($win, [IntPtr]::Zero, 0, 0, 0, 0, 0x0003)
  Start-Sleep -Milliseconds 400
}
if (-not $verified) {
  $cleanup.Invoke()
  $title = New-Object Text.StringBuilder 256
  if ($top -ne [IntPtr]::Zero) { [RWK]::GetWindowTextW($top, $title, 256) | Out-Null }
  throw "the press point $x0,$y0 is not over this app (top-level '$($title.ToString())'); aborting without pressing"
}
"press at $x0,$y0 (verified over '$appName')"

[void][RWK]::SetCursorPos($x0, $y0)
Start-Sleep -Milliseconds 150
[void][RWK]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # left down
Start-Sleep -Milliseconds 150
# Move in small steps to the centre of the drop zone, then release there. The
# zone calls preventDefault on dragover, so a working drop target must produce a
# `drop`.
$steps = 12
for ($i = 1; $i -le $steps; $i++) {
  $x = $x0 + [int](($x1 - $x0) * $i / $steps)
  $y = $y0 + [int](($y1 - $y0) * $i / $steps)
  [void][RWK]::SetCursorPos($x, $y)
  Start-Sleep -Milliseconds 60
}
Start-Sleep -Milliseconds 300
[void][RWK]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # left up
Start-Sleep -Milliseconds 400

$seen = Send-Mcp -Method 'eval' -Params @{ appId = $AppId; tabId = $TabId; expression = $teardown }
$own = @($seen | Where-Object { $_ -notlike 'doc:*' })
"events: " + ($own -join ', ')
"drag events: " + (@($own | Where-Object { $_ -like '*drag*' }).Count)
