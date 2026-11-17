# 8EH Stage 1 PoC - Uji capture audio input (pipeline FFmpeg dshow)
# Di studio: jalankan dengan -Device "<nama MG-XU dari listing>"
# (mis. "MG-XU" sesuai Yamaha Steinberg USB Driver), lalu mainkan musik dari
# mixer dan cek level dBFS pada hasil volumedetect.
#
# Pakai:
#   .\test-audio-capture.ps1                                   # list device saja
#   .\test-audio-capture.ps1 -Device "MG-XU"                   # list + capture 5s
#   .\test-audio-capture.ps1 -Device "Microphone Array (Realtek(R) Audio)" -Seconds 3

param(
  [string]$Device = "",
  [int]$Seconds = 5,
  [string]$OutDir = "$PSScriptRoot\..\out"
)

$ErrorActionPreference = "Continue"
$ffmpeg = (Get-Command ffmpeg -ErrorAction Stop).Source
if (-not (Test-Path $OutDir)) { New-Item -ItemType Directory -Path $OutDir | Out-Null }

Write-Host "== Audio input devices =="
& $ffmpeg -hide_banner -list_devices true -f dshow -i dummy 2>&1 |
  Where-Object { $_.ToString() -match '\(audio\)|Alternative name' } |
  ForEach-Object { $_.ToString() }

if (-not $Device) {
  Write-Host "`nBelum ada -Device. Jalankan lagi dengan nama device dari listing di atas."
  Write-Host "Di studio, cari device berisi 'MG-XU' (Yamaha Steinberg USB Driver)."
  return
}

$wav = Join-Path $OutDir "capture-test.wav"
Write-Host "`n== Capture $Seconds detik dari '$Device' =="
& $ffmpeg -hide_banner -y -f dshow -i "audio=$Device" -t $Seconds -ac 2 -ar 44100 $wav 2>&1 |
  Select-Object -Last 4 | ForEach-Object { $_.ToString() }

if (-not (Test-Path $wav)) {
  Write-Host "GAGAL: file output tidak dibuat - device mungkin tidak bisa dibuka."
  exit 1
}

Write-Host "`n== Analisis level (dBFS) =="
& $ffmpeg -hide_banner -i $wav -af volumedetect -f null NUL 2>&1 |
  Where-Object { $_.ToString() -match 'mean_volume|max_volume|Duration' } |
  ForEach-Object { $_.ToString() }

Write-Host "`nOK. Output: $wav"
Write-Host "Interpretasi: mean_volume sekitar -20 s/d -6 dBFS = level sehat."
Write-Host "max_volume 0 dB berulang = clipping; turunkan gain di mixer."
