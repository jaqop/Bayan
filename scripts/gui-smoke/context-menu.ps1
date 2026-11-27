# A real right-click at the pane center opens the context menu (M39).
# The menu panel carries rows of ink where the quiet grid has none; the
# region right of center, mid-window, is inside the menu and empty otherwise.
# Proven live on the owner's Nov 19 binary before this scenario existed.
$SmokeSettle = 4.0

function Invoke-SmokeInput($hwnd, $geom) {
    Send-SmokeClick $hwnd $geom 0.5 0.5 'right'
    Start-Sleep -Milliseconds 1200
}

function Invoke-SmokeAssert($bmp, $geom) {
    $x0 = [int]($geom.W * 0.55); $x1 = [int]($geom.W * 0.95)
    $y0 = [int]($geom.H * 0.30); $y1 = [int]($geom.H * 0.80)
    Assert-Ink $bmp $x0 $y0 $x1 $y1
}
