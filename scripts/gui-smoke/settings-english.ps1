# BAYAN_SETTINGS=1 + BAYAN_LANG=en opens the settings panel in English: a
# large ledger over the grid. The panel region carries ink; the mirrored
# (labels-left) geometry is pinned by the render unit tests, this pins that
# the English panel reaches the screen.
$SmokeEnv = @{ BAYAN_SETTINGS = '1'; BAYAN_LANG = 'en' }
$SmokeSettle = 3.0

function Invoke-SmokeAssert($bmp, $geom) {
    $x0 = [int]($geom.W * 0.20); $x1 = [int]($geom.W * 0.80)
    $y0 = [int]($geom.H * 0.15); $y1 = [int]($geom.H * 0.80)
    Assert-Ink $bmp $x0 $y0 $x1 $y1
}
