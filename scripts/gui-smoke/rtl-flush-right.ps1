# A pure-Arabic output line is an RTL paragraph: drawn flush with the
# pane's RIGHT edge, nothing at the left. Finds ink row-bands and asks
# for one whose leftmost ink is past mid-window and whose rightmost ink
# touches the right edge. The mixed command-echo line (LTR "echo" at the
# left) cannot satisfy it.
$SmokeEnv = @{
    BAYAN_TYPE = 'echo مرحبا بالعالم هذا نثر طويل يكفي' + "`r"
}
$SmokeSettle = 10.0
$SmokeCiSafe = $true

function Invoke-SmokeAssert($bmp, $geom) {
    $w = $geom.W; $h = $geom.H
    $bg = $bmp.GetPixel(2, [int]($h * 0.95))
    $bands = @()
    $cur = $null
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
            if ($null -eq $cur) { $cur = @{ Y0 = $y; Y1 = $y; MinX = $minx; MaxX = $maxx } }
            else {
                $cur.Y1 = $y
                if ($minx -lt $cur.MinX) { $cur.MinX = $minx }
                if ($maxx -gt $cur.MaxX) { $cur.MaxX = $maxx }
            }
        } elseif ($cur) { $bands += $cur; $cur = $null }
    }
    if ($cur) { $bands += $cur }
    foreach ($b in $bands) {
        if ($b.MinX -gt $w * 0.55 -and $b.MaxX -gt $w * 0.96) {
            return  # an RTL line flush with the right edge, nothing at the left
        }
    }
    $desc = ($bands | ForEach-Object { "y$($_.Y0)-$($_.Y1) x$($_.MinX)-$($_.MaxX)" }) -join '; '
    throw "no RTL flush-right row band (w=$w): $desc"
}
