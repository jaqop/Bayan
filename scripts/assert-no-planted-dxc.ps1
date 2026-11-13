param(
    [Parameter(Mandatory = $true)][string]$BinaryPath,
    [Parameter(Mandatory = $true)][string]$PlantSource
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $BinaryPath).Path
$source = (Resolve-Path -LiteralPath $PlantSource).Path
# tests/fixtures/marker_dxcompiler.dll, built from marker_dxcompiler.c.
$expected = '4327247952c12c9809fd034fc38610709b84e2da74b40517edd9d2174aae8a2e'
$actual = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "marker DLL hash mismatch: $actual" }
$dir = [IO.Path]::GetDirectoryName($exe)
if ([string]::IsNullOrWhiteSpace($dir)) { throw 'The executable has no directory' }
# These two names only. Never remove $dir itself.
$planted = [IO.Path]::Combine($dir, 'dxcompiler.dll')
$marker = [IO.Path]::Combine($dir, 'dxcompiler.loaded')
if ([IO.Path]::GetFileName($planted) -ne 'dxcompiler.dll') { throw 'Refusing to plant an unexpected name' }
Copy-Item -LiteralPath $source -Destination $planted -Force
if (Test-Path -LiteralPath $marker) { Remove-Item -LiteralPath $marker -Force }

$runRoot = Join-Path ([IO.Path]::GetTempPath()) ('bayan-plant-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $runRoot | Out-Null
$saved = @{}
foreach ($name in @('TEMP', 'TMP', 'BAYAN_PROFILE', 'BAYAN_SHOW_NOW')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$process = $null
try {
    $env:TEMP = $runRoot
    $env:TMP = $runRoot
    $env:BAYAN_PROFILE = '1'
    $env:BAYAN_SHOW_NOW = '1'
    $log = Join-Path $runRoot 'bayan_profile.log'
    $stderr = Join-Path $runRoot 'stderr.txt'
    $process = Start-Process -FilePath $exe -ArgumentList @('--', 'cmd.exe', '/d', '/q') -PassThru -WindowStyle Normal -RedirectStandardError $stderr
    $deadline = [DateTime]::UtcNow.AddSeconds(90)
    $ready = $false
    while ([DateTime]::UtcNow -lt $deadline -and -not $process.HasExited) {
        if (Test-Path -LiteralPath $log) {
            $text = Get-Content -LiteralPath $log -Raw
            $ready = $text -match ' ms  gpu ready' -and $text -match ' ms  first frame presented' -and $text -match ' ms  renderer ready'
            if ($ready) { break }
        }
        Start-Sleep -Milliseconds 200
    }
    if ($process.HasExited) {
        $detail = if (Test-Path -LiteralPath $stderr) { Get-Content -LiteralPath $stderr -Raw } else { '' }
        throw "Bayan exited before the renderer was ready ($($process.ExitCode)): $detail"
    }
    if (-not $ready) { throw 'Startup did not reach renderer ready' }
    $loaded = @(Get-Process -Id $process.Id -Module | Where-Object { $_.ModuleName -eq 'dxcompiler.dll' })
    if ($loaded.Count -ne 0) {
        throw "dxcompiler.dll was mapped: $($loaded[0].FileName)"
    }
    if (Test-Path -LiteralPath $marker) {
        throw "The planted dxcompiler.dll ran (marker $marker)"
    }
    Write-Output 'Planted dxcompiler.dll was not loaded; renderer ready.'
} finally {
    if ($process -and -not $process.HasExited) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        [void]$process.WaitForExit(5000)
    }
    if ([IO.Path]::GetFileName($planted) -eq 'dxcompiler.dll') {
        Remove-Item -LiteralPath $planted -Force -ErrorAction SilentlyContinue
    }
    if ([IO.Path]::GetFileName($marker) -eq 'dxcompiler.loaded') {
        Remove-Item -LiteralPath $marker -Force -ErrorAction SilentlyContinue
    }
    foreach ($name in @($saved.Keys)) {
        [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process')
    }
    Remove-Item -LiteralPath $runRoot -Recurse -Force -ErrorAction SilentlyContinue
}
