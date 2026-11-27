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

A scenario that needs real input defines Invoke-SmokeInput($hwnd, $geom),
called after settle and before capture; Send-SmokeClick delivers a real
left/right click at a fraction of the client area.

An A/B scenario also defines $SmokeEnv2 (and optionally $SmokeConfig2,
flat config keys merged for the second launch, and $SmokeSettle2). The
first process is stopped, the second launched in a fresh temp directory,
and Invoke-SmokeAssert2($bmpA, $bmpB, $geom) compares the two captures.

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
    [DllImport("user32.dll")] public static extern bool ClientToScreen(IntPtr hWnd, ref POINT p);
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern void mouse_event(uint f, uint dx, uint dy, uint d, IntPtr e);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
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

# Count pixels differing from the corner background, over a sampled grid.
function Get-InkCount($bmp, [int]$x0, [int]$y0, [int]$x1, [int]$y1) {
    $bg = $bmp.GetPixel($x0, $y0)
    $n = 0
    for ($y = $y0; $y -le $y1; $y += 2) {
        for ($x = $x0; $x -le $x1; $x += 2) {
            $c = $bmp.GetPixel($x, $y)
            if ([math]::Abs($c.R - $bg.R) -gt 12 -or [math]::Abs($c.G - $bg.G) -gt 12 -or [math]::Abs($c.B - $bg.B) -gt 12) { $n++ }
        }
    }
    return $n
}

function Get-SmokeHwnd([int]$ProcessId) {
    $cb = [GuiSmokeNative+EnumWindowsProc] {
        param($hWnd, $lParam)
        $owner = 0
        [GuiSmokeNative]::GetWindowThreadProcessId($hWnd, [ref]$owner) | Out-Null
        if ($owner -eq $ProcessId -and [GuiSmokeNative]::IsWindowVisible($hWnd)) {
            $script:smokeHwnd = $hWnd
            return $false
        }
        return $true
    }
    $script:smokeHwnd = [IntPtr]::Zero
    [GuiSmokeNative]::EnumWindows($cb, [IntPtr]::Zero) | Out-Null
    return $script:smokeHwnd
}

function Get-SmokeCapture($hwnd) {
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
    if (-not $ok) { $bmp.Dispose(); throw 'PrintWindow refused the capture' }
    $dpi = [GuiSmokeNative]::GetDpiForWindow($hwnd)
    if ($dpi -lt 96) { $dpi = 96 }
    return @{ Bmp = $bmp; Geom = @{ Dpi = $dpi; W = $w; H = $h } }
}

# A real click at a fraction of the client area, through the same
# SetCursorPos + mouse_event path a user's mouse takes. Scenarios that
# need input define Invoke-SmokeInput($hwnd, $geom) and call this.
function Send-SmokeClick($hwnd, $geom, [double]$fx, [double]$fy, [string]$button) {
    $pt = New-Object GuiSmokeNative+POINT
    $pt.X = [int]($geom.W * $fx); $pt.Y = [int]($geom.H * $fy)
    [GuiSmokeNative]::ClientToScreen($hwnd, [ref]$pt) | Out-Null
    [GuiSmokeNative]::SetCursorPos($pt.X, $pt.Y) | Out-Null
    Start-Sleep -Milliseconds 250
    $down = 0x0002; $up = 0x0004
    if ($button -eq 'right') { $down = 0x0008; $up = 0x0010 }
    [GuiSmokeNative]::mouse_event($down, 0, 0, 0, [IntPtr]::Zero)
    Start-Sleep -Milliseconds 100
    [GuiSmokeNative]::mouse_event($up, 0, 0, 0, [IntPtr]::Zero)
}

function Wait-SmokeReady($p, [string]$log, [string]$stderr) {
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    while ([DateTime]::UtcNow -lt $deadline -and -not $p.HasExited) {
        if (Test-Path -LiteralPath $log) {
            $text = $null
            try { $text = Get-Content -LiteralPath $log -Raw -ErrorAction Stop } catch { $text = $null }
            if ($text -and $text -match ' ms  renderer ready') { return }
        }
        Start-Sleep -Milliseconds 200
    }
    $p.Refresh()
    if ($p.HasExited) { throw "Bayan exited before capture with $($p.ExitCode): $(Get-Content -LiteralPath $stderr -Raw)" }
    throw 'window never reached renderer ready'
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
    $bmp2 = $null
    $failed = $null
    try {
        $runHome = Join-Path $runRoot 'home'
        $runTemp = Join-Path $runRoot 'temp'
        New-Item -ItemType Directory -Path $runHome, $runTemp -Force | Out-Null
        $bayanDir = Join-Path $runHome '.bayan'
        New-Item -ItemType Directory -Path $bayanDir -Force | Out-Null
        $configPath = Join-Path $bayanDir 'config.json'
        # Deterministic chrome: bundled font, fixed metrics, still cursor.
        @{
            font_family = 'Kawkab Mono'; font_size = 16.0; padding = 0; opacity = 1.0
            cursor_blink = $false; shell = 'powershell.exe'; scroll = @{ smooth = $false }
        } | ConvertTo-Json | Set-Content -LiteralPath $configPath -Encoding UTF8
        @{
            tabs = @(); active = 0
            window = @{ x = 50; y = 50; w = 1000; h = 700; maximized = $false }
        } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $bayanDir 'session.json') -Encoding UTF8

        # The scenario defines $SmokeEnv, $SmokeSettle, Invoke-SmokeAssert,
        # and optionally $SmokeEnv2 / $SmokeConfig2 / Invoke-SmokeAssert2.
        $SmokeEnv = @{}
        $SmokeEnv2 = $null
        $SmokeConfig2 = $null
        $SmokeSettle = 2.0
        $SmokeSettle2 = $null
        # Scenarios are dot-sourced into this scope; a function one defines
        # (e.g. context-menu's Invoke-SmokeInput right-click) would otherwise
        # leak into every later scenario and click its grid. Reset them.
        foreach ($fn in 'Invoke-SmokeAssert', 'Invoke-SmokeAssert2', 'Invoke-SmokeInput') {
            if (Test-Path "Function:$fn") { Remove-Item "Function:$fn" -Force }
        }
        . $file
        # Isolate scenarios: a hook one sets (BAYAN_PALETTE, BAYAN_GUARD, ...)
        # must not survive into the next launch. Clear every BAYAN_* first,
        # then apply only this scenario's hooks plus the two base ones.
        Get-ChildItem Env: | Where-Object { $_.Name -like 'BAYAN_*' } | ForEach-Object {
            if (-not $saved.ContainsKey($_.Name)) { $saved[$_.Name] = $_.Value }
            Remove-Item "Env:$($_.Name)" -Force
        }
        foreach ($k in $SmokeEnv.Keys) {
            if (-not $saved.ContainsKey($k)) { $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process') }
            [Environment]::SetEnvironmentVariable($k, [string]$SmokeEnv[$k], 'Process')
        }
        $env:USERPROFILE = $runHome
        $env:HOME = $runHome
        $env:TEMP = $runTemp
        $env:TMP = $runTemp
        $env:BAYAN_PROFILE = '1'
        $env:BAYAN_SHOW_NOW = '1'
        $stderr = Join-Path $runRoot 'stderr.txt'
        $p = Start-Process -FilePath $exe -PassThru -WindowStyle Normal -RedirectStandardError $stderr
        Wait-SmokeReady $p (Join-Path $runTemp 'bayan_profile.log') $stderr
        Start-Sleep -Seconds $SmokeSettle
        $p.Refresh()
        if ($p.HasExited) { throw "Bayan exited during settle with $($p.ExitCode)" }
        $hwnd = Get-SmokeHwnd $p.Id
        if ($hwnd -eq [IntPtr]::Zero) { throw "no visible window owned by PID $($p.Id)" }
        if (Get-Command Invoke-SmokeInput -ErrorAction SilentlyContinue) {
            $r0 = New-Object GuiSmokeNative+RECT
            [GuiSmokeNative]::GetClientRect($hwnd, [ref]$r0) | Out-Null
            $d0 = [GuiSmokeNative]::GetDpiForWindow($hwnd)
            if ($d0 -lt 96) { $d0 = 96 }
            $geom0 = @{ Dpi = $d0; W = ($r0.R - $r0.L); H = ($r0.B - $r0.T) }
            [GuiSmokeNative]::SetForegroundWindow($hwnd) | Out-Null
            Start-Sleep -Milliseconds 400
            Invoke-SmokeInput $hwnd $geom0
        }
        $cap = Get-SmokeCapture $hwnd
        $bmp = $cap.Bmp
        $geom = $cap.Geom
        $p.Refresh()
        if ($p.HasExited) { throw 'Bayan exited during capture' }

        Invoke-SmokeAssert $bmp $geom

        if ($SmokeEnv2) {
            if (-not (Get-Command Invoke-SmokeAssert2 -ErrorAction SilentlyContinue)) { throw '$SmokeEnv2 needs Invoke-SmokeAssert2' }
            Stop-Process -Id $p.Id -ErrorAction Stop
            $p.WaitForExit(5000) | Out-Null
            if ($SmokeConfig2) {
                $cfg = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
                foreach ($k in $SmokeConfig2.Keys) {
                    if ($cfg.PSObject.Properties[$k]) { $cfg.$k = $SmokeConfig2[$k] }
                    else { $cfg | Add-Member -NotePropertyName $k -NotePropertyValue $SmokeConfig2[$k] }
                }
                $cfg | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $configPath -Encoding UTF8
            }
            $runTemp2 = Join-Path $runRoot 'temp2'
            New-Item -ItemType Directory -Path $runTemp2 -Force | Out-Null
            $env:TEMP = $runTemp2
            $env:TMP = $runTemp2
            foreach ($k in $SmokeEnv2.Keys) {
                if (-not $saved.ContainsKey($k)) { $saved[$k] = [Environment]::GetEnvironmentVariable($k, 'Process') }
                [Environment]::SetEnvironmentVariable($k, [string]$SmokeEnv2[$k], 'Process')
            }
            $stderr2 = Join-Path $runRoot 'stderr2.txt'
            $p = Start-Process -FilePath $exe -PassThru -WindowStyle Normal -RedirectStandardError $stderr2
            Wait-SmokeReady $p (Join-Path $runTemp2 'bayan_profile.log') $stderr2
            $settle2 = if ($SmokeSettle2) { $SmokeSettle2 } else { $SmokeSettle }
            Start-Sleep -Seconds $settle2
            $p.Refresh()
            if ($p.HasExited) { throw 'Bayan (B) exited during settle' }
            $hwnd2 = Get-SmokeHwnd $p.Id
            if ($hwnd2 -eq [IntPtr]::Zero) { throw "no visible window owned by PID $($p.Id) (B)" }
            $cap2 = Get-SmokeCapture $hwnd2
            $bmp2 = $cap2.Bmp
            Invoke-SmokeAssert2 $bmp $bmp2 $geom
        }

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
        foreach ($pair in @(@($bmp, "$name.png"), @($bmp2, "$name-b.png"))) {
            $b = $pair[0]
            if ($b) {
                if ($failed -or $KeepArtifacts) {
                    $png = Join-Path $runRoot $pair[1]
                    $b.Save($png, [System.Drawing.Imaging.ImageFormat]::Png)
                    if ($failed) { Write-Host "  artifact: $png" }
                }
                $b.Dispose()
            }
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
