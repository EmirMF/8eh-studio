# Verifikasi macOS 8EH Studio

CI menghasilkan dua DMG dan memeriksa isi bundle. Sebelum dipakai siaran, jalankan pengujian ini secara interaktif pada Mac cloud macOS 13 atau lebih baru. Gunakan endpoint Icecast uji, bukan mount produksi.

1. Pasang DMG dan buka aplikasi melalui **Privacy & Security → Open Anyway**. Pastikan login Google membuka browser default dan pengaturan tersimpan setelah aplikasi dimulai ulang.
2. Pilih input audio yang tersedia. Pastikan meter bergerak saat ada suara, lalu uji input kedua dan perubahan routing A/B. Tolak izin Microphone sekali dan pastikan aplikasi menampilkan kegagalan input; izinkan kembali dan coba ulang.
3. Pilih output monitor. Dengarkan kedua jalur, lalu pilih output yang sama dengan desktop audio dan pastikan peringatan feedback muncul.
4. Pilih **Desktop audio**. Tolak Screen Recording sekali dan pastikan preview gagal dengan pesan izin. Izinkan lalu buka ulang aplikasi; putar audio dan pastikan meter bergerak tanpa umpan balik monitor.
5. Putar, jeda, dan ganti lagu di Spotify lalu Music. Periksa judul, artis, status, dan artwork saat tersedia. Tolak Automation sekali untuk memeriksa perilaku tanpa metadata.
6. Siarkan ke Icecast uji dan dengarkan output dari penerima lain. Rekam MP3, hentikan sesi, buka folder rekaman, dan periksa file dapat diputar. Putuskan jaringan saat siaran, lalu gunakan retry setelah tersambung lagi.
7. Cabut perangkat input/output saat preview, lalu pastikan status kesalahan dapat dilihat dan sesi dapat dihentikan.

**Batas pengujian:** Mac cloud tidak membuktikan kompatibilitas MG16XU. Uji perangkat itu pada Mac fisik sebelum menyebutnya terverifikasi.
