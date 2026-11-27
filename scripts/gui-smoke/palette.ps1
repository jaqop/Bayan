# BAYAN_PALETTE=1 opens the command palette at startup: a card over the
# quiet grid. The center band carries card ink; without the hook it is empty.
$SmokeEnv = @{ BAYAN_PALETTE = '1' }
$SmokeSettle = 3.0

function Invoke-SmokeAssert($bmp, $geom) {
    $x0 = [int]($geom.W * 0.30); $x1 = [int]($geom.W * 0.70)
    $y0 = [int]($geom.H * 0.25); $y1 = [int]($geom.H * 0.60)
    Assert-Ink $bmp $x0 $y0 $x1 $y1
}
