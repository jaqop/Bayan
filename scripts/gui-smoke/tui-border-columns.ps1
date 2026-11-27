# PR #100's measurement, automated: three box rows, the middle one holding
# an Arabic field, each with a truecolor-red closing border parked at
# column 41 by CUP. The field path keeps every border on its grid column,
# so the three red marks share one x. Before the fix the Arabic row's
# whole-line shape pulled its border ~14 px left (1597 vs 1611 at 1650 px).
# The rows are NON-adjacent (10, 13, 16): adjacent rows' │ glyphs touch
# vertically and would cluster into one band.
$SmokeEnv = @{
    BAYAN_TYPE = 'Write-Host "$([char]27)[10;1H│ 1. Trust folder$([char]27)[10;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m"; Write-Host "$([char]27)[13;1H│ 2. (سلام abc مرحبا)$([char]27)[13;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m"; Write-Host "$([char]27)[16;1H│ 3. plain ascii row$([char]27)[16;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m"' + "`r"
}
$SmokeSettle = 6.0

function Invoke-SmokeAssert($bmp, $geom) {
    # Collect truecolor-red pixels (only our borders use 255,0,0), cluster
    # them into row bands, and compare each band's left edge.
    $hits = @()
    for ($y = 0; $y -lt $bmp.Height; $y++) {
        for ($x = 0; $x -lt $bmp.Width; $x++) {
            $c = $bmp.GetPixel($x, $y)
            if ([math]::Abs($c.R - 255) -le 4 -and $c.G -le 4 -and $c.B -le 4) {
                $hits += ,@($x, $y)
            }
        }
    }
    if ($hits.Count -lt 30) { throw "only $($hits.Count) red pixels: the borders never drew" }
    $bands = @()
    $cur = $null
    foreach ($h in ($hits | Sort-Object { $_[1] }, { $_[0] })) {
        if ($null -eq $cur -or $h[1] -gt $cur.Y1 + 3) {
            if ($cur) { $bands += $cur }
            $cur = @{ Y0 = $h[1]; Y1 = $h[1]; MinX = $h[0] }
        } else {
            $cur.Y1 = $h[1]
            if ($h[0] -lt $cur.MinX) { $cur.MinX = $h[0] }
        }
    }
    if ($cur) { $bands += $cur }
    if ($bands.Count -ne 3) { throw "expected 3 border bands, found $($bands.Count)" }
    $xs = @($bands | ForEach-Object { $_.MinX })
    $spread = ($xs | Measure-Object -Maximum -Minimum)
    if (($spread.Maximum - $spread.Minimum) -gt 2) {
        throw "border columns disagree: x = $($xs -join ', ') (spread $($spread.Maximum - $spread.Minimum) px, want <= 2)"
    }
}
