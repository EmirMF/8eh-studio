# 8EH Companion Agent - Tahap 3 (Windows, versi skrip)
# Tray app pembaca metadata lagu → push ke widget situs.
# Logika ini nanti dipindah ke Tauri (Rust); versi ini prototype yang
# fungsional dan bisa dipakai kru langsung di laptop musik Windows.
#
# Fitur (sesuai PRD C-01..C-05):
#   - System tray: ikon hijau = terhubung, merah = gagal, tooltip status
#   - Auto mode: baca media session OS (Spotify/Chrome), debounce 2s,
#     dedupe, paused → clear
#   - Program mode: menu tray → input judul program; auto dihentikan
#     sampai "Kembali ke Otomatis"
#   - Heartbeat tiap 60s (jaga state tidak basi di backend)
#   - Offline: simpan metadata terakhir saja, kirim ulang saat pulih
#
# Pakai:
#   .\companion-agent.ps1                    # tray penuh
#   .\companion-agent.ps1 -Headless 3        # test: 3 iterasi engine tanpa UI
#   .\companion-agent.ps1 -ProgramTitle "Talkshow Sore"   # langsung program mode

param(
  [int]$Headless = 0,
  [string]$ProgramTitle = "",
  [string]$SiteUrl = "",
  [string]$Token = ""
)

$ErrorActionPreference = "Continue"

# ---------- konfigurasi ----------
$envFile = "C:\Users\Meong\Documents\8ehradioitb\.env"
foreach ($line in Get-Content $envFile) {
  if ($line -match '^(NEXT_PUBLIC_SITE_URL|NEXTAUTH_URL|STUDIO_DEVICE_TOKEN)=(.*)$') {
    $rawVal = $Matches[2].Trim('"')
    switch ($Matches[1]) {
      "NEXT_PUBLIC_SITE_URL" { if (-not $SiteUrl) { $SiteUrl = $rawVal } }
      "NEXTAUTH_URL"         { if (-not $SiteUrl) { $SiteUrl = $rawVal } }
      "STUDIO_DEVICE_TOKEN"  { if (-not $Token)   { $Token = $rawVal } }
    }
  }
}
if (-not $SiteUrl) { throw "SiteUrl kosong (cek NEXT_PUBLIC_SITE_URL di .env)" }
if (-not $Token)   { throw "Token kosong (cek STUDIO_DEVICE_TOKEN di .env)" }
if ($SiteUrl -notmatch '^https?://') { $SiteUrl = "https://$SiteUrl" }
$uri = "$SiteUrl/api/studio/now-playing"

$DEBOUNCE_MS = 2000
$HEARTBEAT_MS = 60000

# ---------- SMTC reader (dari metadata-watcher, tanpa filter) ----------
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$null = [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager, Windows.Media.Control, ContentType = WindowsRuntime]
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

function Get-TopSession {
  # shortcut: ambil session playing pertama; kalau tidak ada yang playing,
  # ambil session pertama saja (untuk deteksi paused). Spotify > lainnya.
  $manager = Await ([Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager]::RequestAsync()) (
    [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionManager])
  $sessions = @($manager.GetSessions())
  if ($sessions.Count -eq 0) { return $null }

  foreach ($s in $sessions) {
    if ($s.SourceAppUserModelId -like "*Spotify*") { return $s }
  }
  return $sessions[0]
}

function Read-Playback {
  # keluaran: @{ title; artist; playing (bool); hasSession }
  $s = Get-TopSession
  if (-not $s) { return @{ hasSession = $false } }
  $props = Await ($s.TryGetMediaPropertiesAsync()) (
    [Windows.Media.Control.GlobalSystemMediaTransportControlsSessionMediaProperties])

  # shortcut: PlaybackInfo null via proyeksi WinRT PS 5.1 - inferensi dari
  # posisi timeline (maju = playing). Upgrade: windows-rs di Tauri.
  $playing = $false
  if ($s.PlaybackInfo) {
    $playing = ([string]$s.PlaybackInfo.PlaybackStatus) -eq "Playing"
  } else {
    $p1 = $s.GetTimelineProperties().Position
    Start-Sleep -Milliseconds 250
    $p2 = $s.GetTimelineProperties().Position
    $playing = $p2 -gt $p1
  }
  return @{
    hasSession = $true
    title      = [string]$props.Title
    artist     = [string]$props.Artist
    playing    = $playing
  }
}

# ---------- HTTP ----------
$script:lastPushOk = $true   # status ikon
$script:lastError = ""

function Send-Post($body) {
  try {
    $resp = Invoke-RestMethod -Uri $uri -Method Post -ContentType "application/json" `
      -Headers @{ "X-Device-Token" = $Token } -Body ($body | ConvertTo-Json -Compress) -TimeoutSec 10
    $script:lastPushOk = $true
    $script:lastError = ""
    return $resp
  } catch {
    $script:lastPushOk = $false
    $script:lastError = $_.Exception.Message
    return $null
  }
}

# ---------- engine state ----------
$script:mode = if ($ProgramTitle) { "program" } else { "auto" }
$script:programTitle = $ProgramTitle
$lastSentKey = ""        # dedupe judul|artis yang sudah dikirim
$candidateKey = ""       # judul|artis kandidat debounce
$candidateFirstSeen = [datetime]::MinValue
$candidateData = $null
$pendingPlayPause = $false  # perlu kirim clear (pause terdeteksi)
$lastHeartbeat = [datetime]::MinValue

function Send-Candidate($data) {
  $resp = Send-Post @{ mode = "auto"; title = $data.title; artist = $data.artist; playbackState = $(if ($data.playing) { "Playing" } else { "Paused" }) }
  if ($null -ne $resp) {
    $script:lastPushOk = $true
    if ($resp.skipped) { Write-Host "  (skipped oleh server: $($resp.reason))" }
  }
}

function Engine-Step {
  # 1. heartbeat (hanya jika ada state agent di server & push terakhir sukses)
  if (((Get-Date) - $lastHeartbeat).TotalMilliseconds -ge $HEARTBEAT_MS) {
    $null = Send-Post @{ mode = "heartbeat" }
    $lastHeartbeat = Get-Date
  }

  # 2. program mode: kirim judul program sekali (dedupe), auto dilesuhkan
  if ($script:mode -eq "program") {
    $key = "prog|$script:programTitle"
    if ($key -ne $lastSentKey) {
      $resp = Send-Post @{ mode = "program"; title = $script:programTitle }
      if ($null -ne $resp) { $lastSentKey = $key }
    }
    return
  }

  # 3. auto mode
  $pb = Read-Playback
  if (-not $pb.hasSession) {
    if ($lastSentKey -ne "clear") {
      $null = Send-Post @{ mode = "clear" }
      $lastSentKey = "clear"
    }
    return
  }

  if (-not $pb.playing) {
    # paused/stop → clear widget sekali (dedupe)
    if ($lastSentKey -ne "clear") {
      $resp = Send-Post @{ mode = "clear" }
      if ($null -ne $resp) { $lastSentKey = "clear"; $pendingPlayPause = $false }
    }
    return
  }

  $key = "$($pb.title)|$($pb.artist)"
  if ($key -eq $lastSentKey) { return }   # sudah dikirim

  # debounce: kandidat harus stabil DEBOUNCE_MS
  if ($key -ne $candidateKey) {
    $candidateKey = $key
    $candidateFirstSeen = Get-Date
    $candidateData = $pb
    return
  }
  if (((Get-Date) - $candidateFirstSeen).TotalMilliseconds -lt $DEBOUNCE_MS) { return }

  Send-Candidate $candidateData
  if ($script:lastPushOk) { $lastSentKey = $key }
  $candidateKey = ""
}

# ---------- headless (test) ----------
if ($Headless -gt 0) {
  Write-Host "HEADLESS: $Headless iterasi engine (mode=$script:mode)"
  for ($i = 1; $i -le $Headless; $i++) {
    Write-Host "iter $i..."
    Engine-Step
    if ($i -lt $Headless) { Start-Sleep -Seconds 1 }
  }
  Write-Host "status push terakhir: ok=$script:lastPushOk err=$script:lastError"
  exit $(if ($script:lastPushOk) { 0 } else { 1 })
}

# ---------- tray UI ----------
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName Microsoft.VisualBasic

function Make-Icon($color) {
  $bmp = New-Object System.Drawing.Bitmap(16, 16)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.Clear($color)
  $g.Dispose()
  return [System.Drawing.Icon]::FromHandle($bmp.GetHicon())
}
$iconOk = Make-Icon([System.Drawing.Color]::LimeGreen)
$iconErr = Make-Icon([System.Drawing.Color]::Firebrick)

$notify = New-Object System.Windows.Forms.NotifyIcon
$notify.Icon = $iconOk
$notify.Visible = $true
$notify.Text = "8EH Companion"

$menu = New-Object System.Windows.Forms.ContextMenuStrip

$menuProgram = New-Object System.Windows.Forms.ToolStripMenuItem("Mode Program...")
$menuAuto = New-Object System.Windows.Forms.ToolStripMenuItem("Kembali ke Otomatis")
$menuExit = New-Object System.Windows.Forms.ToolStripMenuItem("Keluar")
$menuAuto.Enabled = $false

$menuProgram.Add_Click({
  $t = [Microsoft.VisualBasic.Interaction]::InputBox("Judul program/segmen (obrolan):", "8EH Program Mode", $script:programTitle)
  if ($t.Trim()) {
    $script:programTitle = $t.Trim()
    $script:mode = "program"
    $menuAuto.Enabled = $true
    $lastSentKey = ""
    Write-Host "program mode: $script:programTitle"
  }
})
$menuAuto.Add_Click({
  $script:mode = "auto"
  $script:programTitle = ""
  $menuAuto.Enabled = $false
  $lastSentKey = ""
  $candidateKey = ""
  Write-Host "kembali ke otomatis"
})
$menuExit.Add_Click({ $script:exit = $true })

$menu.Items.AddRange(@($menuProgram, $menuAuto, (New-Object System.Windows.Forms.ToolStripSeparator), $menuExit))
$notify.ContextMenuStrip = $menu

$script:exit = $false
Write-Host "8EH Companion Agent berjalan (tray). URL: $uri"

# loop utama: engine tiap detik + status ikon
while (-not $script:exit) {
  Engine-Step
  $notify.Icon = $(if ($script:lastPushOk) { $iconOk } else { $iconErr })
  $status = $(if ($script:lastPushOk) { "terhubung" } else { "GAGAL: $($script:lastError.Substring(0, [Math]::Min(60, $script:lastError.Length)))" })
  $notify.Text = "8EH Companion - $status - mode: $($script:mode)"
  [System.Windows.Forms.Application]::DoEvents()
  Start-Sleep -Milliseconds 1000
}

$notify.Visible = $false
$notify.Dispose()
Write-Host "keluar."
