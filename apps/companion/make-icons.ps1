# Generate icon companion sederhana (lingkaran merah 8EH) - dijalankan sekali
Add-Type -AssemblyName System.Drawing

function Make-Png($size, $path) {
  $bmp = New-Object System.Drawing.Bitmap($size, $size)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.SmoothingMode = "AntiAlias"
  $g.Clear([System.Drawing.Color]::Transparent)
  $brush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(216, 50, 50))
  $g.FillEllipse($brush, 0, 0, $size - 1, $size - 1)
  $font = New-Object System.Drawing.Font("Segoe UI", [float]($size * 0.45), [System.Drawing.FontStyle]::Bold)
  $fmt = New-Object System.Drawing.StringFormat
  $fmt.Alignment = "Center"; $fmt.LineAlignment = "Center"
  $g.DrawString("8E", $font, [System.Drawing.Brushes]::White, (New-Object System.Drawing.RectangleF(0, 0, $size, $size)), $fmt)
  $g.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
}

$dir = Join-Path $PSScriptRoot 'src-tauri\icons'
New-Item -ItemType Directory -Path $dir -Force | Out-Null
Make-Png 32 "$dir\32x32.png"
Make-Png 128 "$dir\128x128.png"

# ICO dari PNG 32
$png = [System.Drawing.Image]::FromFile("$dir\32x32.png")
$png.Save("$dir\icon.ico", [System.Drawing.Imaging.ImageFormat]::Icon)
$png.Dispose()
"icons dibuat: " + (Get-ChildItem $dir | ForEach-Object { $_.Name } | Join-String -Separator ", ")
