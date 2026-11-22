# ICO valid: header ICONDIR + entry + payload PNG 32x32 (didukung Vista+)
$dir = Join-Path $PSScriptRoot 'src-tauri\icons'
$png = [System.IO.File]::ReadAllBytes("$dir\32x32.png")

$ms = New-Object System.IO.MemoryStream
$bw = New-Object System.IO.BinaryWriter($ms)
$bw.Write([uint16]0)      # reserved
$bw.Write([uint16]1)      # type: icon
$bw.Write([uint16]1)      # count
$bw.Write([byte]32)       # width
$bw.Write([byte]32)       # height
$bw.Write([byte]0)        # palette
$bw.Write([byte]0)        # reserved
$bw.Write([uint16]1)      # planes
$bw.Write([uint16]32)     # bpp
$bw.Write([uint32]$png.Length)
$bw.Write([uint32]22)     # offset data setelah header 6+16
$bw.Write($png)
$bw.Flush()
[System.IO.File]::WriteAllBytes("$dir\icon.ico", $ms.ToArray())
$bw.Close()
"ico ditulis: $((Get-Item "$dir\icon.ico").Length) bytes"
