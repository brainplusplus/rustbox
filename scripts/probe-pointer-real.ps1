# Does the pointer-based reorder survive real OS mouse input?
#
# This is the test that matters. CDP input enters Chromium through the renderer's
# input pipeline, where everything works. The bug being fixed only ever appeared
# on the *real* cursor path (window procedure -> browser -> renderer), which is
# where HTML5 drag and drop lost its drop event. So the same shipping function is
# injected here and driven with mouse_event instead.
#
# The injected list is a throwaway overlay; nothing is navigated and it is
# removed afterwards.
#
#   powershell -File scripts/probe-pointer-real.ps1 -AppId app-xxxx

param(
  [Parameter(Mandatory = $true)][string]$AppId,
  [string]$TabId = 'main'
)

$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public class PR {
  public delegate bool EnumProc(IntPtr h, IntPtr l);
  [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetClassNameW(IntPtr h, StringBuilder s, int n);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr h, IntPtr after, int x, int y, int w, int z, uint flags);
  [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, IntPtr extra);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr h, ref POINT p);
  [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetAncestor(IntPtr h, uint flags);
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, IntPtr e);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
$null = [PR]::SetProcessDPIAware()

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
  $cb = [PR+EnumProc]{
    param($h, $l)
    $owner = 0
    [PR]::GetWindowThreadProcessId($h, [ref]$owner) | Out-Null
    if ($owner -ne $script:shellPid) { return $true }
    $class = New-Object Text.StringBuilder 256
    [PR]::GetClassNameW($h, $class, 256) | Out-Null
    if ($class.ToString() -ne 'Tauri Window') { return $true }
    $title = New-Object Text.StringBuilder 256
    [PR]::GetWindowTextW($h, $title, 256) | Out-Null
    if ($script:exact) {
      if ($title.ToString() -ne $Needle) { return $true }
      $r = New-Object PR+RECT
      [PR]::GetWindowRect($h, [ref]$r) | Out-Null
      $area = ($r.R - $r.L) * ($r.B - $r.T)
      if ($area -gt $script:biggest) { $script:biggest = $area; $script:found = $h }
    } elseif ($title.ToString() -like "*$Needle*") {
      $script:found = $h
    }
    return $true
  }
  $script:shellPid = $ShellPid
  $script:exact = [bool]$Exact
  [PR]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
  return $script:found
}

$html = Get-Content (Join-Path (Split-Path -Parent $PSScriptRoot) 'ui\index.html') -Raw
# Cut the function out by its blank-line boundary: the next top-level "function"
# after it. The file has backticks in it, so the boundary is found rather than
# written into a string literal.
$start = $html.IndexOf('function enablePointerReorder(')
if ($start -lt 0) { throw 'enablePointerReorder not found in ui/index.html' }
$m = [regex]::Match($html.Substring($start + 1), "\r?\nfunction ")
if (-not $m.Success) { throw 'the end of enablePointerReorder was not found' }
$end = $start + 1 + $m.Index
$fnSource = $html.Substring($start, $end - $start)

$setup = @"
(() => {
  document.getElementById('__reorder_probe')?.remove();
  let dragEndedAt = 0;
  $fnSource
  const box = document.createElement('div');
  box.id = '__reorder_probe';
  box.style.cssText = ['position:fixed','left:80px','top:120px','width:44px',
    'display:flex','flex-direction:column','gap:6px','z-index:2147483647'].join(';');
  const order = [];
  window.__reorderOrder = order;
  window.__reorderCalls = [];
  for (let i = 1; i <= 5; i++) {
    const item = document.createElement('div');
    item.className = 'probe-item';
    item.dataset.id = 'item-' + i;
    item.textContent = String(i);
    item.style.cssText = ['width:44px','height:44px','display:flex','align-items:center',
      'justify-content:center','background:#333','color:#fff','font:600 18px sans-serif',
      'border-radius:10px','user-select:none'].join(';');
    box.appendChild(item);
    order.push(item.dataset.id);
  }
  document.body.appendChild(box);
  enablePointerReorder({
    container: box,
    itemSelector: '.probe-item',
    horizontal: false,
    commit: (from, to, after) => {
      const list = window.__reorderOrder;
      list.splice(list.indexOf(from.dataset.id), 1);
      let at = list.indexOf(to.dataset.id);
      if (after) at += 1;
      list.splice(at, 0, from.dataset.id);
      for (const id of list) box.appendChild(box.querySelector('[data-id="' + id + '"]'));
      window.__reorderCalls.push({ from: from.dataset.id, to: to.dataset.id, after });
    },
  });
  const a = box.children[0].getBoundingClientRect();
  const b = box.children[3].getBoundingClientRect();
  return JSON.stringify({
    focused: document.hasFocus(),
    dpr: window.devicePixelRatio,
    screenX: window.screenX, screenY: window.screenY,
    outerW: window.outerWidth, outerH: window.outerHeight, innerW: window.innerWidth, innerH: window.innerHeight,
    fromClient: { x: a.left + a.width / 2, y: a.top + a.height / 2 },
    toClient: { x: b.left + b.width / 2, y: b.top + b.height / 2 + 12 },
  });
})()
"@

$teardown = @'
(() => {
  const out = {
    order: window.__reorderOrder || [],
    calls: window.__reorderCalls || [],
    dom: [...document.querySelectorAll('#__reorder_probe .probe-item')].map(el => el.dataset.id),
  };
  document.getElementById('__reorder_probe')?.remove();
  delete window.__reorderOrder;
  delete window.__reorderCalls;
  return JSON.stringify(out);
})()
'@

$null = Send-Mcp -Method 'activate_tab' -Params @{ appId = $AppId; tabId = $TabId }
Start-Sleep -Milliseconds 400

$appName = (Send-Mcp -Method 'get_app' -Params @{ appId = $AppId }).app.name
$shellPid = (Get-Content (Join-Path $env:LOCALAPPDATA 'rustbox\mcp-endpoint.json') -Raw |
  ConvertFrom-Json).pid

# The shell window must be restored first: while it is minimized the tab windows
# are parked off screen (Windows keeps them at -32000,-32000), and no screen
# coordinate can reach them.
$shell = Find-AppWindow -ShellPid $shellPid -Needle 'RustBox' -Exact
if ($shell -ne [IntPtr]::Zero) {
  if ([PR]::IsIconic($shell)) { [void][PR]::ShowWindow($shell, 9) }
  [void][PR]::ShowWindow($shell, 5)
  Start-Sleep -Milliseconds 600
}

$win = Find-AppWindow -ShellPid $shellPid -Needle $appName
if ($win -eq [IntPtr]::Zero) { throw "no visible window for '$appName'" }
if ([PR]::IsIconic($win)) { [void][PR]::ShowWindow($win, 9) }
[void][PR]::ShowWindow($win, 5)

$info = $null
for ($attempt = 1; $attempt -le 6; $attempt++) {
  [void][PR]::keybd_event(0x12, 0, 0, [IntPtr]::Zero)
  [void][PR]::keybd_event(0x12, 0, 2, [IntPtr]::Zero)
  [void][PR]::SetForegroundWindow($win)
  [void][PR]::BringWindowToTop($win)
  [void][PR]::SetWindowPos($win, [IntPtr]::Zero, 0, 0, 0, 0, 0x0003)
  Start-Sleep -Milliseconds 300
  $raw = Send-Mcp -Method 'eval' -Params @{ appId = $AppId; tabId = $TabId; expression = $setup }
  # The expression returns a JSON string so the shape survives the bridge; parse
  # it rather than reading properties off a string.
  $info = $raw | ConvertFrom-Json
  if ($info.focused) { break }
}
"app=$appName focused=$($info.focused) dpr=$($info.dpr)"

# The page-reported screenX/screenY can be stale while the window was parked off
# screen, so the click points are derived from the window's real client origin.
$origin = New-Object PR+POINT
$origin.X = 0
$origin.Y = 0
[PR]::ClientToScreen($win, [ref]$origin) | Out-Null
$dpr = [double]$info.dpr
$fromX = $origin.X + [int]($info.fromClient.x * $dpr)
$fromY = $origin.Y + [int]($info.fromClient.y * $dpr)
$toX = $origin.X + [int]($info.toClient.x * $dpr)
$toY = $origin.Y + [int]($info.toClient.y * $dpr)
"client origin $($origin.X),$($origin.Y)  from $fromX,$fromY -> $toX,$toY"

$verified = $false
$raised = $false
for ($attempt = 1; $attempt -le 6; $attempt++) {
  $pt = New-Object PR+POINT
  $pt.X = $fromX; $pt.Y = $fromY
  $top = [PR]::GetAncestor([PR]::WindowFromPoint($pt), 2)
  if ($top -eq $win) { $verified = $true; break }
  # Raising the window is enough when another app owns the foreground and refuses
  # to give it up; it is undone below, and it never sends the window keystrokes.
  [void][PR]::SetWindowPos($win, [IntPtr](-1), 0, 0, 0, 0, 0x0003)  # HWND_TOPMOST, NOSIZE|NOMOVE
  $raised = $true
  Start-Sleep -Milliseconds 400
}
if (-not $verified) {
  if ($raised) { [void][PR]::SetWindowPos($win, [IntPtr](-2), 0, 0, 0, 0, 0x0003) }  # HWND_NOTOPMOST
  $null = Send-Mcp -Method 'eval' -Params @{ appId = $AppId; tabId = $TabId; expression = $teardown }
  throw "the press point is not over '$appName'; aborting without pressing"
}

[void][PR]::SetCursorPos($fromX, $fromY)
Start-Sleep -Milliseconds 200
[void][PR]::mouse_event(0x0002, 0, 0, 0, [IntPtr]::Zero)   # left down
Start-Sleep -Milliseconds 200
$steps = 12
for ($i = 1; $i -le $steps; $i++) {
  $x = $fromX + [int](($toX - $fromX) * $i / $steps)
  $y = $fromY + [int](($toY - $fromY) * $i / $steps)
  [void][PR]::SetCursorPos($x, $y)
  Start-Sleep -Milliseconds 60
}
Start-Sleep -Milliseconds 300
[void][PR]::mouse_event(0x0004, 0, 0, 0, [IntPtr]::Zero)   # left up
Start-Sleep -Milliseconds 400

$rawResult = Send-Mcp -Method 'eval' -Params @{ appId = $AppId; tabId = $TabId; expression = $teardown }
$result = $rawResult | ConvertFrom-Json
if ($raised) { [void][PR]::SetWindowPos($win, [IntPtr](-2), 0, 0, 0, 0, 0x0003) }  # HWND_NOTOPMOST
"commit calls: $($result.calls | ConvertTo-Json -Compress)"
"order:        $($result.order -join ' | ')"
"dom order:    $($result.dom -join ' | ')"
if (($result.order -join ',') -eq 'item-2,item-3,item-4,item-1,item-5') {
  'PASS: a real cursor moved item-1 after item-4'
} else {
  'FAIL: unexpected order'
}
