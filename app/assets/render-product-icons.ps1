param(
    [Parameter(Mandatory)]
    [string]$DesignSystem,
    [string]$Browser
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
$renderer = Join-Path $DesignSystem 'scripts/product_icon.py'
$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$assets = 'snipscreen-icon.svg', 'snipscreen-icon-small.svg', 'snipscreen-icon.png', 'snipscreen-icon-256.png', 'snipscreen.ico'
$scratch = New-Item -ItemType Directory -Path ([IO.Path]::Combine([IO.Path]::GetTempPath(), [guid]::NewGuid().ToString('N')))

function Resize-Icon($source, [int]$size, [string]$path) {
    $bitmap = [Drawing.Bitmap]::new($size, $size, [Drawing.Imaging.PixelFormat]::Format32bppArgb)
    try {
        $graphics = [Drawing.Graphics]::FromImage($bitmap)
        try {
            $graphics.CompositingMode = [Drawing.Drawing2D.CompositingMode]::SourceCopy
            $graphics.InterpolationMode = [Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
            $graphics.PixelOffsetMode = [Drawing.Drawing2D.PixelOffsetMode]::Half
            $graphics.DrawImage($source, 0, 0, $size, $size)
        } finally {
            $graphics.Dispose()
        }
        $bitmap.Save($path, [Drawing.Imaging.ImageFormat]::Png)
    } finally {
        $bitmap.Dispose()
    }
}

try {
    $browserOption = if ($Browser) { @('--browser', $Browser) }
    python -I $renderer (Join-Path $PSScriptRoot 'product-icons.json') --out $scratch.FullName --png @browserOption
    if ($LASTEXITCODE -ne 0) { throw 'The Teo product icon renderer failed.' }

    $full = [Drawing.Bitmap]::new((Join-Path $scratch.FullName 'snipscreen-icon.png'))
    try {
        $small = [Drawing.Bitmap]::new((Join-Path $scratch.FullName 'snipscreen-icon-small.png'))
        try {
            $frames = foreach ($size in $sizes) {
                $source = if ($size -le 24) { $small } else { $full }
                $path = Join-Path $scratch.FullName "snipscreen-icon-$size.png"
                Resize-Icon $source $size $path
                @{ Size = $size; Png = [IO.File]::ReadAllBytes($path) }
            }
        } finally {
            $small.Dispose()
        }
    } finally {
        $full.Dispose()
    }

    $ico = [IO.BinaryWriter]::new([IO.File]::Create((Join-Path $scratch.FullName 'snipscreen.ico')))
    try {
        $ico.Write([uint16]0)
        $ico.Write([uint16]1)
        $ico.Write([uint16]$frames.Count)
        $offset = 6 + 16 * $frames.Count
        foreach ($frame in $frames) {
            $dimension = [byte]($frame.Size % 256) # ICO uses zero for 256 px.
            $ico.Write($dimension)
            $ico.Write($dimension)
            $ico.Write([uint16]0)
            $ico.Write([uint16]1)
            $ico.Write([uint16]32)
            $ico.Write([uint32]$frame.Png.Length)
            $ico.Write([uint32]$offset)
            $offset += $frame.Png.Length
        }
        foreach ($frame in $frames) { $ico.Write($frame.Png) }
    } finally {
        $ico.Dispose()
    }

    foreach ($file in $assets) {
        Copy-Item -LiteralPath (Join-Path $scratch.FullName $file) -Destination $PSScriptRoot
    }
} finally {
    # Only remove the files created by this run and its newly created directory.
    $ownedFiles = $assets + 'snipscreen-icon-small.png' + ($sizes | ForEach-Object { "snipscreen-icon-$_.png" })
    foreach ($file in $ownedFiles) {
        $path = Join-Path $scratch.FullName $file
        if (Test-Path -LiteralPath $path -PathType Leaf) { Remove-Item -LiteralPath $path }
    }
    if (-not (Get-ChildItem -LiteralPath $scratch.FullName -Force)) { Remove-Item -LiteralPath $scratch.FullName }
}
