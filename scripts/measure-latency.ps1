<#
Key-to-frame latency through the real key path.

Needs a build with dev hooks, so BAYAN_PROFILE is honoured:
    cargo build --release --features dev-hooks

Starts Bayan with a throwaway USERPROFILE (defaults, no saved session
touched), presses `a` -Keys times and then Backspace as often, through
SendInput, and reads the latency lines BAYAN_PROFILE writes to
%TEMP%\bayan_profile.log. A key is sent only while Bayan's window is in the
foreground; otherwise the run stops, so keys never land in another program.

What the numbers cover is in src/latency.rs: from winit delivering the key
to the first PTY read after it (key->pty-output) and to the next frame
submitted to the GPU (key->frame). Not the OS input queue, not the display.
#>
param(
    [string]$Exe = "target\release\bayan.exe",
    [int]$Keys = 50,
    [int]$GapMs = 150
)
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class LatencyInput {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
  [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
  [DllImport("user32.dll")] public static extern uint SendInput(uint n, INPUT[] i, int size);
  [StructLayout(LayoutKind.Sequential)] public struct KEYBDINPUT { public ushort vk; public ushort scan; public uint flags; public uint time; public IntPtr extra; }
  [StructLayout(LayoutKind.Explicit, Size = 40)] public struct INPUT { [FieldOffset(0)] public uint type; [FieldOffset(8)] public KEYBDINPUT ki; }
  public static uint Pid(IntPtr h) { uint p; GetWindowThreadProcessId(h, out p); return p; }
  public static bool Front(IntPtr h) {
    uint dummy; uint ft = GetWindowThreadProcessId(GetForegroundWindow(), out dummy); uint me = GetCurrentThreadId();
    AttachThreadInput(me, ft, true); BringWindowToTop(h); SetForegroundWindow(h); AttachThreadInput(me, ft, false);
    return GetForegroundWindow() == h;
  }
  public static void Tap(ushort scan) {
    var i = new INPUT[2];
    i[0].type = 1; i[0].ki.scan = scan; i[0].ki.flags = 0x0008u;
    i[1].type = 1; i[1].ki.scan = scan; i[1].ki.flags = 0x0008u | 0x0002u;
    SendInput(2, i, Marshal.SizeOf(typeof(INPUT)));
  }
}
'@

$exePath = (Resolve-Path $Exe).Path
$log = Join-Path $env:TEMP 'bayan_profile.log'
$home2 = Join-Path $env:TEMP ("bayan-latency-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $home2 | Out-Null

$oldProfile = $env:USERPROFILE
$env:USERPROFILE = $home2; $env:BAYAN_PROFILE = '1'
try { $p = Start-Process -FilePath $exePath -PassThru }
finally { $env:USERPROFILE = $oldProfile; Remove-Item Env:BAYAN_PROFILE }

try {
    $h = [IntPtr]::Zero
    for ($i = 0; $i -lt 80 -and $h -eq [IntPtr]::Zero; $i++) {
        Start-Sleep -Milliseconds 250; $p.Refresh()
        if ($p.MainWindowTitle -match 'Bayan') { $h = $p.MainWindowHandle }
    }
    if ($h -eq [IntPtr]::Zero) { throw "no Bayan window (exited=$($p.HasExited))" }
    if ([LatencyInput]::Pid($h) -ne $p.Id) { throw "the window belongs to another process" }
    Start-Sleep -Seconds 4 # shell start and first prompt

    $sent = 0
    foreach ($scan in @(@(0x1E) * $Keys + @(0x0E) * $Keys)) { # a, then Backspace
        if ([LatencyInput]::GetForegroundWindow() -ne $h -and -not [LatencyInput]::Front($h)) {
            throw "Bayan lost the foreground after $sent keys; stopped"
        }
        [LatencyInput]::Tap([uint16]$scan); $sent++
        Start-Sleep -Milliseconds $GapMs
    }
    Start-Sleep -Milliseconds 500
    if ($p.HasExited) { throw "Bayan exited during the run" }
} finally {
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
}

$rx = 'latency key->pty-output ([\d.]+) ms, key->frame ([\d.]+) ms'
$rows = Get-Content $log | ForEach-Object { if ($_ -match $rx) { [pscustomobject]@{ out = [double]$Matches[1]; frame = [double]$Matches[2] } } }
if (-not $rows) { throw "no latency lines in $log; was this build made with --features dev-hooks?" }
function Pct($v, $p) { $s = @($v | Sort-Object); $s[[math]::Max(1, [math]::Ceiling($p / 100 * $s.Count)) - 1] }
foreach ($k in 'out', 'frame') {
    $v = $rows.$k
    $name = @{ out = 'key->pty-output'; frame = 'key->frame' }[$k]
    "{0,-16} n={1} p50={2:N1} p95={3:N1} max={4:N1} ms" -f $name, $v.Count, (Pct $v 50), (Pct $v 95), ($v | Measure-Object -Maximum).Maximum
}
"keys sent: $sent; samples: $($rows.Count) (a key typed before the previous echo is not its own sample)"
