# 8EH Stage 1 — Proof-of-Concept Kit

Kit validasi Tahap 1. Buktikan tiga jalur sebelum membangun aplikasi Tauri:

1. **Metadata**: lagu terbaca dari OS (Spotify/Music/browser).
2. **Audio**: FFmpeg menangkap input USB mixer (MG16XU) dengan level sehat.
3. **AzuraCast**: endpoint update metadata bekerja pada server stasiun.

Hasil aktual dicatat di `docs/stage1-results.md`.

## Struktur

```
poc/
├── windows/
│   ├── metadata-watcher.ps1     # SMTC via WinRT (Windows PowerShell 5.1)
│   ├── test-audio-capture.ps1   # dshow: list device + capture + volumedetect
│   └── update-azuracast.ps1     # POST /api/station/{id}/nowplaying/update
├── macos/
│   ├── metadata-watcher.sh       # JXA: Spotify.app + Music.app
│   └── test-audio-capture.sh    # AVFoundation: list + capture + volumedetect
├── out/                         # hasil capture wav (di-ignore)
└── docs/stage1-results.md       # template + hasil pengujian
```

## Cara pakai (Windows)

```powershell
cd C:\Users\Meong\Documents\8eh-studio\poc\windows

# 1. Metadata — jalankan Spotify (atau Chrome) lalu putar lagu, lalu:
.\metadata-watcher.ps1 -Once             # satu poll
.\metadata-watcher.ps1 -Filter spotify   # loop, hanya Spotify

# 2. Audio — cari device mixer di list, lalu:
.\test-audio-capture.ps1                              # list device
.\test-audio-capture.ps1 -Device "MG-XU" -Seconds 5   # capture (di studio)

# 3. AzuraCast:
.\update-azuracast.ps1 -Title "Uji Coba" -Artist "8EH"            # dry run
.\update-azuracast.ps1 -Title "Uji Coba" -Artist "8EH" -ConfirmLive
```

Kredensial AzuraCast otomatis dibaca dari `.env` situs (`8ehradioitb`).

## Cara pakai (macOS)

```bash
cd poc/macos
chmod +x *.sh

# 1. Metadata — buka Spotify/Music, putar lagu, lalu:
./metadata-watcher.sh          # loop; izinkan Automation saat prompt pertama

# 2. Audio:
./test-audio-capture.sh                 # list device
./test-audio-capture.sh 5               # capture 5s dari MG-XU (auto-index)

# 3. AzuraCast (curl):
curl -s -X POST "https://stream.8ehradioitb.com/api/station/1/nowplaying/update" \
  -H "Content-Type: application/json" -H "X-API-Key: $KEY" \
  -d '{"title":"Uji Coba","artist":"8EH"}'
```

## Kriteria lolos Tahap 1

- [ ] Windows: `metadata-watcher` menampilkan judul + artis saat Spotify diputar.
- [ ] macOS: `metadata-watcher.sh` menampilkan judul + artis (Spotify & Music).
- [ ] Studio: capture 5 detik dari MG-XU menghasilkan mean_volume −20…−6 dBFS.
- [ ] AzuraCast: POST `-ConfirmLive` mengembalikan `success:true` dan judul
      berubah di `https://stream.8ehradioitb.com/api/nowplaying/1`.
- [ ] Browser (P1): catat kombinasi browser/situs yang memberi metadata via SMTC.

## Batasan yang diketahui

- `metadata-watcher.ps1` hanya jalan di **Windows PowerShell 5.1** (proyeksi WinRT).
- macOS: hanya Spotify.app & Music.app; browser perlu pembuktian terpisah.
- `update-azuracast.ps1` tanpa `-ConfirmLive` tidak mengirim apa pun.
- Endpoint update metadata **menimpa Now Playing yang dilihat pendengar**;
  lakukan tes saat siaran off-air atau jam sepi.
