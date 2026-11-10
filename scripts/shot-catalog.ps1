# Launch RustBox, bring it to the foreground, drive it with keystrokes, and
# capture its window with PrintWindow (no desktop capture, no other app raised).
param(
  [string]$Exe = 'C:\Users\brainplusplus\Desktop\RustBox.exe',
  [string]$Out = 'D:\Rust\rustbox\shot-catalog.png',
  [string]$Keys = '^k',
  [int]$Warmup = 9
)

Add-Type -AssemblyName System.Drawing, System.Windows.Forms

$src = @'
using System;
using System.Runtime.InteropServices;
public class W32 {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr hWnd, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr hWnd);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr hWnd, int cmd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
}
'@
if (-not ('W32' -as [type])) { Add-Type -TypeDefinition $src }
[void][W32]::SetProcessDPIAware()

Get-Process RustBox -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 700

$p = Start-Process -FilePath $Exe -PassThru
Start-Sleep -Seconds $Warmup
$p.Refresh()
if ($p.HasExited) { throw "app exited early, code=$($p.ExitCode)" }
$h = $p.MainWindowHandle
if ($h -eq 0) { throw "no main window handle" }

# Focus without stealing: attach our input queue to the current foreground
# thread, raise RustBox, then detach.
$fgPid = [uint32]0
$fgThread = [W32]::GetWindowThreadProcessId([W32]::GetForegroundWindow(), [ref]$fgPid)
$me = [W32]::GetCurrentThreadId()
[void][W32]::AttachThreadInput($me, $fgThread, $true)
[void][W32]::BringWindowToTop($h)
[void][W32]::SetForegroundWindow($h)
[void][W32]::AttachThreadInput($me, $fgThread, $false)
Start-Sleep -Milliseconds 900

$fgPid = [uint32]0
[void][W32]::GetWindowThreadProcessId([W32]::GetForegroundWindow(), [ref]$fgPid)
if ($fgPid -ne $p.Id) { "WARN foreground pid=$fgPid expected=$($p.Id)" }

if ($Keys) {
  [System.Windows.Forms.SendKeys]::SendWait($Keys)
  Start-Sleep -Seconds 3
}

$r = New-Object W32+RECT
[void][W32]::GetWindowRect($h, [ref]$r)
$w = $r.Right - $r.Left; $hh = $r.Bottom - $r.Top
$bmp = New-Object System.Drawing.Bitmap $w, $hh
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [W32]::PrintWindow($h, $hdc, 2)
$g.ReleaseHdc($hdc); $g.Dispose()
if (-not $ok) { throw "PrintWindow failed" }
$bmp.Save($Out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()

"OK pid=$($p.Id) rect=${w}x${hh} bytes=$((Get-Item -LiteralPath $Out).Length)"
