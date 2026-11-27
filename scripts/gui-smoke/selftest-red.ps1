# The capture path's own proof: PowerShell echoes a truecolor-red SGR plus
# ED 2, so the whole grid paints (255,0,0). Three spread points must read
# red. If this fails, no other scenario's pixels can be trusted.
$SmokeEnv = @{
    BAYAN_TYPE = 'echo "$([char]27)[48;2;255;0;0m$([char]27)[2J"' + "`r"
}
$SmokeSettle = 6.0

function Invoke-SmokeAssert($bmp, $geom) {
    $w = $geom.W; $h = $geom.H
    # Below the tab bar, inside the grid; fractions of the client area.
    $top = [int](60 * $geom.Dpi / 96.0)
    $gh = $h - $top
    Assert-Pixel $bmp ([int]($w * 0.25)) ([int]($top + $gh * 0.30)) 255 0 0
    Assert-Pixel $bmp ([int]($w * 0.50)) ([int]($top + $gh * 0.50)) 255 0 0
    Assert-Pixel $bmp ([int]($w * 0.75)) ([int]($top + $gh * 0.70)) 255 0 0
}
