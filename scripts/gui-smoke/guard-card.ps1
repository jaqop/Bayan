# BAYAN_GUARD=1 arms the multi-line paste confirm card at startup: a card
# over the quiet grid. Same center-ink assertion as palette (shared proof).
$SmokeEnv = @{ BAYAN_GUARD = '1' }
$SmokeSettle = 3.0

function Invoke-SmokeAssert($bmp, $geom) {
    $x0 = [int]($geom.W * 0.30); $x1 = [int]($geom.W * 0.70)
    $y0 = [int]($geom.H * 0.25); $y1 = [int]($geom.H * 0.60)
    Assert-Ink $bmp $x0 $y0 $x1 $y1
}
