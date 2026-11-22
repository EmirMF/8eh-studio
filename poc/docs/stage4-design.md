# Tahap 4 — Desain "8EH Studio" (app gabungan)

Tanggal: 2026-11-15. Status: desain disetujui → implementasi.

## Fakta terverifikasi (uji push 15 Nov 2026)

- Station AzuraCast: frontend **Icecast 2.4-kh22 port 8000**, backend `none`
- Mount `/radio.mp3` (MP3 192kbps 44.1kHz stereo), source_pw aktif
- **Uji push nyata BERHASIL**: FFmpeg 9.0.1 dshow (mic Realtek) →
  `icecast://source:***@stream.8ehradioitb.com:8000/radio.mp3`
  tersambung 90 detik, incoming bitrate ~202 kbps, sample stream 400KB
  bisa diunduh (audio/mpeg valid)
- Port 8000 & 80 terjangkau dari jaringan dev (443 diblokir — tidak relevan)
- Kredensial stream bisa diambil dari API admin (`frontend_config.port`,
  `source_pw`) — situs sudah punya kode inferensi ini di `lib/azuracast.js`

## Keputusan arsitektur

**Satu binary "8EH Studio" untuk dua laptop** (pilihan user), tab menentukan
peran yang dipakai:

```
8EH Studio (Tauri 2, Rust, apps/studio/)
├── Tab "Metadata"   — engine SMTC + Program Mode (port dari apps/companion)
├── Tab "Siaran"     — device picker, tombol ON AIR, level meter, log
└── Tab "Pengaturan" — URL situs, token, kredensial Icecast, bitrate

Backed:
├── Engine metadata: thread 1s, SMTC, debounce 2s, heartbeat 60s (teruji)
├── Broadcaster: std::process spawn FFmpeg (bukan plugin shell — tanpa
│   capability hassle, stderr streaming dibaca thread Rust)
│   args: -f dshow -i audio=<dev> -acodec libmp3lame -b:a <n>k -ar 44100
│         -ac 2 -content_type audio/mpeg -f mp3
│         icecast://source:<pw>@<host>:8000/radio.mp3
├── Level meter: filter -af astats=metadata=1:reset=1,ametadata=print →
│   parse "RMS_level=-xx.x" dari stderr per frame
├── Device list: `ffmpeg -list_devices true -f dshow -i dummy` → parse
└── Config: %APPDATA%\com.eighteh.studio\config.json (migrasi otomatis dari
    com.eighteh.companion bila ada)
```

## CLI headless (untuk uji otomatis)

`8EH Studio.exe --push-test=N` → jalankan push N detik pakai config
tersimpan, log ke stdout, exit. Sama pola dengan uji engine metadata.

## Hal yang sengaja ditunda (dicatat, bukan dilupakan)

- Bundle FFmpeg ke installer (~170MB) — v1 deteksi PATH + field path di
  pengaturan
- macOS (WASAPI→CoreAudio adapter)
- Ducking otomatis saat ON AIR, VU stereo terpisah L/R
- Retry otomatis push saat putus (v1: tombol ON AIR manual ulang, status
  jelas di UI)
- MG16XU: muncul otomatis di device picker begitu dicolok (dshow) —
  tanpa kode khusus

## Hasil uji implementasi (15 Nov 2026)

App `apps/studio/` build sukses. Uji `--push-test` headless:

- ✅ studio.exe spawn FFmpeg → tersambung Icecast 8000 (192 kbps)
- ✅ Level meter RMS ter-parse real-time dari stderr astats
- ✅ Proses keluar bersih (kill + wait, tanpa orphan)
- ⚠️ Uji terhenti di detik-2: source LAIN (kemungkinan BUTT kru
  dengan auto-reconnect, terlihat menyambung ulang di 12:37) merebut
  mount kembali → error -10053. **Ini konflik dunia nyata, bukan bug**
  — persis risiko "dua source berebut mount" di risk register.
  Solusi operasional: hanya laptop broadcaster yang boleh push.

Catatan lingkungan: disk C: pernah penuh (0 GB) saat build studio;
dibebaskan 2,3 GB via `cargo clean` companion (exe disalin dulu ke
`8eh-studio\dist\`). Free space setelahnya ~3 GB — **kredit disk tipis,
hapus artefak besar lain sebelum release build**.

Binary debug: `8eh-studio\dist\studio.exe` + `companion.exe`.

## Risk register

| Risiko | Mitigasi |
|---|---|
| dshow device name berubah antar boot | simpan device terakhir; pesan jelas bila tidak ada + tombol refresh |
| source_pw diganti admin AzuraCast | field password di pengaturan; dokumentasi cara ambil dari API |
| Dua laptop push bersamaan → saling tendang | catat di docs kru: hanya laptop A yang ON AIR (PRD dua-node) |
| Meter parse gagal (format berubah) | meter opsional — degradasi jadi status teks, siaran tetap jalan |
