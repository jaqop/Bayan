# BAYAN_ATLAS_STRESS=1 renders CJK, Latin and Arabic together so the glyph
# atlas holds several scripts' pages at once. The grid must carry ink (a
# quiet grid is empty); corruption/eviction would blank regions. Ink-based,
# so it is environment-robust and CI-safe.
$SmokeEnv = @{ BAYAN_ATLAS_STRESS = '1' }
$SmokeSettle = 4.0
$SmokeCiSafe = $true

function Invoke-SmokeAssert($bmp, $geom) {
    $x0 = [int]($geom.W * 0.10); $x1 = [int]($geom.W * 0.90)
    $y0 = [int]($geom.H * 0.20); $y1 = [int]($geom.H * 0.80)
    Assert-Ink $bmp $x0 $y0 $x1 $y1
}
