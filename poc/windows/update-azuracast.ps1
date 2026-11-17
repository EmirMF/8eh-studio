# 8EH Stage 1 PoC - Update metadata "Now Playing" di AzuraCast
# Endpoint terkonfirmasi: POST /api/station/{id}/nowplaying/update
# Body: {"title": "...", "artist": "..."} + header X-API-Key.
#
# Default kredensial dibaca dari .env situs (8ehradioitb). Tanpa -ConfirmLive
# hanya DRY RUN: request dibangun & dicetak, TIDAK dikirim (metadata live
# yang dilihat pendengar akan berubah jika benar-benar dikirim).
#
# Pakai:
#   .\update-azuracast.ps1 -Title "Uji Coba" -Artist "8EH"          # dry run
#   .\update-azuracast.ps1 -Title "Uji Coba" -Artist "8EH" -ConfirmLive

param(
  [Parameter(Mandatory = $true)][string]$Title,
  [string]$Artist = "",
  [string]$BaseUrl = "",
  [string]$StationId = "",
  [string]$ApiKey = "",
  [switch]$ConfirmLive
)

$ErrorActionPreference = "Stop"

# fallback .env situs, urutan: relatif dari skrip ini lalu path absolut
$envCandidates = @(
  (Join-Path $PSScriptRoot "..\..\8ehradioitb\.env"),
  "C:\Users\Meong\Documents\8ehradioitb\.env"
)
$envFile = $envCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if ($envFile) {
  foreach ($line in Get-Content $envFile) {
    if ($line -match '^(AZURACAST_BASE_URL|AZURACAST_STATION_ID|AZURACAST_API_KEY)=(.*)$') {
      switch ($Matches[1]) {
        "AZURACAST_BASE_URL"   { if (-not $BaseUrl)   { $BaseUrl = $Matches[2] } }
        "AZURACAST_STATION_ID" { if (-not $StationId) { $StationId = $Matches[2] } }
        "AZURACAST_API_KEY"    { if (-not $ApiKey)    { $ApiKey = $Matches[2] } }
      }
    }
  }
}

foreach ($pair in @(@("BaseUrl", $BaseUrl), @("StationId", $StationId), @("ApiKey", $ApiKey))) {
  if (-not $pair[1]) {
    Write-Host "GAGAL: $($pair[0]) kosong dan tidak ditemukan di .env ($envFile)."
    exit 1
  }
}

if ($BaseUrl -notmatch '^https?://') { $BaseUrl = "https://$BaseUrl" }
$uri = "$BaseUrl/api/station/$StationId/nowplaying/update"
$body = @{ title = $Title; artist = $Artist } | ConvertTo-Json -Compress

if (-not $ConfirmLive) {
  Write-Host "DRY RUN - request TIDAK dikirim. Tambahkan -ConfirmLive untuk benar-benar POST."
  Write-Host "POST $uri"
  Write-Host "X-API-Key: <disembunyikan>"
  Write-Host "Body: $body"
  return
}

Write-Host "Mengirim POST ke $uri ..."
try {
  $resp = Invoke-RestMethod -Uri $uri -Method Post -ContentType "application/json" `
    -Headers @{ "X-API-Key" = $ApiKey } -Body $body -TimeoutSec 15
  Write-Host "RESPONSE:" ($resp | ConvertTo-Json -Compress)
  Write-Host "Sukses. Verifikasi publik: GET $BaseUrl/api/nowplaying/$StationId"
} catch {
  Write-Host "GAGAL: $($_.Exception.Message)"
  exit 1
}
