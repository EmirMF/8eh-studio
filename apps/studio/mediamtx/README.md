# Video siaran di staging

## VPS 8EH

1. Siapkan hostname staging, misalnya `video-staging.8ehradioitb.com`, dan sertifikat TLS yang dipercaya publik. Simpan salinan `fullchain.pem` dan `privkey.pem` di direktori khusus di VPS. Jika sertifikat diperbarui, perbarui salinan dan restart container MediaMTX.
2. Jalankan Compose dari direktori ini di VPS. Isi tiga variabel berikut pada konfigurasi rahasia host, tanpa memasukkan nilai rahasia ke Git.

   | Variabel | Isi |
   | --- | --- |
   | `VIDEO_CERT_DIR` | Path absolut direktori sertifikat di VPS |
   | `VIDEO_PUBLISH_PASSWORD` | Kata sandi publikasi acak, misalnya keluaran `openssl rand -hex 32` |
   | `VIDEO_WEB_ORIGINS` | Origin website staging, atau daftar origin yang dipisah koma |

   Periksa konfigurasi dengan `docker compose config --quiet`, lalu jalankan `docker compose up -d`. Batasi akses TCP 1936 ke jaringan studio jika jaringan tersebut memiliki IP publik tetap.
3. Arahkan reverse proxy HTTPS untuk hostname video ke `http://127.0.0.1:8888`. Teruskan path `/studio/` apa adanya, termasuk permintaan segmen dan header CORS MediaMTX. Nonaktifkan cache untuk playlist `.m3u8`. Port 8888 hanya tersedia secara lokal di VPS.
4. Isi `LIVE_VIDEO_HLS_URL=https://video-staging.8ehradioitb.com/studio/index.m3u8` pada konfigurasi deploy Next.js. Deploy website staging. Halaman `/live-video` bisa dibuka sebelum siaran dimulai dan akan menampilkan status offline.

## OBS dan 8EH Studio

1. Buat profil OBS khusus staging. Untuk output utama website, pilih layanan Custom di **Settings → Stream**. Isi server `rtmps://video-staging.8ehradioitb.com:1936/studio?user=studio&pass=<VIDEO_PUBLISH_PASSWORD>` dan biarkan kolom stream key kosong. Gunakan canvas 16:9, H.264, AAC, dan keyframe tiap dua detik. Atur dan uji sumber audio dari OBS.
2. Untuk tujuan kedua, pasang Aitum Stream Suite 1.2.6 yang kompatibel dengan versi OBS pada mesin studio. Buat canvas 9:16 dan output **Stream** bernama **TikTok** yang memakai canvas tersebut. Saat staging, pakai endpoint RTMP uji terpisah. Untuk produksi, gunakan server RTMP dan stream key dari akun TikTok 8EH setelah akses LIVE dari encoder terverifikasi.
3. Aktifkan WebSocket Server dan autentikasi melalui **Tools → WebSocket Server Settings**. Buka tab **Video** di Studio, masukkan port dan kata sandi OBS, lalu klik **Hubungkan OBS**. Pilih output dan canvas TikTok di Studio. Kata sandi WebSocket diperlukan tiap kali Studio dijalankan.
4. Uji mulai dan berhenti per tujuan, perpindahan scene, pemutaran dari perangkat lain, serta sambung ulang setelah OBS ditutup dan dibuka. Periksa juga bahwa berhenti di satu tujuan tidak menghentikan tujuan lain atau Icecast. Untuk OBS, status **Output aktif** menunjukkan bahwa OBS sedang mengirim data. Konfirmasi playback TikTok dilakukan di TikTok.

[Konfigurasi MediaMTX](https://mediamtx.org/docs/features/configuration) · [Publikasi OBS melalui RTMPS](https://mediamtx.org/docs/publish/obs-studio) · [Aitum Stream Suite 1.2.6](https://github.com/Aitum/obs-aitum-stream-suite/releases/tag/1.2.6)
