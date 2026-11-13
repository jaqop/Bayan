param(
    [string]$BinaryPath = 'target/release/bayan.exe',
    [string]$OutputPath = 'startup.log',
    [ValidateRange(1,20)][int]$Runs = 5,
    [ValidateRange(1,120)][int]$TimeoutSeconds = 30
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $BinaryPath).Path
$output = [IO.Path]::GetFullPath($OutputPath)
if (Test-Path -LiteralPath $output) { throw 'Use a fresh startup log path' }
$runRoot = Join-Path ([IO.Path]::GetTempPath()) ('bayan-startup-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runRoot | Out-Null
$names = @('USERPROFILE','HOME','TEMP','TMP','BAYAN_PROFILE','BAYAN_SHOW_NOW')
$saved = @{}
foreach ($name in $names) { $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process') }
try {
    foreach ($run in 1..$Runs) {
        $dir = Join-Path $runRoot "run-$run"
        $runHome = Join-Path $dir 'home'
        $runTemp = Join-Path $dir 'temp'
        New-Item -ItemType Directory -Path $runHome, $runTemp -Force | Out-Null
        $env:USERPROFILE = $runHome
        $env:HOME = $runHome
        $env:TEMP = $runTemp
        $env:TMP = $runTemp
        $env:BAYAN_PROFILE = '1'
        $env:BAYAN_SHOW_NOW = '1'
        $log = Join-Path $runTemp 'bayan_profile.log'
        $stderr = Join-Path $dir 'stderr.txt'
        $p = $null
        try {
            # A visible first presentation is the measurement. /d skips cmd AutoRun.
            $p = Start-Process -FilePath $exe -ArgumentList @('--','cmd.exe','/d','/q') -PassThru -WindowStyle Normal -RedirectStandardError $stderr
            $deadline = [DateTime]::UtcNow.AddSeconds($TimeoutSeconds)
            $ready = $false
            while ([DateTime]::UtcNow -lt $deadline -and -not $p.HasExited) {
                if (Test-Path -LiteralPath $log) {
                    $text = Get-Content -LiteralPath $log -Raw
                    $ready = $text -match ' ms  gpu ready' -and $text -match ' ms  first frame presented' -and $text -match ' ms  renderer ready'
                    if ($ready) { break }
                }
                Start-Sleep -Milliseconds 100
            }
            if ($p.HasExited) { throw "Bayan exited during startup with $($p.ExitCode): $(Get-Content -LiteralPath $stderr -Raw)" }
            if (-not $ready) { throw "Startup run $run did not produce all required marks" }
            "--- startup run $run" | Tee-Object -FilePath $output -Append
            Get-Content -LiteralPath $log | Where-Object { $_ -notmatch 'latency|  key ' } | Tee-Object -FilePath $output -Append
        } finally {
            # Only this Start-Process result; never stop all processes named Bayan.
            if ($p -and -not $p.HasExited) {
                Stop-Process -Id $p.Id -ErrorAction Stop
                if (-not $p.WaitForExit(5000)) { throw 'Owned startup process did not exit' }
            }
        }
    }
} finally {
    foreach ($name in $names) { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
}
