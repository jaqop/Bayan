# Universality (owner scope): CJK wide glyphs inside a TUI field must keep
# the closing border on its grid column, exactly as Arabic does (PR #100's
# field path). Three non-adjacent box rows, the middle one a CJK field,
# truecolor-red closing borders at column 41; the three marks share one x.
$SmokeEnv = @{
    BAYAN_TYPE = 'Write-Host "$([char]27)[10;1H│ 1. Trust folder$([char]27)[10;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m"; Write-Host "$([char]27)[13;1H│ 2. (你好世界 テスト 한국어)$([char]27)[13;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m"; Write-Host "$([char]27)[16;1H│ 3. plain ascii row$([char]27)[16;41H$([char]27)[38;2;255;0;0m│$([char]27)[0m"' + "`r"
}
$SmokeSettle = 10.0
$SmokeCiSafe = $false # local-only: hosted WARP/inbox-ConPTY renders truecolor dimmed and drops Write-Host+CUP

function Invoke-SmokeAssert($bmp, $geom) {
    $hits = @()
    for ($y = 0; $y -lt $bmp.Height; $y++) {
        for ($x = 0; $x -lt $bmp.Width; $x++) {
            $c = $bmp.GetPixel($x, $y)
            if ([math]::Abs($c.R - 255) -le 4 -and $c.G -le 4 -and $c.B -le 4) { $hits += ,@($x, $y) }
        }
    }
    if ($hits.Count -lt 30) { throw "only $($hits.Count) red pixels: the borders never drew" }
    $bands = @(); $cur = $null
    foreach ($h in ($hits | Sort-Object { $_[1] }, { $_[0] })) {
        if ($null -eq $cur -or $h[1] -gt $cur.Y1 + 3) {
            if ($cur) { $bands += $cur }
            $cur = @{ Y0 = $h[1]; Y1 = $h[1]; MinX = $h[0] }
        } else { $cur.Y1 = $h[1]; if ($h[0] -lt $cur.MinX) { $cur.MinX = $h[0] } }
    }
    if ($cur) { $bands += $cur }
    if ($bands.Count -ne 3) { throw "expected 3 border bands, found $($bands.Count)" }
    $xs = @($bands | ForEach-Object { $_.MinX })
    $sp = $xs | Measure-Object -Maximum -Minimum
    if (($sp.Maximum - $sp.Minimum) -gt 2) { throw "border columns disagree: x = $($xs -join ', ')" }
}
