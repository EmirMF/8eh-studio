#!/bin/bash
# 8EH Stage 1 PoC - Uji capture audio input macOS (AVFoundation)
# Menampilkan device list, lalu capture N detik dari device MG-XU
# (atau index yang diberikan) dan analisa level dBFS.
#
# Pakai:
#   ./test-audio-capture.sh              # list device saja
#   ./test-audio-capture.sh 5            # auto-cari "MG-XU", capture 5s
#   ./test-audio-capture.sh 5 2          # pakai input index 2, capture 5s
#
# Catatan AVFoundation: input audio = angka index, diawali ":" tanpa index video.
# Contoh: ffmpeg -f avfoundation -i ":2" -t 5 out.wav

set -u
SECONDS_TO_CAPTURE="${1:-}"
FORCED_INDEX="${2:-}"
OUT_DIR="$(cd "$(dirname "$0")/.." && pwd)/out"
mkdir -p "$OUT_DIR"

FFMPEG=$(command -v ffmpeg) || { echo "ffmpeg tidak ada di PATH"; exit 1; }

echo "== AVFoundation devices =="
DEVLIST=$("$FFMPEG" -hide_banner -f avfoundation -list_devices true -i "" 2>&1) || true
echo "$DEVLIST" | grep -E '\[AVFoundation'

INDEX="$FORCED_INDEX"
if [ -z "$INDEX" ] && [ -n "$SECONDS_TO_CAPTURE" ]; then
  # ambil index audio pertama yang namanya memuat MG-XU
  INDEX=$(echo "$DEVLIST" | grep -iE '\[AVFoundation.*\[2\].*MG-?XU' | grep -oE '\[[0-9]+\]' | head -1 | tr -d '[]')
fi

if [ -z "$SECONDS_TO_CAPTURE" ]; then
  echo ""
  echo "Belum ada durasi. Jalankan: ./test-audio-capture.sh <detik> [index-input]"
  echo "Di studio, pastikan device 'MG-XU' muncul di list di atas."
  exit 0
fi
if [ -z "$INDEX" ]; then
  echo "GAGAL: device 'MG-XU' tidak ditemukan. Jalankan dengan index eksplisit:"
  echo "  ./test-audio-capture.sh $SECONDS_TO_CAPTURE <index-audio>"
  exit 1
fi

WAV="$OUT_DIR/capture-test-mac.wav"
echo ""
echo "== Capture $SECONDS_TO_CAPTURE detik dari input index $INDEX =="
"$FFMPEG" -hide_banner -y -f avfoundation -i ":$INDEX" -t "$SECONDS_TO_CAPTURE" -ac 2 -ar 44100 "$WAV" 2>&1 | tail -n 4

if [ ! -f "$WAV" ]; then
  echo "GAGAL: file output tidak dibuat - cek izin Microphone untuk Terminal/ffmpeg."
  exit 1
fi

echo ""
echo "== Analisis level (dBFS) =="
"$FFMPEG" -hide_banner -i "$WAV" -af volumedetect -f null /dev/null 2>&1 | grep -E 'mean_volume|max_volume|Duration'

echo ""
echo "OK. Output: $WAV"
echo "Interpretasi: mean_volume sekitar -20 s/d -6 dBFS = level sehat."
echo "max_volume 0 dB berulang = clipping; turunkan gain di mixer."
