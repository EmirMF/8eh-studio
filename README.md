# 8EH Studio

Aplikasi desktop Windows untuk siaran Icecast, monitoring audio, rekaman MP3 opsional, dan metadata 8EH Radio ITB.

## Build

1. Clone repo dengan Git LFS tersedia, lalu jalankan `git lfs pull` agar `apps/studio/src-tauri/binaries/ffmpeg.exe` terunduh lengkap.
2. Pasang Rust, Node.js, dan prasyarat Tauri untuk Windows.
3. Dari `apps/studio`, jalankan `npm ci` lalu `npm run tauri -- dev` untuk pengembangan.
4. Jalankan `npm run tauri -- build` untuk membuat installer NSIS di `apps/studio/src-tauri/target/release/bundle/nsis/` (atau di lokasi `CARGO_TARGET_DIR` bila diatur).

Build menyertakan FFmpeg. Pemberitahuan dan lisensinya ada di `apps/studio/src-tauri/binaries/`. Folder `dist/` berisi hasil build lokal dan tidak masuk Git.

`apps/companion/` dan `poc/` menyimpan kode serta catatan eksperimen sebelumnya.
