# 8EH Stage 1 PoC - Windows metadata watcher (SMTC)
# Membaca media session OS (Spotify, Chrome, dsb.) via
# GlobalSystemMediaTransportControlsSessionManager (Windows 10+).
# Harus dijalankan dengan Windows PowerShell 5.1 (bukan pwsh 7 tanpa runtime WinRT).
#
# Pakai:
#   .\metadata-watcher.ps1 -Once                # satu poll, lalu keluar
#   .\metadata-watcher.ps1 -Filter spotify      # hanya source yang cocok
#   .\metadata-watcher.ps1                      # loop, print JSON saat berubah
#
# Output: satu baris JSON per perubahan:
#   {"observedAt":"...ISO-8601...","sessions":[{"source":"Spotify.exe","title":"...","artist":"...","playbackState":"Playing"}]}

param(
  [string]$Filter = "",
  [switch]$Once,
  [int]$IntervalMs = 1000
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Runtime.WindowsRuntime

# shortcut: WinRT projection butuh registrasi eksplisit di PS 5.1
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]

# Await untuk IAsyncOperation<T> (PS 5.1 tidak bisa await WinRT langsung)
$asTaskGeneric = ([System.WindowsRuntimeSystemExtensions].GetMethods() |
  Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
  })[0]

function Await($WinRtTask, $ResultType) {
  $asTask = $asTaskGeneric.MakeGenericMethod($ResultType)
  $netTask = $asTask.Invoke($null, @($WinRtTask))
  $netTask.Wait(-1) | Out-Null
  $netTask.Result
}

function Get-MediaSessions {
  $manager = Await ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) (
    [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])

  foreach ($session in $manager.GetSessions()) {
    $props = Await ($session.TryGetMediaPropertiesAsync()) (
      [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])

    # shortcut: PlaybackInfo null via proyeksi WinRT PS 5.1; status diinferensi
    # dari posisi timeline (maju = playing). Di app Tauri (windows-rs) tidak perlu.
    $state = ""
    if ($session.PlaybackInfo) {
      $state = [string]$session.PlaybackInfo.PlaybackStatus
    } else {
      $pos1 = $session.GetTimelineProperties().Position
      Start-Sleep -Milliseconds 300
      $pos2 = $session.GetTimelineProperties().Position
      $state = if ($pos2 -gt $pos1) { "Playing" } else { "Paused" }
    }

    [pscustomobject]@{
      source        = [string]$session.SourceAppUserModelId
      title         = [string]$props.Title
      artist        = [string]$props.Artist
      playbackState = $state
    }
  }
}

$last = $null
do {
  $sessions = @(Get-MediaSessions)
  if ($Filter) {
    $sessions = @($sessions | Where-Object { $_.source -like "*$Filter*" })
  }

  # rakit JSON sendiri: ConvertTo-Json PS 5.1 melebur array 1 elemen
  $entries = @($sessions | ForEach-Object { $_ | ConvertTo-Json -Compress })
  $stamp = (Get-Date).ToUniversalTime().ToString("o")
  $snap = '{"observedAt":"' + $stamp + '","sessions":[' + ($entries -join ',') + ']}'

  if ($snap -ne $last) {
    $last = $snap
    Write-Output $snap
  }

  if (-not $Once) { Start-Sleep -Milliseconds $IntervalMs }
} while (-not $Once)
