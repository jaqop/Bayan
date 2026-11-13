<#
Idle private memory, and redraws while a quiet tab is visible or a busy
tab is minimized.

Same release binary as the startup probe (`--features dev-hooks`). Each
window is its own process: a fresh home with cursor blink off, a fresh
temp directory, `cmd /d /q`, and only that process is stopped.

The visible window is also the memory sample: after `renderer ready` and
a quiet prompt, five seconds with no input. Private bytes are
Get-Process PrivateMemorySize64 (PrivateUsage). Working set is reported
and not gated. Child cmd/ConPTY processes are not included.

The minimized window sets BAYAN_IDLE_BUSY=1, waits until frame counters
are moving, then ShowWindow(SW_MINIMIZE). Its counts are report-only in
the gate until minimized redraws are stopped.

Both windows publish `%TEMP%\bayan_frame_counters.txt` because
BAYAN_FRAME_COUNTERS is set. The latency probe does not set it.
#>
param(
    [string]$BinaryPath = 'target/release/bayan.exe',
    [string]$OutputPath = 'idle.log',
    [ValidateRange(20, 180)][int]$TimeoutSeconds = 60
)
$ErrorActionPreference = 'Stop'
$inv = [Globalization.CultureInfo]::InvariantCulture
$WindowSeconds = 5
$exe = (Resolve-Path -LiteralPath $BinaryPath).Path
$output = [IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $output) { throw 'Use a fresh idle log path' }

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class IdleWindow {
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
}
'@

function Format-Mb([long]$bytes) {
    return ($bytes / 1MB).ToString('0.0', $inv)
}

function Read-Counters([string]$path) {
    for ($i = 0; $i -lt 20; $i++) {
        if (Test-Path -LiteralPath $path) {
            $text = Get-Content -LiteralPath $path -Raw
            if ($text -match 'redraws=(\d+) presents=(\d+)') {
                return @{ redraws = [int64]$Matches[1]; presents = [int64]$Matches[2] }
            }
        }
        Start-Sleep -Milliseconds 50
    }
    throw "frame counters missing or malformed: $path"
}

function Wait-Mark($proc, $log, $stderr, $pattern, $deadline) {
    while ([DateTime]::UtcNow -lt $deadline) {
        $proc.Refresh()
        if ($proc.HasExited) {
            throw "Bayan exited while waiting for ${pattern}: $($proc.ExitCode) $(Get-Content -LiteralPath $stderr -Raw)"
        }
        if (Test-Path -LiteralPath $log) {
            $hit = $false
            try { $hit = [bool](Select-String -LiteralPath $log -Pattern $pattern -Quiet -ErrorAction Stop) } catch { $hit = $false }
            if ($hit) { return }
        }
        Start-Sleep -Milliseconds 100
    }
    throw "timed out waiting for $pattern"
}

function Wait-Stable($proc, $counters, $deadline) {
    $prev = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $proc.Refresh()
        if ($proc.HasExited) { throw 'Bayan exited while frame counters were settling' }
        $now = Read-Counters $counters
        if ($null -ne $prev -and $now.redraws -eq $prev.redraws -and $now.presents -eq $prev.presents) {
            return $now
        }
        $prev = $now
        Start-Sleep -Milliseconds 1500
    }
    throw 'frame counters did not settle for a quiet window'
}

function Stop-Owned($proc) {
    if ($proc -and -not $proc.HasExited) {
        Stop-Process -Id $proc.Id -ErrorAction Stop
        if (-not $proc.WaitForExit(5000)) { throw 'Owned idle process did not exit' }
    }
}

$names = @(
    'USERPROFILE', 'HOME', 'TEMP', 'TMP',
    'BAYAN_PROFILE', 'BAYAN_SHOW_NOW', 'BAYAN_FRAME_COUNTERS',
    'BAYAN_IDLE_BUSY', 'BAYAN_TAP', 'BAYAN_TYPE'
)
$saved = @{}
foreach ($name in $names) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$failed = $false

function Invoke-QuietWindow {
    $root = Join-Path ([IO.Path]::GetTempPath()) ('bayan-idle-' + [Guid]::NewGuid().ToString('N'))
    $home = Join-Path $root 'home'
    $temp = Join-Path $root 'temp'
    New-Item -ItemType Directory -Path $home, (Join-Path $home '.bayan'), $temp -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $home '.bayan\config.json'), '{"cursor_blink":false}')
    $env:USERPROFILE = $home
    $env:HOME = $home
    $env:TEMP = $temp
    $env:TMP = $temp
    $env:BAYAN_PROFILE = '1'
    $env:BAYAN_SHOW_NOW = '1'
    $env:BAYAN_FRAME_COUNTERS = '1'
    Remove-Item Env:BAYAN_IDLE_BUSY -ErrorAction SilentlyContinue
    Remove-Item Env:BAYAN_TAP -ErrorAction SilentlyContinue
    Remove-Item Env:BAYAN_TYPE -ErrorAction SilentlyContinue
    $log = Join-Path $temp 'bayan_profile.log'
    $counters = Join-Path $temp 'bayan_frame_counters.txt'
    $stderr = Join-Path $root 'stderr.txt'
    $proc = $null
    try {
        $proc = Start-Process -FilePath $exe -ArgumentList @('--', 'cmd.exe', '/d', '/q') -PassThru -WindowStyle Normal -RedirectStandardError $stderr
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        Wait-Mark $proc $log $stderr ' ms  renderer ready' $deadline
        Wait-Mark $proc $log $stderr ' ms  first pty output' $deadline
        $start = Wait-Stable $proc $counters $deadline
        Start-Sleep -Seconds $WindowSeconds
        $proc.Refresh()
        if ($proc.HasExited) { throw "Bayan exited during the quiet window: $($proc.ExitCode)" }
        $end = Read-Counters $counters
        $mem = Get-Process -Id $proc.Id
        $private = Format-Mb $mem.PrivateMemorySize64
        $working = Format-Mb $mem.WorkingSet64
        $presents = $end.presents - $start.presents
        $redraws = $end.redraws - $start.redraws
        if ($presents -lt 0 -or $redraws -lt 0) { throw 'frame counters went backwards' }
        Write-Host "idle visible private=$private working_set=$working presents=$presents redraws=$redraws"
        @(
            "PROBE idle_memory private=$private MB working_set=$working MB"
            "PROBE idle_redraw visible presents=$presents redraws=$redraws window_s=$WindowSeconds"
        ) | Tee-Object -FilePath $output -Append
    } finally {
        Stop-Owned $proc
    }
}

function Invoke-MinimizedWindow {
    $root = Join-Path ([IO.Path]::GetTempPath()) ('bayan-idle-min-' + [Guid]::NewGuid().ToString('N'))
    $home = Join-Path $root 'home'
    $temp = Join-Path $root 'temp'
    New-Item -ItemType Directory -Path $home, (Join-Path $home '.bayan'), $temp -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $home '.bayan\config.json'), '{"cursor_blink":false}')
    $env:USERPROFILE = $home
    $env:HOME = $home
    $env:TEMP = $temp
    $env:TMP = $temp
    $env:BAYAN_PROFILE = '1'
    $env:BAYAN_SHOW_NOW = '1'
    $env:BAYAN_FRAME_COUNTERS = '1'
    $env:BAYAN_IDLE_BUSY = '1'
    Remove-Item Env:BAYAN_TAP -ErrorAction SilentlyContinue
    Remove-Item Env:BAYAN_TYPE -ErrorAction SilentlyContinue
    $log = Join-Path $temp 'bayan_profile.log'
    $counters = Join-Path $temp 'bayan_frame_counters.txt'
    $stderr = Join-Path $root 'stderr.txt'
    $proc = $null
    try {
        $proc = Start-Process -FilePath $exe -ArgumentList @('--', 'cmd.exe', '/d', '/q') -PassThru -WindowStyle Normal -RedirectStandardError $stderr
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        Wait-Mark $proc $log $stderr ' ms  renderer ready' $deadline
        $before = Read-Counters $counters
        Start-Sleep -Seconds 1
        $proc.Refresh()
        if ($proc.HasExited) { throw 'Bayan exited before minimize' }
        $after = Read-Counters $counters
        if ($after.redraws -le $before.redraws -and $after.presents -le $before.presents) {
            throw 'busy tab did not move frame counters before minimize'
        }
        $handle = [IntPtr]::Zero
        for ($i = 0; $i -lt 50 -and $handle -eq [IntPtr]::Zero; $i++) {
            $proc.Refresh()
            $handle = $proc.MainWindowHandle
            if ($handle -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 100 }
        }
        if ($handle -eq [IntPtr]::Zero) { throw 'minimized probe found no window handle' }
        # SW_MINIMIZE = 6. The return is the previous visibility, not success.
        [void][IdleWindow]::ShowWindow($handle, 6)
        Start-Sleep -Milliseconds 300
        if (-not [IdleWindow]::IsIconic($handle)) { throw 'ShowWindow did not minimize the window' }
        # Let the minimize itself finish redrawing before the measured window.
        Start-Sleep -Milliseconds 750
        $start = Read-Counters $counters
        Start-Sleep -Seconds $WindowSeconds
        $proc.Refresh()
        if ($proc.HasExited) { throw 'Bayan exited during the minimized window' }
        $end = Read-Counters $counters
        $presents = $end.presents - $start.presents
        $redraws = $end.redraws - $start.redraws
        if ($presents -lt 0 -or $redraws -lt 0) { throw 'frame counters went backwards' }
        Write-Host "idle minimized presents=$presents redraws=$redraws"
        "PROBE idle_redraw minimized presents=$presents redraws=$redraws window_s=$WindowSeconds" | Tee-Object -FilePath $output -Append
    } finally {
        Stop-Owned $proc
    }
}

try {
    try {
        Invoke-QuietWindow
    } catch {
        Write-Host "idle visible/memory failed: $_"
        $failed = $true
    }
    try {
        Invoke-MinimizedWindow
    } catch {
        Write-Host "idle minimized failed: $_"
        $failed = $true
    }
    if ($failed) { exit 1 }
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
