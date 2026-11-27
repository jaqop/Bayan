<#
Live smoke harness: hook-driven launch, PID-checked PrintWindow capture,
pixel assertions. The re-runnable form of the README's one-off live proofs.

Needs the dev-hooks release build the perf probes use:
    cargo build --locked --release --features dev-hooks

One fresh home and temp directory per scenario (the measure-startup.ps1
pattern), a deterministic config.json and a fixed 1000x700 window rect,
and only the process this script started is stopped. The user's ~/.bayan
is never read or written. Fails closed when the window never appears, the
PID dies, or an assertion fails; a failed scenario keeps its PNG and
profile log for inspection.

A scenario file (scripts/gui-smoke/<name>.ps1) defines:
    $SmokeEnv      hashtable of BAYAN_* environment for the launch
    $SmokeSettle   seconds to wait after `renderer ready` (default 2)
    Invoke-SmokeAssert($bmp, $geom)  — throws on a failed expectation
$geom carries Dpi, W, H of the captured client area. Coordinates scale
by Dpi/96; nothing is hard-coded in screen pixels.

Every assertion must be proven non-vacuous once (wrong expectation ->
FAIL) before it is trusted; see the plan's per-task steps.
#>
param(
    [string]$BinaryPath = 'target/release/bayan.exe',
    [string]$Scenario = '',
    [switch]$KeepArtifacts
)
$ErrorActionPreference = 'Stop'
$exe = (Resolve-Path -LiteralPath $BinaryPath).Path
$scenarioDir = Join-Path $PSScriptRoot 'gui-smoke'
Add-Type -AssemblyName System.Drawing

Add-Type @'
using System;
using System.Runtime.InteropServices;
public class GuiSmokeNative {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr v);
    public delegate bool EnumWindowsProc(IntPtr hWnd, IntPtr lParam);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumWindowsProc cb, IntPtr lParam);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hWnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hWnd);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hWnd, out RECT r);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
'@
# PerMonitorV2 (-4): PrintWindow pixels then match the window's own DPI.
[GuiSmokeNative]::SetProcessDpiAwarenessContext([IntPtr](-4)) | Out-Null

function Assert-Pixel($bmp, [int]$x, [int]$y, [int]$r, [int]$g, [int]$b, [int]$Tolerance = 2) {
    $c = $bmp.GetPixel($x, $y)
    if ([math]::Abs($c.R - $r) -gt $Tolerance -or [math]::Abs($c.G - $g) -gt $Tolerance -or [math]::Abs($c.B - $b) -gt $Tolerance) {
        throw "pixel ($x,$y) = ($($c.R),$($c.G),$($c.B)), want ($r,$g,$b) +/-$Tolerance"
    }
}

function Test-RegionUniform($bmp, [int]$x0, [int]$y0, [int]$x1, [int]$y1) {
    $first = $bmp.GetPixel($x0, $y0)
    for ($y = $y0; $y -le $y1; $y += 4) {
        for ($x = $x0; $x -le $x1; $x += 4) {
            $c = $bmp.GetPixel($x, $y)
            if ([math]::Abs($c.R - $first.R) -gt 8 -or [math]::Abs($c.G - $first.G) -gt 8 -or [math]::Abs($c.B - $first.B) -gt 8) {
                return $false
            }
        }
    }
    return $true
}

function Assert-Ink($bmp, [int]$x0, [int]$y0, [int]$x1, [int]$y1) {
    if (Test-RegionUniform $bmp $x0 $y0 $x1 $y1) { throw "region ($x0,$y0)-($x1,$y1) is uniform: nothing drawn" }
}

function Assert-NoInk($bmp, [int]$x0, [int]$y0, [int]$x1, [int]$y1) {
    if (-not (Test-RegionUniform $bmp $x0 $y0 $x1 $y1)) { throw "region ($x0,$y0)-($x1,$y1) carries ink, want uniform" }
}

function Find-ColorX($bmp, [int]$y, [int]$r, [int]$g, [int]$b, [int]$Tolerance = 8, [switch]$Last) {
    $hit = -1
    for ($x = 0; $x -lt $bmp.Width; $x++) {
        $c = $bmp.GetPixel($x, $y)
        if ([math]::Abs($c.R - $r) -le $Tolerance -and [math]::Abs($c.G - $g) -le $Tolerance -and [math]::Abs($c.B - $b) -le $Tolerance) {
            if (-not $Last) { return $x }
            $hit = $x
        }
    }
    return $hit
}

function Assert-SameBitmap($a, $b) {
    if ($a.Width -ne $b.Width -or $a.Height -ne $b.Height) { throw "bitmap sizes differ: $($a.Width)x$($a.Height) vs $($b.Width)x$($b.Height)" }
    for ($y = 0; $y -lt $a.Height; $y += 3) {
        for ($x = 0; $x -lt $a.Width; $x += 3) {
            $ca = $a.GetPixel($x, $y); $cb = $b.GetPixel($x, $y)
            if ($ca.R -ne $cb.R -or $ca.G -ne $cb.G -or $ca.B -ne $cb.B) { throw "bitmaps differ at ($x,$y)" }
        }
    }
}

function Get-SmokeHwnd([int]$ProcessId) {
    $found = [IntPtr]::Zero
    $cb = [GuiSmokeNative+EnumWindowsProc] {
        param($hWnd, $lParam)
        $pid2 = 0
        [GuiSmokeNative]::GetWindowThreadProcessId($hWnd, [ref]$pid2) | Out-Null
        if ($pid2 -eq $ProcessId -and [GuiSmokeNative]::IsWindowVisible($hWnd)) {
            $script:smokeHwnd = $hWnd
            return $false
        }
        return $true
    }
    $script:smokeHwnd = [IntPtr]::Zero
    [GuiSmokeNative]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    return $script:smokeHwnd
}

function Invoke-SmokeScenario([string]$file, [string]$exe) {
    $name = [IO.Path]::GetFileNameWithoutExtension($file)
    $runRoot = Join-Path ([IO.Path]::GetTempPath()) ('bayan-smoke-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $runRoot | Out-Null
    $names = @('USERPROFILE', 'HOME', 'TEMP', 'TMP', 'BAYAN_PROFILE', 'BAYAN_SHOW_NOW')
    $saved = @{}
    foreach ($n in $names) { $saved[$n] = [Environment]::GetEnvironmentVariable($n, 'Process') }
    $p = $null
    $bmp = $null
    $failed = $null
    try {
        $runHome = Join-Path $runRoot 'home'
        $runTemp = Join-Path $runRoot 'temp'
        New-Item -ItemType Directory -Path $runHome, $runTemp -Force | Out-Null
        $bayanDir = Join-Path $runHome '.bayan'
        New-Item -ItemType Directory -Path $bayanDir -Force | Out-Null
        # Deterministic chrome: bundled font, fixed metrics, still cursor.
        @{
            font_family = 'Kawkab Mono'; font_size = 16.0; padding = 0; opacity = 1.0
            cursor_blink = $false; shell = 'powershell.exe'; scroll = @{ smooth = $false }
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $bayanDir 'config.json') -Encoding UTF8
        @{
            tabs = @(); active = 0
            window = @{ x = 50; y = 50; w = 1000; h = 700; maximized = $false }
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $bayanDir 'session.json') -Encoding UTF8

        # The scenario defines $SmokeEnv, $SmokeSettle, Invoke-SmokeAssert.
        $SmokeEnv = @{}
        $SmokeSettle = 2.0
        . $file
        foreach ($k in $SmokeEnv.Keys) {
            $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process')
            [Environment]::SetEnvironmentVariable($k, [string]$SmokeEnv[$k], 'Process')
        }
        $env:USERPROFILE = $runHome
        $env:HOME = $runHome
        $env:TEMP = $runTemp
        $env:TMP = $runTemp
        $env:BAYAN_PROFILE = '1'
        $env:BAYAN_SHOW_NOW = '1'
        $log = Join-Path $runTemp 'bayan_profile.log'
        $stderr = Join-Path $runRoot 'stderr.txt'
        $p = Start-Process -FilePath $exe -PassThru -WindowStyle Normal -RedirectStandardError $stderr
        $deadline = [DateTime]::UtcNow.AddSeconds(30)
        $ready = $false
        while ([DateTime]::UtcNow -lt $deadline -and -not $p.HasExited) {
            if (Test-Path -LiteralPath $log) {
                $text = $null
                try { $text = Get-Content -LiteralPath $log -Raw -ErrorAction Stop } catch { $text = $null }
                if ($text -and $text -match ' ms  renderer ready') { $ready = $true; break }
            }
            Start-Sleep -Milliseconds 200
        }
        $p.Refresh()
        if ($p.HasExited) { throw "Bayan exited before capture with $($p.ExitCode): $(Get-Content -LiteralPath $stderr -Raw)" }
        if (-not $ready) { throw 'window never reached renderer ready' }
        Start-Sleep -Seconds $SmokeSettle
        $p.Refresh()
        if ($p.HasExited) { throw "Bayan exited during settle with $($p.ExitCode)" }

        $hwnd = Get-SmokeHwnd $p.Id
        if ($hwnd -eq [IntPtr]::Zero) { throw "no visible window owned by PID $($p.Id)" }
        $rect = New-Object GuiSmokeNative+RECT
        [GuiSmokeNative]::GetClientRect($hwnd, [ref]$rect) | Out-Null
        $w = $rect.R - $rect.L; $h = $rect.B - $rect.T
        if ($w -lt 100 -or $h -lt 100) { throw "client rect too small: ${w}x${h}" }
        $bmp = New-Object System.Drawing.Bitmap $w, $h
        $gfx = [System.Drawing.Graphics]::FromImage($bmp)
        $hdc = $gfx.GetHdc()
        # PW_RENDERFULLCONTENT = 2: captures the DWM surface of a GPU window.
        $ok = [GuiSmokeNative]::PrintWindow($hwnd, $hdc, 2)
        $gfx.ReleaseHdc()
        $gfx.Dispose()
        if (-not $ok) { throw 'PrintWindow refused the capture' }
        $dpi = [GuiSmokeNative]::GetDpiForWindow($hwnd)
        if ($dpi -lt 96) { $dpi = 96 }
        $geom = @{ Dpi = $dpi; W = $w; H = $h }
        $p.Refresh()
        if ($p.HasExited) { throw 'Bayan exited during capture' }

        Invoke-SmokeAssert $bmp $geom

        $p.Refresh()
        if ($p.HasExited) { throw 'Bayan exited during assertions' }
        Write-Host "PASS $name"
    } catch {
        $failed = $_.Exception.Message
        Write-Host "FAIL ${name}: $failed"
    } finally {
        if ($p -and -not $p.HasExited) {
            Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
            $p.WaitForExit(5000) | Out-Null
        }
        foreach ($n in $saved.Keys) { [Environment]::SetEnvironmentVariable($n, $saved[$n], 'Process') }
        if ($bmp) {
            if ($failed -or $KeepArtifacts) {
                $png = Join-Path $runRoot "$name.png"
                $bmp.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
                if ($failed) { Write-Host "  artifact: $png" }
            }
            $bmp.Dispose()
        }
        if (-not $failed -and -not $KeepArtifacts) {
            Remove-Item -LiteralPath $runRoot -Recurse -Force -ErrorAction SilentlyContinue
        }
    }
    return $null -eq $failed
}

# @() around the whole if: a single-element pipeline unrolls to a scalar
# on assignment, and $files[0] would then be the path's first character.
$files = @(if ($Scenario) {
    Join-Path $scenarioDir "$Scenario.ps1"
} else {
    Get-ChildItem -LiteralPath $scenarioDir -Filter '*.ps1' | Sort-Object Name | ForEach-Object { $_.FullName }
})
if (-not $files -or -not (Test-Path -LiteralPath $files[0])) { throw "no scenarios found in $scenarioDir" }
$allPass = $true
foreach ($f in $files) {
    if (-not (Invoke-SmokeScenario $f $exe)) { $allPass = $false }
}
if (-not $allPass) { exit 1 }
