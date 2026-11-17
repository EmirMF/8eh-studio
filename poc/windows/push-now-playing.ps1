# 8EH Stage 1 PoC - Push metadata "Now Playing" ke API situs (widget)
# Endpoint: POST /api/studio/now-playing dengan X-Device-Token.
# Membaca metadata dari metadata-watcher.ps1 (pipeline) atau argumen manual,
# lalu POST ke situs. Bukan bagian dari streaming AzuraCast (out of scope).
#
# Pakai:
#   .\push-now-playing.ps1 -Title "Judul" -Artist "Artis"                 # manual
#   .\push-now-playing.ps1 -Program "Talkshow Sore"                        # mode program
#   .\push-now-playing.ps1 -Clear                                           # kosongkan
#   .\push-now-playing.ps1 -FollowSpotify                                   # loop ikut Spotify
#
# Site URL & token dibaca dari .env situs (8ehradioitb):
#   NEXT_PUBLIC_SITE_URL / NEXTAUTH_URL + STUDIO_DEVICE_TOKEN

param(
  [string]$SiteUrl = "",
  [string]$Token = "",
  [string]$Title = "",
  [string]$Artist = "",
  [string]$Program = "",
  [string]$Source = "",
  [switch]$Clear,
  [switch]$FollowSpotify
)

$ErrorActionPreference = "Stop"

$envFile = "C:\Users\Meong\Documents\8ehradioitb\.env"
foreach ($line in Get-Content $envFile) {
  if ($line -match '^(NEXT_PUBLIC_SITE_URL|NEXTAUTH_URL|STUDIO_DEVICE_TOKEN)=(.*)$') {
    $rawVal = $Matches[2].Trim('"')  # .env situs memakai kutip pada beberapa nilai
    switch ($Matches[1]) {
      "NEXT_PUBLIC_SITE_URL" { if (-not $SiteUrl) { $SiteUrl = $rawVal } }
      "NEXTAUTH_URL"         { if (-not $SiteUrl) { $SiteUrl = $rawVal } }
      "STUDIO_DEVICE_TOKEN"  { if (-not $Token)   { $Token = $rawVal } }
    }
  }
}

if (-not $SiteUrl) { Write-Host "GAGAL: SiteUrl kosong (cek NEXT_PUBLIC_SITE_URL di .env)"; exit 1 }
if (-not $Token)   { Write-Host "GAGAL: token kosong (cek STUDIO_DEVICE_TOKEN di .env)"; exit 1 }
if ($SiteUrl -notmatch '^https?://') { $SiteUrl = "https://$SiteUrl" }
$uri = "$SiteUrl/api/studio/now-playing"

function Push($body, $label) {
  try {
    $resp = Invoke-RestMethod -Uri $uri -Method Post -ContentType "application/json" `
      -Headers @{ "X-Device-Token" = $Token } -Body ($body | ConvertTo-Json -Compress) -TimeoutSec 15
    $line = if ($resp.title) { "$($resp.artist) - $($resp.title)" } else { "(cleared)" }
    Write-Host "OK [$label]: $line"
  } catch {
    Write-Host "GAGAL [$label]: $($_.Exception.Message)"
  }
}

if ($Clear) {
  Push @{ mode = "clear" } "clear"
  return
}

if ($Program) {
  Push @{ mode = "program"; title = $Program } "program"
  return
}

if ($FollowSpotify) {
  Write-Host "Mengikuti Spotify... (Ctrl+C untuk berhenti)"
  $watcher = Join-Path $PSScriptRoot "metadata-watcher.ps1"
  # -Filter spotify + loop internal watcher; parse JSON per baris
  $last = ""
  & powershell -NoProfile -ExecutionPolicy Bypass -File $watcher -Filter "spotify" | ForEach-Object {
    try {
      $snap = $_ | ConvertFrom-Json
    } catch { return }
    $s = $snap.sessions | Where-Object { $_.title } | Select-Object -First 1
    if (-not $s) { return }
    $key = "$($s.title)|$($s.artist)"
    if ($key -eq $last) { return }
    $last = $key
    Push @{ mode = "auto"; title = $s.title; artist = $s.artist; playbackState = $s.playbackState } "spotify"
  }
  return
}

if (-not $Title) { Write-Host "Pakai -Title, -Program, -Clear, atau -FollowSpotify"; exit 1 }
Push @{ mode = "auto"; title = $Title; artist = $Artist } "manual"
