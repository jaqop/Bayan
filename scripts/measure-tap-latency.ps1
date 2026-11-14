<#
Key-to-frame latency through BAYAN_TAP.

The CI path. scripts/measure-latency.ps1 still types with SendInput for a
local run where Bayan can take the foreground. This one does not: the dev
hook calls keyroute::type_key, the same function a winit press reaches, and
the sample starts at that call.

Needs the release build the startup step already produced:
    cargo build --locked --release --features dev-hooks

One fresh home and temp directory, cmd /d /q, and only the process this
script started is stopped. The profile log is that temp directory's, never
the user's. Fails closed when the process exits early or the log never
reaches `renderer ready`.
#>
param(
    [string]$BinaryPath = 'target/release/bayan.exe',
    [string]$OutputPath = 'latency.log',
    [ValidateRange(40, 200)][int]$Keys = 50,
    [ValidateRange(15, 180)][int]$TimeoutSeconds = 90
)
$ErrorActionPreference = 'Stop'
$inv = [Globalization.CultureInfo]::InvariantCulture
$exe = (Resolve-Path -LiteralPath $BinaryPath).Path
$output = [IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $output) { throw 'Use a fresh latency log path' }

function Percentile($values, $p) {
    # Nearest rank, matching src/latency.rs percentile().
    $sorted = @($values | Sort-Object)
    $rank = [math]::Ceiling(($p / 100.0) * $sorted.Count)
    if ($rank -lt 1) { $rank = 1 }
    if ($rank -gt $sorted.Count) { $rank = $sorted.Count }
    return [double]$sorted[$rank - 1]
}

$runRoot = Join-Path ([IO.Path]::GetTempPath()) ('bayan-latency-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runRoot | Out-Null
$names = @('USERPROFILE', 'HOME', 'TEMP', 'TMP', 'BAYAN_PROFILE', 'BAYAN_SHOW_NOW', 'BAYAN_TAP', 'BAYAN_FRAME_COUNTERS', 'BAYAN_TYPE', 'BAYAN_IDLE_BUSY')
$saved = @{}
foreach ($name in $names) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
$p = $null
try {
    $runHome = Join-Path $runRoot 'home'
    $runTemp = Join-Path $runRoot 'temp'
    New-Item -ItemType Directory -Path $runHome, $runTemp -Force | Out-Null
    $env:USERPROFILE = $runHome
    $env:HOME = $runHome
    $env:TEMP = $runTemp
    $env:TMP = $runTemp
    $env:BAYAN_PROFILE = '1'
    $env:BAYAN_SHOW_NOW = '1'
    $env:BAYAN_TAP = "$Keys"
    # The counter file is the idle probe's. Leaving it unset keeps a file
    # replace off the key->frame path.
    Remove-Item Env:BAYAN_FRAME_COUNTERS -ErrorAction SilentlyContinue
    Remove-Item Env:BAYAN_TYPE -ErrorAction SilentlyContinue
    Remove-Item Env:BAYAN_IDLE_BUSY -ErrorAction SilentlyContinue
    $log = Join-Path $runTemp 'bayan_profile.log'
    $stderr = Join-Path $runRoot 'stderr.txt'
    $p = Start-Process -FilePath $exe -ArgumentList @('--', 'cmd.exe', '/d', '/q') -PassThru -WindowStyle Normal -RedirectStandardError $stderr
    $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
    $ready = $false
    $sawDone = $false
    $lastCount = -1
    $stableSince = $null
    while ([DateTime]::UtcNow -lt $deadline) {
        $p.Refresh()
        if ($p.HasExited) { throw "Bayan exited during latency with $($p.ExitCode): $(Get-Content -LiteralPath $stderr -Raw)" }
        if (Test-Path -LiteralPath $log) {
            # The profile log is opened and closed on every mark. A sharing
            # violation on this poll is the next iteration's read.
            $text = $null
            try { $text = Get-Content -LiteralPath $log -Raw -ErrorAction Stop } catch { $text = $null }
            if (-not $text) { Start-Sleep -Milliseconds 200; continue }
            if ($text -match ' ms  renderer ready') { $ready = $true }
            if ($text -match ' ms  tap done') { $sawDone = $true }
            $count = [regex]::Matches($text, 'latency key->pty-output').Count
            if ($sawDone -and $count -eq $lastCount) {
                if ($null -eq $stableSince) { $stableSince = [DateTime]::UtcNow }
                if (([DateTime]::UtcNow - $stableSince).TotalSeconds -ge 1.5) { break }
            } else {
                $stableSince = $null
                $lastCount = $count
            }
        }
        Start-Sleep -Milliseconds 200
    }
    $p.Refresh()
    if ($p.HasExited) { throw "Bayan exited during latency with $($p.ExitCode): $(Get-Content -LiteralPath $stderr -Raw)" }
    if (-not $ready) { throw 'Latency run did not reach renderer ready' }
    if (-not (Test-Path -LiteralPath $log)) { throw 'Latency run wrote no profile log' }
    $text = Get-Content -LiteralPath $log -Raw
    $rows = @()
    $deltas = @()
    foreach ($line in ($text -split "`n")) {
        if ($line -match 'latency key->pty-output ([\d.]+) ms, key->frame ([\d.]+) ms') {
            $out = [double]::Parse($Matches[1], $inv)
            $frame = [double]::Parse($Matches[2], $inv)
            $rows += $frame
            $deltas += ($frame - $out)
        }
    }
    Write-Host "latency samples=$($rows.Count) tap_done=$sawDone"
    if ($rows.Count -lt 1) { throw "no latency lines in $log" }
    $p50 = (Percentile $rows 50).ToString('0.0', $inv)
    $p95 = (Percentile $rows 95).ToString('0.0', $inv)
    $d50 = (Percentile $deltas 50).ToString('0.0', $inv)
    $d95 = (Percentile $deltas 95).ToString('0.0', $inv)
    $n = $rows.Count
    @(
        "PROBE latency key->frame p50=$p50 p95=$p95 n=$n"
        "PROBE latency render p50=$d50 p95=$d95 n=$n"
    ) | Tee-Object -FilePath $output
} finally {
    if ($p -and -not $p.HasExited) {
        Stop-Process -Id $p.Id -ErrorAction Stop
        if (-not $p.WaitForExit(5000)) { throw 'Owned latency process did not exit' }
    }
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
