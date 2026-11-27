# Arabic reaches the screen as shaped ink. A/B: the same positioned line
# fed as Arabic prose, then as spaces. The Arabic capture must carry far
# more ink in the target band. Letter-connection IDENTITY is pinned by the
# render unit tests (glyph inspection); this pins that the shaping path's
# output reaches the window at all, in the right place.
$SmokeEnv = @{
    BAYAN_TYPE = 'Write-Host "$([char]27)[12;10Hمرحبا بالعالم هذا نثر"' + "`r"
}
$SmokeEnv2 = @{
    BAYAN_TYPE = 'Write-Host "$([char]27)[12;10H                    "' + "`r"
}
$SmokeSettle = 10.0

# Row 12's glyphs occupy ~0.62-0.67H at the harness's fixed metrics
# (~35 px cells, 700 px window, tab bar ~65 px). The band stops above the
# next prompt row (13), which would otherwise dilute the A/B ratio.
function Invoke-SmokeAssert($bmp, $geom) {
    $y0 = [int]($geom.H * 0.62); $y1 = [int]($geom.H * 0.67)
    $x0 = [int]($geom.W * 0.05); $x1 = [int]($geom.W * 0.95)
    $ink = Get-InkCount $bmp $x0 $y0 $x1 $y1
    if ($ink -lt 100) { throw "Arabic line drew only $ink ink samples in the target band (want >= 100)" }
}

function Invoke-SmokeAssert2($a, $b, $geom) {
    $y0 = [int]($geom.H * 0.62); $y1 = [int]($geom.H * 0.67)
    $x0 = [int]($geom.W * 0.05); $x1 = [int]($geom.W * 0.95)
    $inkA = Get-InkCount $a $x0 $y0 $x1 $y1
    $inkB = Get-InkCount $b $x0 $y0 $x1 $y1
    if ($inkA -le 2 * $inkB) { throw "Arabic ink $inkA does not dominate the spaces feed $inkB (want > 2x)" }
}
