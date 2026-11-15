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
# Read once: each run points TEMP at its own folder, so a later
# GetTempPath() would nest every run inside the one before it.
$baseTemp = [IO.Path]::GetTempPath()

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
    'BAYAN_IDLE_BUSY', 'BAYAN_TAP', 'BAYAN_TYPE', 'BAYAN_TABS'
)
$saved = @{}
foreach ($name in $names) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$failed = $false

function Invoke-QuietWindow {
    $root = Join-Path $baseTemp ('bayan-idle-' + [Guid]::NewGuid().ToString('N'))
    $runHome = Join-Path $root 'home'
    $temp = Join-Path $root 'temp'
    New-Item -ItemType Directory -Path $runHome, (Join-Path $runHome '.bayan'), $temp -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $runHome '.bayan\config.json'), '{"cursor_blink":false}')
    $env:USERPROFILE = $runHome
    $env:HOME = $runHome
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
    $root = Join-Path $baseTemp ('bayan-idle-min-' + [Guid]::NewGuid().ToString('N'))
    $runHome = Join-Path $root 'home'
    $temp = Join-Path $root 'temp'
    New-Item -ItemType Directory -Path $runHome, (Join-Path $runHome '.bayan'), $temp -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $runHome '.bayan\config.json'), '{"cursor_blink":false}')
    $env:USERPROFILE = $runHome
    $env:HOME = $runHome
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

# A fresh home (cursor blink off), temp directory and profile log for one
# owned process. Returns the paths; the caller sets its own extra hooks.
function New-ProbeRun([string]$prefix, [string]$config = '{"cursor_blink":false}') {
    $root = Join-Path $baseTemp ($prefix + [Guid]::NewGuid().ToString('N'))
    $runHome = Join-Path $root 'home'
    $temp = Join-Path $root 'temp'
    New-Item -ItemType Directory -Path $runHome, (Join-Path $runHome '.bayan'), $temp -Force | Out-Null
    [IO.File]::WriteAllText((Join-Path $runHome '.bayan\config.json'), $config)
    $env:USERPROFILE = $runHome
    $env:HOME = $runHome
    $env:TEMP = $temp
    $env:TMP = $temp
    $env:BAYAN_PROFILE = '1'
    $env:BAYAN_SHOW_NOW = '1'
    $env:BAYAN_FRAME_COUNTERS = '1'
    foreach ($name in 'BAYAN_IDLE_BUSY', 'BAYAN_TAP', 'BAYAN_TYPE', 'BAYAN_TABS') {
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    return @{
        root = $root
        temp = $temp
        log = Join-Path $temp 'bayan_profile.log'
        counters = Join-Path $temp 'bayan_frame_counters.txt'
        stderr = Join-Path $root 'stderr.txt'
    }
}

# Ten `cmd /d /q` tabs, the first active and nine in the background. Once
# the frame counters settle: private memory, and redraws over 5s.
function Invoke-TenTabs {
    $run = New-ProbeRun 'bayan-idle-tabs-'
    $env:BAYAN_TABS = '10'
    $proc = $null
    try {
        $proc = Start-Process -FilePath $exe -ArgumentList @('--', 'cmd.exe', '/d', '/q') -PassThru -WindowStyle Normal -RedirectStandardError $run.stderr
        $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
        Wait-Mark $proc $run.log $run.stderr ' ms  renderer ready' $deadline
        Wait-Mark $proc $run.log $run.stderr ' ms  first pty output' $deadline
        $start = Wait-Stable $proc $run.counters $deadline
        Start-Sleep -Seconds $WindowSeconds
        $proc.Refresh()
        if ($proc.HasExited) { throw "Bayan exited during the ten-tab window: $($proc.ExitCode)" }
        $end = Read-Counters $run.counters
        $mem = Get-Process -Id $proc.Id
        $private = Format-Mb $mem.PrivateMemorySize64
        $working = Format-Mb $mem.WorkingSet64
        $presents = $end.presents - $start.presents
        $redraws = $end.redraws - $start.redraws
        if ($presents -lt 0 -or $redraws -lt 0) { throw 'frame counters went backwards' }
        Write-Host "ten tabs private=$private working_set=$working presents=$presents redraws=$redraws"
        @(
            "PROBE tabs_memory tabs=10 private=$private MB working_set=$working MB"
            "PROBE idle_redraw tabs10 presents=$presents redraws=$redraws window_s=$WindowSeconds"
        ) | Tee-Object -FilePath $output -Append
    } finally {
        Stop-Owned $proc
    }
}

# `type` of a 16 MB ASCII file through the real ConPTY path, in `cmd /k`.
# A shell's exit does not close the pane on inbox ConPTY (conhost keeps the
# pipe open), so the end is the last frame: the time from `first pty output`
# to the last change in the frame counters, once they have held still for
# 3s. Scrollbar autohide is off so its delayed hide frame is not counted.
# Private bytes are sampled every 100 ms and the peak is gated. Conhost is in
# the middle, so this is end to end, not Bayan alone (the reader throughput
# probe is that).
function Invoke-LargeCat {
    $run = New-ProbeRun 'bayan-idle-cat-' '{"cursor_blink":false,"scrollbar":{"autohide":false}}'
    $file = Join-Path $run.temp 'cat-16mb.txt'
    $writer = [IO.StreamWriter]::new($file, $false, [Text.UTF8Encoding]::new($false))
    try {
        $filler = 'the quick brown fox jumps over the lazy dog 0123456789 abcdefghijklmnop'
        for ($i = 0; $i -lt 200000; $i++) { $writer.Write($i.ToString('000000', $inv)); $writer.Write(' '); $writer.WriteLine($filler) }
    } finally {
        $writer.Dispose()
    }
    $mb = [Math]::Round((Get-Item -LiteralPath $file).Length / 1e6)
    # Read it once first: an on-access scan of a fresh 16 MB file can hold the first
    # open for seconds, and the settle loop would end before any output.
    $direct = (& cmd.exe /d /c type "$file" | Measure-Object -Line).Lines
    if ($direct -ne 200000) { throw "large cat: cmd type outside Bayan read $direct lines" }
    $proc = $null
    try {
        # An elevated Bayan drops a `--` command (the hosted runner's token is elevated),
        # so open the cmd profile and type the command; `exit` ends cmd when type is done.
        # The pane stays (inbox ConPTY keeps the pipe open), and frames can pause for
        # seconds mid-flood, so quiet counters alone do not mean done.
        $env:BAYAN_TYPE = 'type "' + $file + '" & exit' + "`r"
        $proc = Start-Process -FilePath $exe -ArgumentList @('-p', '"Command Prompt"') -PassThru -WindowStyle Normal -RedirectStandardError $run.stderr
        Remove-Item Env:BAYAN_TYPE
        $deadline = [DateTime]::UtcNow.AddSeconds(180)
        Wait-Mark $proc $run.log $run.stderr ' ms  first pty output' $deadline
        $started = [DateTime]::UtcNow
        $base = (Get-Process -Id $proc.Id).PrivateMemorySize64
        $peak = $base
        $last = $null
        $changed = $started
        $doneAt = $null
        while ($true) {
            if ([DateTime]::UtcNow -ge $deadline) { throw 'large cat did not settle within 180s' }
            $proc.Refresh()
            if ($proc.HasExited) {
                if ($doneAt) { break }
                throw "Bayan exited during the cat: $($proc.ExitCode)"
            }
            $peak = [Math]::Max($peak, (Get-Process -Id $proc.Id).PrivateMemorySize64)
            $now = Read-Counters $run.counters
            if ($null -eq $last -or $now.redraws -ne $last.redraws -or $now.presents -ne $last.presents) {
                $last = $now
                $changed = [DateTime]::UtcNow
            }
            if (-not $doneAt) {
                $cmd = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($proc.Id) AND Name='cmd.exe'")
                if ($cmd.Count -eq 0) { $doneAt = [DateTime]::UtcNow }
            } elseif (([DateTime]::UtcNow - $changed).TotalSeconds -ge 2 -and ([DateTime]::UtcNow - $doneAt).TotalSeconds -ge 2) {
                break
            }
            Start-Sleep -Milliseconds 100
        }
        if ($doneAt -gt $changed) { $changed = $doneAt }
        $seconds = ($changed - $started).TotalSeconds.ToString('0.00', $inv)
        if ($changed -le $started) { throw 'large cat: no frame after the first output' }
        # The 10k-line scrollback alone is ~33 MB; less growth means the file never reached the grid.
        if (($peak - $base) -lt 16MB) {
            # Evidence for the log: the same type outside Bayan, Bayan's stderr, and its marks.
            # Late or stuck: watch 15 s more.
            $late = $peak
            for ($i = 0; $i -lt 150; $i++) { Start-Sleep -Milliseconds 100; $proc.Refresh(); if ($proc.HasExited) { break }; $late = [Math]::Max($late, (Get-Process -Id $proc.Id).PrivateMemorySize64) }
            $responding = $proc.Responding
            $err = if (Test-Path -LiteralPath $run.stderr) { Get-Content -LiteralPath $run.stderr -Raw } else { '' }
            $marks = if (Test-Path -LiteralPath $run.log) { (Get-Content -LiteralPath $run.log -Tail 20) -join ' | ' } else { '' }
            throw "large cat: private memory rose only $(Format-Mb ($peak - $base)) MB; the file's output did not arrive. 15 s later +$(Format-Mb ($late - $base)) MB; responding=$responding; frames=$($last.redraws)/$($last.presents); stderr: $err; marks: $marks"
        }
        $peakMb = Format-Mb $peak
        Write-Host "large cat ${mb} MB seconds=$seconds peak_private=$peakMb"
        "PROBE cat mb=$mb seconds=$seconds peak_private=$peakMb MB" | Tee-Object -FilePath $output -Append
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
        Invoke-TenTabs
    } catch {
        Write-Host "ten tabs failed: $_"
        $failed = $true
    }
    try {
        Invoke-LargeCat
    } catch {
        Write-Host "large cat failed: $_"
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
