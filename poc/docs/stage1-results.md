# Hasil Tahap 1 — Proof of Concept

Status: **berjalan**. Hasil terbukti di bawah adalah dari laptop Windows
pengembang (bukan mesin studio). Baris kosong = belum diuji.

## 1. Metadata — Windows (SMTC)

| Tanggal | Mesin | Sumber | Hasil | Catatan |
|---|---|---|---|---|
| 2026-11-15 | Laptop dev Windows | Spotify desktop | ✅ OK | `{"source":"Spotify.exe","title":"Last Night on Earth","artist":"Green Day","playbackState":"Paused"}` |
| | | Chrome | Belum | Jalankan `metadata-watcher.ps1 -Filter chrome` saat memutar Spotify Web/YouTube |

**Quirk terdokumentasi:** `PlaybackInfo` selalu `null` pada proyeksi WinRT
PowerShell 5.1, sehingga status playback diinferensi dari posisi timeline
(maju = Playing, diam = Paused). App Tauri (crate `windows` Rust) tidak
terkena quirk ini — verifikasi normal saat Fase 2.

## 2. Metadata — macOS (JXA)

| Tanggal | Mesin | Sumber | Hasil | Catatan |
|---|---|---|---|---|
| | | Spotify | | |
| | | Music | | |
| | | Chrome/Safari | | |

## 3. Audio capture — pipeline FFmpeg

| Tanggal | OS | Device | mean_volume | max_volume | Catatan |
|---|---|---|---|---|---|
| 2026-11-15 | Windows | Microphone Array (laptop dev) | −73.1 dB | −58.1 dB | Pipeline dshow→volumedetect ✅ OK. Level rendah wajar: ruang senyap. **MG16XU belum diuji — harus di studio.** |
| | Windows | MG-XU (studio) | | | |
| | macOS | MG-XU (studio) | | | |

## 4. AzuraCast metadata update — OUT OF SCOPE (keputusan produk)

Stasiun `8EH Radio ITB` dikonfigurasi **frontend-only** (`"backend":"none"`,
tanpa Liquidsoap), sehingga endpoint `/api/station/{id}/nowplaying/update`
selalu menolak (`StationUnsupportedException`). **Keputusan 2026-11-15:**
siaran hanya lewat widget website — sinkronisasi metadata AzuraCast
dihapus dari scope. Skrip `update-azuracast.ps1` disimpan hanya sebagai
referensi jika suatu hari stasiun diaktifkan backend-nya.

## 4b. Endpoint widget situs — `POST /api/studio/now-playing` (BARU, dibuat & teruji)

Diimplementasikan di repo situs (`8ehradioitb`):

- `lib/now-playing-state.js` — helper state dibagikan dari route dashboard
- `app/api/studio/now-playing/route.js` — auth `X-Device-Token` env
  `STUDIO_DEVICE_TOKEN`, menulis ke `NowPlayingState` yang sama dengan
  tombol manual, lalu publish Pusher ke widget pendengar
- Skrip kirim: `poc/windows/push-now-playing.ps1`

Hasil tes (dev server lokal, MongoDB & Pusher produksi):

| Tes | Hasil |
|---|---|
| POST manual `mode:auto` | ✅ state berubah, `source: studio-agent`, GET konsisten |
| POST `mode:program` | ✅ `source: studio-program`, judul program masuk |
| POST `mode:clear` | ✅ kembali ke fallback persis seperti sebelumnya |
| POST tanpa token | ✅ 401 |
| POST `playbackState:Paused` | ✅ `{"skipped":true}` — judul basi tidak masuk |
| Widget payload format | ✅ sama dengan kontrak `GlobalAudioPlayer` |

** Jalur lengkap Spotify → widget terbukti:**
`Spotify.exe → metadata-watcher → push-now-playing → API → Pusher` (latensi
tes lokal <1 detik; debounce 2s belum diterapkan — tugas Companion Agent Tahap 3).

**Deploy:** tambahkan `STUDIO_DEVICE_TOKEN` ke env Vercel sebelum merge;
tanpa itu endpoint membalas 401 di produksi.

## 6. Tahap 2 — integrasi website (2026-11-15, SELESAI)

Perubahan di repo situs:

1. `prisma/schema.prisma` — `NowPlayingState.control` (ownership:
   `"manual"` dashboard / `"agent-auto"` / `"agent-program"` / null)
2. `lib/now-playing-state.js` — expiry: state agent >3 menit tanpa
   heartbeat → GET balas fallback (agent mati ≠ judul menggantung)
3. `app/api/now-playing/route.js` — set-item/clear dashboard mengunci
   (`control: "manual"`) / melepas (`control: null`)
4. `app/api/studio/now-playing/route.js` — 409 saat dashboard mengunci;
   mode baru `heartbeat` (refresh updatedAt tanpa publish); `clear`
   melepas kendali agent
5. `GlobalAudioPlayer.jsx` — langganan Pusher `now-playing` aktif sejak
   mount (sebelumnya menunggu interaksi; target <5 detik tanpa aksi user)

Tes otomatis: **10/10 PASS** (auto diterima, lock 409 ×3, heartbeat locked,
lepas lock, refresh updatedAt, expiry → fallback, state segar, clear).
`prisma generate` sudah dijalankan.

## 7. Tahap 3 — Companion Agent (2026-11-15, engine SELESAI)

`poc/windows/companion-agent.ps1` — prototype fungsional penuh (logika
identik dengan rencana Tauri; tray via WinForms NotifyIcon):

- Engine loop 1 detik: baca SMTC → debounce 2s → dedupe → push
- Paused/stop → clear widget; Spotify diprioritaskan saat multi-session
- Program mode: `-ProgramTitle` atau menu tray → auto dilesuhkan
- Heartbeat 60s; ikon tray hijau/merah + tooltip status (C-05)
- Mode test `-Headless N` tanpa UI

Hasil tes engine (dev server lokal):

| Skenario | Hasil |
|---|---|
| Spotify paused → clear → widget fallback | ✅ |
| Program mode → `source: studio-program` | ✅ |
| Server mati → gagal rapi (exit 1, tanpa crash) | ✅ |
| Server pulih → agent lanjut otomatis | ✅ |

**Sisa Tahap 3:** smoke test tray UI di laptop musik asli (interaksi
menu tidak bisa diuji otomatis di sini), dan uji produksi setelah deploy
(set `STUDIO_DEVICE_TOKEN` di Vercel). Batasan dikenal: agent keluar
tanpa clear — widget pulih via expiry 3 menit (diterima sementara).

## 7b. Tahap 3 — Companion Tauri 2 (2026-11-15, SELESAI)

`apps/companion/` — port engine ke desktop app native:

- Rust: SMTC via `windows` crate (status playback **akurat** — tidak
  terkena quirk proyeksi WinRT PowerShell), reqwest blocking,
  tray `tray-icon`, config JSON di `%APPDATA%\com.eighteh.companion`
- UI: window settings (URL, token, Program Mode, status polling)
- Logika engine identik dengan versi PS yang teruji (debounce 2s,
  dedupe, clear-when-pause, heartbeat 60s, prioritas Spotify)

Hasil uji run nyata (`companion.exe`, dev server lokal):

| Uji | Hasil |
|---|---|
| Build (MSVC 2022, Rust 1.100) | ✅ |
| Run + baca Spotify playing → widget update | ✅ "Right Now — One Direction" |
| Status playback akurat via GetPlaybackInfo | ✅ (quirk PS hilang di Rust) |
| RAM saat jalan | ✅ 46,3 MB (< target 50 MB PRD) |

Diketahui belum dikerjakan: menu context tray (klik kiri buka settings
sudah jalan), autostart, installer NSIS (`cargo tauri build`), ikon
status hijau/merah di tray (tooltip status via settings window dulu).

## 5. Matriks dukungan (hasil akhir Tahap 1)

| Kombinasi | Status |
|---|---|
| Spotify desktop — Windows | ✅ Tercoba di mesin dev |
| Spotify desktop — macOS | Belum (jalankan `metadata-watcher.sh`) |
| Music.app — macOS | Belum |
| Chrome + Spotify Web/YouTube — Windows | Belum (`metadata-watcher.ps1 -Filter chrome`) |
| Chrome/Safari — macOS | Belum |
| MG16XU → FFmpeg (Windows/macOS) | Belum — **gate utama Tahap 1**, uji di studio |
| AzuraCast `-ConfirmLive` | ❌ Tidak relevan — stasiun `backend:none`, sinkron AzuraCast dihapus dari scope (2026-11-15). Widget situs kini punya endpoint sendiri yang teruji (lihat §4b). |

## Sisa pekerjaan Tahap 1 (semua butuh perangkat yang tidak ada di mesin dev)

1. Studio: `test-audio-capture.ps1 -Device "<nama MG-XU>"` → target mean −20…−6 dBFS.
2. macOS: `metadata-watcher.sh` untuk Spotify, Music, dan coba browser.
3. Windows: `metadata-watcher.ps1 -Filter chrome` untuk Spotify Web/YouTube.
4. Deploy: set `STUDIO_DEVICE_TOKEN` di Vercel, lalu jalankan
   `push-now-playing.ps1 -SiteUrl https://<domain-produksi>` untuk uji produksi.
