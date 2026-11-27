# Universality (owner scope): Russian prose is an LTR paragraph — drawn
# flush LEFT, nothing at the right edge. Mirror of rtl-flush-right; proves
# the BiDi path does not misfire on Cyrillic.
$SmokeEnv = @{
    BAYAN_TYPE = 'echo Привет мир это длинная строка текста' + "`r"
}
$SmokeSettle = 10.0
$SmokeCiSafe = $true

function Invoke-SmokeAssert($bmp, $geom) {
    $w = $geom.W; $h = $geom.H
    $bg = $bmp.GetPixel(2, [int]($h * 0.95))
    $bands = @(); $cur = $null
    for ($y = [int]($h * 0.08); $y -lt [int]($h * 0.60); $y += 2) {
        $minx = -1; $maxx = -1
        for ($x = 2; $x -lt $w - 2; $x += 2) {
            $c = $bmp.GetPixel($x, $y)
            if ([math]::Abs($c.R - $bg.R) -gt 12 -or [math]::Abs($c.G - $bg.G) -gt 12 -or [math]::Abs($c.B - $bg.B) -gt 12) {
                if ($minx -lt 0) { $minx = $x }
                $maxx = $x
            }
        }
        if ($minx -ge 0) {
            if ($null -eq $cur) { $cur = @{ MinX = $minx; MaxX = $maxx } }
            else { if ($minx -lt $cur.MinX) { $cur.MinX = $minx }; if ($maxx -gt $cur.MaxX) { $cur.MaxX = $maxx } }
        } elseif ($cur) { $bands += $cur; $cur = $null }
    }
    if ($cur) { $bands += $cur }
    foreach ($b in $bands) {
        if ($b.MinX -lt $w * 0.10 -and $b.MaxX -lt $w * 0.60) { return }
    }
    $desc = ($bands | ForEach-Object { "x$($_.MinX)-$($_.MaxX)" }) -join '; '
    throw "no LTR left-aligned Cyrillic band (w=$w): $desc"
}
