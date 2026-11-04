# Generates the HoldMyRAM app icons. Three concepts, each with a normal (blue/teal) and an ON (green) variant.
#
#   A  RAM stick held between two clamp brackets (the grip is the silhouette)
#   B  RAM stick whose chips form a pause/"hold" glyph (two bars) on a squircle tile
#   A2 refinement of A: same stick, brackets OPEN (normal) or CLOSED IN and gripping (on)
#   A3 like A2 with a smaller stick and brackets that wrap onto its face when closed
#   C  flat chip outline with a notch and one accent bar, no gradients
#
# State is readable by SHAPE, not just colour: the "on" variant has the key element SOLID,
# the normal variant has it HOLLOW (A: stick, B: pause bars, C: short bar vs full bar).
#
# Usage (from the repo root):
#   powershell -ExecutionPolicy Bypass -File scripts\make-icons.ps1 -Concept B           # writes crates\app\icons
#   powershell -ExecutionPolicy Bypass -File scripts\make-icons.ps1 -Concept A -OutDir C:\temp\x
#   powershell -ExecutionPolicy Bypass -File scripts\make-icons.ps1 -Preview             # concept PNGs + sheets in docs\icon-concepts
# Output (final): icon.ico (16/20/24/32/48/64/128/256, PNG entries), 32x32.png, 32x32-on.png, 128x128.png, icon.png
param(
    [ValidateSet('A', 'A2', 'A3', 'B', 'C')][string]$Concept = 'B',
    [string]$OutDir,
    [switch]$Preview
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $root 'crates\app\icons' }

function Hex([string]$h) { [System.Drawing.ColorTranslator]::FromHtml($h) }
function Brush([string]$h) { New-Object System.Drawing.SolidBrush (Hex $h) }

function RR([single]$x, [single]$y, [single]$w, [single]$h, [single]$r) {
    $p = New-Object System.Drawing.Drawing2D.GraphicsPath
    $d = [Math]::Min($r * 2, [Math]::Min($w, $h))
    $p.AddArc($x, $y, $d, $d, 180, 90)
    $p.AddArc($x + $w - $d, $y, $d, $d, 270, 90)
    $p.AddArc($x + $w - $d, $y + $h - $d, $d, $d, 0, 90)
    $p.AddArc($x, $y + $h - $d, $d, $d, 90, 90)
    $p.CloseFigure()
    return $p
}
function Pen([string]$h, [single]$w) {
    $p = New-Object System.Drawing.Pen (Hex $h), $w
    $p.LineJoin = 'Round'; $p.StartCap = 'Round'; $p.EndCap = 'Round'
    return $p
}
function Tile($g, $path, [string]$c1, [string]$c2) {
    if ($c1 -eq $c2) { $b = Brush $c1 }
    else { $b = [System.Drawing.Drawing2D.LinearGradientBrush]::new([System.Drawing.Point]::new(0, 24), [System.Drawing.Point]::new(0, 1000), (Hex $c1), (Hex $c2)) }
    $g.FillPath($b, $path)
}

# All drawing is in a 1024 x 1024 space.
function Draw-A($g, [bool]$on) {
    if ($on) { $c1 = '#38C777'; $c2 = '#15924A'; $ink = '#0C5C2E' } else { $c1 = '#3C86F5'; $c2 = '#1B55CC'; $ink = '#1B55CC' }
    Tile $g (RR 24 24 976 976 220) $c1 $c2
    $white = Brush '#FFFFFF'
    # Two clamp brackets: vertical bar plus top and bottom arms reaching inward.
    foreach ($side in 'L', 'R') {
        $bp = New-Object System.Drawing.Drawing2D.GraphicsPath
        if ($side -eq 'L') { $x = 96; $arm = 96 + 190 } else { $x = 1024 - 96 - 84; $arm = 1024 - 96 - 190 }
        $bar = RR $x 250 84 524 34
        $g.FillPath($white, $bar)
        if ($side -eq 'L') { $ax = $x; $aw = 190 } else { $ax = 1024 - 96 - 190; $aw = 190 }
        $g.FillPath($white, (RR $ax 250 $aw 84 34))
        $g.FillPath($white, (RR $ax 690 $aw 84 34))
    }
    # The held stick, vertical in the middle.
    $sx = 372; $sy = 262; $sw = 280; $sh = 500
    if ($on) {
        $g.FillPath($white, (RR $sx $sy $sw $sh 40))
        $dark = Brush $ink
        $g.FillPath($dark, (RR ($sx + 60) ($sy + 70) 160 130 20))
        $g.FillPath($dark, (RR ($sx + 60) ($sy + 250) 160 130 20))
    } else {
        $pen = Pen '#FFFFFF' 46
        $g.DrawPath($pen, (RR ($sx + 23) ($sy + 23) ($sw - 46) ($sh - 46) 30))
        $g.FillPath($white, (RR ($sx + 90) ($sy + 110) 100 90 14))
        $g.FillPath($white, (RR ($sx + 90) ($sy + 290) 100 90 14))
    }
}

function Draw-B($g, [bool]$on) {
    if ($on) { $c1 = '#3FCB7D'; $c2 = '#17934B'; $ink = '#0F6A35' } else { $c1 = '#17B3B8'; $c2 = '#0A6F9E'; $ink = '#0A6F9E' }
    # squircle-ish tile (large radius)
    Tile $g (RR 24 24 976 976 300) $c1 $c2
    $white = Brush '#FFFFFF'
    $dark = Brush $ink
    # stick body
    $g.FillPath($white, (RR 130 250 764 430 36))
    # pins in two groups (3 + 4) split by the off-centre RAM key gap, so they read as contacts, not teeth
    $pw = 62; $gap = 26
    $x = 168
    foreach ($grp in @(3, 4)) {
        for ($i = 0; $i -lt $grp; $i++) { $g.FillRectangle($white, [single]$x, 680, $pw, 100); $x += $pw + $gap }
        $x += 70
    }    # pause glyph: two bars. ON = solid, normal = hollow outlines.
    $bw = 120; $bh = 270; $by = 325
    foreach ($bx in 316, 588) {
        if ($on) { $g.FillPath($dark, (RR $bx $by $bw $bh 26)) }
        else {
            $pen = Pen $ink 46
            $g.DrawPath($pen, (RR ($bx + 23) ($by + 23) ($bw - 46) ($bh - 46) 10))
        }
    }
}

function Draw-C($g, [bool]$on) {
    if ($on) { $tile = '#1FA85A'; $acc = '#FFFFFF' } else { $tile = '#12386B'; $acc = '#4FD6D0' }
    Tile $g (RR 24 24 976 976 200) $tile $tile
    $pen = Pen '#FFFFFF' 70
    # chip outline
    $g.DrawPath($pen, (RR 175 295 674 430 70))
    # notch at the bottom edge: tile-coloured disc over the outline, then the notch rim
    $g.FillEllipse((Brush $tile), 432, 612, 160, 160)
    $g.FillRectangle((Brush $tile), 432, 700, 160, 100)
    $g.DrawArc($pen, 442, 622, 140, 140, 0, 180)
    # accent bar: short when idle, full when on
    $len = if ($on) { 500 } else { 230 }
    $g.FillPath((Brush $acc), (RR 262 440 $len 100 50))
}

# A2: same stick in both states. Normal: clamps open with a visible gap. ON: clamps closed in, touching the stick.
# $bold = thicker brackets/outline and a wider open gap, used for renders of 24 px and below.
function Draw-A2($g, [bool]$on, [bool]$bold) {
    if ($on) { $c1 = '#38C777'; $c2 = '#15924A'; $ink = '#0C5C2E' } else { $c1 = '#3C86F5'; $c2 = '#1B55CC'; $ink = '#1B55CC' }
    Tile $g (RR 24 24 976 976 220) $c1 $c2
    $white = Brush '#FFFFFF'
    $sx = 372; $sy = 262; $sw = 280; $sh = 500
    if ($bold) { $t = 112; $arm = 184; $out = 70; $move = 138; $pw = 66 } else { $t = 84; $arm = 190; $out = 96; $move = 96; $pw = 46 }
    $shift = if ($on) { $move } else { 0 }
    # brackets: vertical bar plus top and bottom arms reaching toward the stick
    $lx = $out + $shift
    $g.FillPath($white, (RR $lx 250 $t 524 34))
    $g.FillPath($white, (RR $lx 250 $arm $t 34))
    $g.FillPath($white, (RR $lx (774 - $t) $arm $t 34))
    $rx = 1024 - $out - $shift - $t
    $g.FillPath($white, (RR $rx 250 $t 524 34))
    $g.FillPath($white, (RR ($rx + $t - $arm) 250 $arm $t 34))
    $g.FillPath($white, (RR ($rx + $t - $arm) (774 - $t) $arm $t 34))
    # the stick: identical in both states (outline with two chips)
    $pen = Pen '#FFFFFF' $pw
    $h = $pw / 2
    $g.DrawPath($pen, (RR ($sx + $h) ($sy + $h) ($sw - $pw) ($sh - $pw) 30))
    $g.FillPath($white, (RR ($sx + 90) ($sy + 110) 100 90 14))
    $g.FillPath($white, (RR ($sx + 90) ($sy + 290) 100 90 14))
}
# A3: like A2, but the stick is ~10% smaller in BOTH states and the closed brackets move in until their arms
# lie over the stick's face (inner edge clearly past the stick edge). A thin tile-coloured halo around each
# bracket separates it from the stick so the overlap reads as a bite, not a merge.
function Draw-A3($g, [bool]$on, [bool]$bold) {
    if ($on) { $c1 = '#38C777'; $c2 = '#15924A'; $ink = '#0C5C2E'; $halo = '#1A9E55' } else { $c1 = '#3C86F5'; $c2 = '#1B55CC'; $ink = '#1B55CC'; $halo = '#2F6EE0' }
    Tile $g (RR 24 24 976 976 220) $c1 $c2
    $white = Brush '#FFFFFF'
    $sw = 252; $sh = 450; $sx = 512 - $sw / 2; $sy = 512 - $sh / 2
    if ($bold) { $t = 112; $arm = 184; $out = 70; $move = 160; $pw = 54; $hw = 24 } else { $t = 84; $arm = 190; $out = 96; $move = 146; $pw = 46; $hw = 30 }
    # the stick first
    $pen = Pen '#FFFFFF' $pw
    $h = $pw / 2
    $g.DrawPath($pen, (RR ($sx + $h) ($sy + $h) ($sw - $pw) ($sh - $pw) 28))
    $cw = 90; $ch = 80; $off = 100
    if ($bold) { $cw = 100; $ch = 92; $off = 84 }
    $cx = $sx + ($sw - $cw) / 2
    $g.FillPath($white, (RR $cx ($sy + $off) $cw $ch 14))
    $g.FillPath($white, (RR $cx ($sy + $sh - $off - $ch) $cw $ch 14))
    # brackets on top, each with a halo
    $shift = if ($on) { $move } else { 0 }
    $hp = Pen $halo $hw
    $lx = $out + $shift
    $rx = 1024 - $out - $shift - $t
    $paths = @(
        (RR $lx 250 $t 524 34), (RR $lx 250 $arm $t 34), (RR $lx (774 - $t) $arm $t 34),
        (RR $rx 250 $t 524 34), (RR ($rx + $t - $arm) 250 $arm $t 34), (RR ($rx + $t - $arm) (774 - $t) $arm $t 34)
    )
    # one halo pass per bracket (bar+arms together), then fills
    foreach ($grp in @(@(0, 1, 2), @(3, 4, 5))) {
        foreach ($i in $grp) { $g.DrawPath($hp, $paths[$i]) }
        foreach ($i in $grp) { $g.FillPath($white, $paths[$i]) }
    }
}
function New-Master([string]$concept, [bool]$on, [bool]$bold = $false) {
    $S = 1024
    $bmp = New-Object System.Drawing.Bitmap $S, $S, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)
    switch ($concept) { 'A' { Draw-A $g $on } 'A2' { Draw-A2 $g $on $bold } 'A3' { Draw-A3 $g $on $bold } 'B' { Draw-B $g $on } 'C' { Draw-C $g $on } }
    $g.Dispose()
    return $bmp
}

# High quality downscale by repeated halving, so tiny sizes stay crisp.
function Scale-Bitmap([System.Drawing.Bitmap]$src, [int]$size) {
    $cur = $src
    while ($cur.Width -gt $size) {
        $next = [Math]::Max($size, [int]($cur.Width / 2))
        $b = New-Object System.Drawing.Bitmap $next, $next, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
        $g = [System.Drawing.Graphics]::FromImage($b)
        $g.InterpolationMode = 'HighQualityBicubic'; $g.SmoothingMode = 'HighQuality'
        $g.PixelOffsetMode = 'HighQuality'; $g.CompositingQuality = 'HighQuality'
        $g.DrawImage($cur, (New-Object System.Drawing.Rectangle 0, 0, $next, $next), 0, 0, $cur.Width, $cur.Height, [System.Drawing.GraphicsUnit]::Pixel)
        $g.Dispose()
        if ($cur -ne $src) { $cur.Dispose() }
        $cur = $b
    }
    return $cur
}
function Png-Bytes([System.Drawing.Bitmap]$bmp) {
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    return ,$ms.ToArray()
}
function Sized([System.Drawing.Bitmap]$master, [int]$size) {
    if ($size -eq $master.Width) { return $master.Clone() }
    return (Scale-Bitmap $master $size)
}

# Render one size, using the bold master for small sizes of A2.
$script:mcache = @{}
function Render-Size([string]$concept, [bool]$on, [int]$size) {
    $bold = ($concept -in 'A2', 'A3' -and $size -le 24)
    $key = "$concept-$on-$bold"
    if (-not $script:mcache.ContainsKey($key)) { $script:mcache[$key] = New-Master $concept $on $bold }
    return (Sized $script:mcache[$key] $size)
}

if ($Preview) {
    $pdir = Join-Path $root 'docs\icon-concepts'
    New-Item -ItemType Directory -Force $pdir | Out-Null
    $sizes = 16, 20, 24, 32, 48, 64, 128, 256
    $cache = @{}
    foreach ($c in 'A', 'B', 'C') {
        foreach ($on in $false, $true) {
            $m = New-Master $c $on
            $tag = if ($on) { "$c-on" } else { $c }
            foreach ($s in $sizes) {
                $b = Sized $m $s
                $cache["$tag-$s"] = $b
                $b.Save((Join-Path $pdir "concept-$tag-$s.png"), [System.Drawing.Imaging.ImageFormat]::Png)
            }
            $m.Dispose()
        }
    }
    # Contact sheet: two panels (dark/light), a row per concept, cells = normal then ON at 256, 64, 32, 16.
    $show = 256, 64, 32, 16
    $pad = 24
    $cellW = ($show | Measure-Object -Sum).Sum + $pad * ($show.Count + 1)
    $panelW = $cellW * 2 + 40
    $rowH = 256 + $pad * 2
    $W = $panelW * 2; $H = $rowH * 3
    $sheet = New-Object System.Drawing.Bitmap ([int]$W), ([int]$H)
    $g = [System.Drawing.Graphics]::FromImage($sheet)
    $g.InterpolationMode = 'NearestNeighbor'
    $bgs = '#1e1e1e', '#f3f3f3'
    for ($pi = 0; $pi -lt 2; $pi++) {
        $g.FillRectangle((Brush $bgs[$pi]), $pi * $panelW, 0, $panelW, $H)
        $ci = 0
        foreach ($c in 'A', 'B', 'C') {
            $x = $pi * $panelW + 10
            foreach ($on in $false, $true) {
                $tag = if ($on) { "$c-on" } else { $c }
                $x += $pad
                foreach ($s in $show) {
                    $y = $ci * $rowH + $pad + (256 - $s) / 2
                    $g.DrawImage($cache["$tag-$s"], [int]$x, [int]$y, $s, $s)
                    $x += $s + $pad
                }
                if (-not $on) { $x += 10 }
            }
            $ci++
        }
    }
    $g.Dispose()
    $sheet.Save((Join-Path $pdir 'sheet.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    $sheet.Dispose()
    # Zoom sheet: 16 and 20 px at 6x (nearest neighbour) next to true size, to judge pixel detail.
    $zs = 16, 20
    $zw = 2 * ((16 * 6 + 20 * 6) + 16 + 20 + 5 * 12) + 20
    $zh = 3 * (20 * 6 + 24)
    $zoom = New-Object System.Drawing.Bitmap ([int]($zw * 2)), ([int]$zh)
    $g = [System.Drawing.Graphics]::FromImage($zoom)
    $g.InterpolationMode = 'NearestNeighbor'; $g.PixelOffsetMode = 'Half'
    for ($pi = 0; $pi -lt 2; $pi++) {
        $g.FillRectangle((Brush $bgs[$pi]), $pi * $zw, 0, $zw, $zh)
        $ci = 0
        foreach ($c in 'A', 'B', 'C') {
            $x = $pi * $zw + 12
            $y = $ci * (20 * 6 + 24) + 12
            foreach ($on in $false, $true) {
                $tag = if ($on) { "$c-on" } else { $c }
                foreach ($s in $zs) {
                    $g.DrawImage($cache["$tag-$s"], $x, $y, $s * 6, $s * 6); $x += $s * 6 + 12
                    $g.DrawImage($cache["$tag-$s"], $x, $y, $s, $s); $x += $s + 12
                }
            }
            $ci++
        }
    }
    $g.Dispose()
    $zoom.Save((Join-Path $pdir 'sheet-zoom.png'), [System.Drawing.Imaging.ImageFormat]::Png)
    $zoom.Dispose()
    Write-Host "Preview written to $pdir (sheet.png, sheet-zoom.png, concept-*.png)"
    foreach ($cn in 'A2', 'A3') {
    # A2 sheets: normal and ON side by side at 256/128/64/32/20/16 on dark and light, plus a 6x true-pixel strip.
    $show2 = 256, 128, 64, 32, 20, 16
    $a2c = @{}
    foreach ($on in $false, $true) { foreach ($sz in $show2) {
        $b = Render-Size $cn $on $sz; $a2c["$on-$sz"] = $b
        $tag = if ($on) { "$cn-on" } else { $cn }
        $b.Save((Join-Path $pdir "concept-$tag-$sz.png"), [System.Drawing.Imaging.ImageFormat]::Png)
    } }
    $pad = 24
    $cell = ($show2 | Measure-Object -Sum).Sum + $pad * ($show2.Count + 1)
    $panel = $cell * 2 + 30
    $sh2 = New-Object System.Drawing.Bitmap ([int]($panel * 2)), ([int](256 + $pad * 2))
    $g = [System.Drawing.Graphics]::FromImage($sh2); $g.InterpolationMode = 'NearestNeighbor'
    for ($pi = 0; $pi -lt 2; $pi++) {
        $g.FillRectangle((Brush $bgs[$pi]), $pi * $panel, 0, $panel, $sh2.Height)
        $x = $pi * $panel + 10
        foreach ($on in $false, $true) {
            $x += $pad
            foreach ($sz in $show2) {
                $g.DrawImage($a2c["$on-$sz"], [int]$x, [int](($sh2.Height - $sz) / 2), $sz, $sz); $x += $sz + $pad
            }
            $x += 10
        }
    }
    $g.Dispose(); $sh2.Save((Join-Path $pdir "sheet-$cn.png"), [System.Drawing.Imaging.ImageFormat]::Png); $sh2.Dispose()
    $z = 6; $zs2 = 16, 20
    $zcell = 2 * (($zs2 | Measure-Object -Sum).Sum * ($z + 1) + 12 * 4) + 24
    $zb = New-Object System.Drawing.Bitmap ([int]($zcell * 2)), ([int](20 * $z + 40))
    $g = [System.Drawing.Graphics]::FromImage($zb); $g.InterpolationMode = 'NearestNeighbor'; $g.PixelOffsetMode = 'Half'
    for ($pi = 0; $pi -lt 2; $pi++) {
        $g.FillRectangle((Brush $bgs[$pi]), $pi * $zcell, 0, $zcell, $zb.Height)
        $x = $pi * $zcell + 16
        foreach ($on in $false, $true) { foreach ($sz in $zs2) {
            $g.DrawImage($a2c["$on-$sz"], $x, 20, $sz * $z, $sz * $z); $x += $sz * $z + 12
            $g.DrawImage($a2c["$on-$sz"], $x, 20, $sz, $sz); $x += $sz + 12
        } }
    }
    $g.Dispose(); $zb.Save((Join-Path $pdir "sheet-$cn-zoom.png"), [System.Drawing.Imaging.ImageFormat]::Png); $zb.Dispose()
    }
    Write-Host "A2/A3 sheets written"
    exit 0
}

# Final output for the chosen concept.
New-Item -ItemType Directory -Force $OutDir | Out-Null
$sizes = 16, 20, 24, 32, 48, 64, 128, 256
$imgs = foreach ($s in $sizes) { $b = Render-Size $Concept $false $s; ,(Png-Bytes $b) }
$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter $ms
$bw.Write([uint16]0); $bw.Write([uint16]1); $bw.Write([uint16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([uint16]1); $bw.Write([uint16]32)
    $bw.Write([uint32]$imgs[$i].Length); $bw.Write([uint32]$offset)
    $offset += $imgs[$i].Length
}
foreach ($b in $imgs) { $bw.Write($b) }
$bw.Flush()
[System.IO.File]::WriteAllBytes((Join-Path $OutDir 'icon.ico'), $ms.ToArray())
foreach ($spec in @(@('32x32.png', $false, 32), @('32x32-on.png', $true, 32), @('128x128.png', $false, 128), @('icon.png', $false, 256))) {
    $b = Render-Size $Concept $spec[1] $spec[2]
    [System.IO.File]::WriteAllBytes((Join-Path $OutDir $spec[0]), (Png-Bytes $b))
}
Write-Host "Concept $Concept icons written to $OutDir"
