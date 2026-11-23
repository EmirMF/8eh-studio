#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."

case "$(uname -m)" in
  arm64) archive_url="https://ffmpeg.martin-riedl.de/download/macos/arm64/1795207104_9.1/ffmpeg.zip" ;;
  x86_64) archive_url="https://ffmpeg.martin-riedl.de/download/macos/amd64/1795212433_9.1/ffmpeg.zip" ;;
  *) echo "Unsupported Mac architecture" >&2; exit 1 ;;
esac

mkdir -p binaries/macos
if [[ ! -x binaries/macos/ffmpeg ]]; then
  curl -fL --retry 3 "$archive_url" -o binaries/macos/ffmpeg.zip
  curl -fL --retry 3 "$archive_url.sha256" -o binaries/macos/ffmpeg.zip.sha256
  expected="$(awk '{print $1}' binaries/macos/ffmpeg.zip.sha256)"
  actual="$(shasum -a 256 binaries/macos/ffmpeg.zip | awk '{print $1}')"
  [[ "$expected" == "$actual" ]] || { echo "FFmpeg checksum mismatch" >&2; exit 1; }
  unzip -jo binaries/macos/ffmpeg.zip -d binaries/macos
  chmod +x binaries/macos/ffmpeg
fi
binaries/macos/ffmpeg -hide_banner -encoders 2>/dev/null | grep libmp3lame >/dev/null

swiftc -O -parse-as-library -target "$(uname -m)-apple-macos13.0" \
  macos/desktop-capture.swift -o binaries/macos/desktop-capture
chmod +x binaries/macos/desktop-capture

mkdir -p icons/icon.iconset
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" icons/128x128.png --out "icons/icon.iconset/icon_${size}x${size}.png" >/dev/null
done
cp icons/icon.iconset/icon_32x32.png icons/icon.iconset/icon_16x16@2x.png
sips -z 64 64 icons/128x128.png --out icons/icon.iconset/icon_32x32@2x.png >/dev/null
cp icons/icon.iconset/icon_256x256.png icons/icon.iconset/icon_128x128@2x.png
cp icons/icon.iconset/icon_512x512.png icons/icon.iconset/icon_256x256@2x.png
sips -z 1024 1024 icons/128x128.png --out icons/icon.iconset/icon_512x512@2x.png >/dev/null
iconutil -c icns icons/icon.iconset -o icons/icon.icns
