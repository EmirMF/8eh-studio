# 8EH Studio

Aplikasi desktop Windows untuk siaran Icecast, monitoring audio, rekaman MP3 opsional, dan metadata 8EH Radio ITB.

Panduan [video OBS dan MediaMTX](apps/studio/mediamtx/README.md) mencakup dua output OBS, player website, dan setup staging.

## Build

1. Clone repo dengan Git LFS tersedia, lalu jalankan `git lfs pull` agar `apps/studio/src-tauri/binaries/ffmpeg.exe` terunduh lengkap.
2. Pasang Rust, Node.js, dan prasyarat Tauri untuk Windows.
3. Dari `apps/studio`, jalankan `npm ci` lalu `npm run tauri -- dev` untuk pengembangan.
4. Jalankan `npm run tauri -- build` untuk membuat installer NSIS di `apps/studio/src-tauri/target/release/bundle/nsis/` (atau di lokasi `CARGO_TARGET_DIR` bila diatur).

Build menyertakan FFmpeg. Pemberitahuan dan lisensinya ada di `apps/studio/src-tauri/binaries/`. Folder `dist/` berisi hasil build lokal dan tidak masuk Git.

`apps/companion/` dan `poc/` menyimpan kode serta catatan eksperimen sebelumnya.

## macOS (13 atau lebih baru)

Build di Mac Intel atau Apple Silicon: pasang Xcode Command Line Tools, Rust, dan Node.js. Jalankan `bash apps/studio/src-tauri/macos/prepare.sh`, lalu dari `apps/studio` jalankan `npm ci` dan `npm run tauri -- build`. Script menyiapkan FFmpeg statis, helper ScreenCaptureKit, dan ikon macOS. CI `.github/workflows/studio-macos.yml` menghasilkan DMG terpisah untuk kedua arsitektur.

Untuk memasang DMG, buka lalu seret **8EH Studio** ke **Applications**. Karena rilis ini belum ditandatangani Apple, coba buka aplikasi sekali, kemudian pilih **System Settings → Privacy & Security → Open Anyway**. Jika perlu, pengguna mahir dapat menjalankan `xattr -cr "/Applications/8EH Studio.app"` di Terminal. Saat diminta, izinkan akses Microphone, Screen Recording (untuk Desktop audio), dan Automation (untuk metadata Spotify/Music), lalu buka ulang aplikasi setelah memberi izin Screen Recording.

Metadata Chrome/Safari belum didukung di macOS. MG16XU pada Mac belum terverifikasi dengan perangkat fisik. Sebelum rilis untuk siaran studio, lakukan uji interaktif di Mac: input, output monitor, rekaman, desktop audio tanpa feedback, Spotify/Music, dan koneksi Icecast.
