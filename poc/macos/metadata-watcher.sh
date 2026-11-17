#!/bin/bash
# 8EH Stage 1 PoC - macOS metadata watcher (JXA/AppleScript)
# Membaca track aktif dari Spotify.app dan Music.app.
# Run pertama memunculkan prompt izin Automation/AppleEvents - klik OK.
#
# Pakai:
#   ./metadata-watcher.sh            # loop; print JSON saat berubah
#   ./metadata-watcher.sh once        # satu poll, lalu keluar
#
# Output: satu baris JSON per perubahan:
#   {"observedAt":"...ISO-8601...","sessions":[{"source":"Spotify","title":"...","artist":"...","playbackState":"playing"}]}

set -u
INTERVAL_MS=2000
MODE="${1:-loop}"

read_metadata() {
  # shortcut: JXA cukup untuk Spotify + Music; browser perlu pembuktian terpisah
  osascript -l JavaScript -e '
    function pick(name) {
      try {
        const app = Application(name);
        if (!app.running()) return null;
        const state = app.playerState();
        if (state !== "playing" && state !== "paused") return null;
        const t = app.currentTrack();
        return { source: name, title: t.name(), artist: t.artist(), playbackState: state };
      } catch (e) { return null; }
    }
    JSON.stringify([pick("Spotify"), pick("Music")].filter(Boolean));
  ' 2>/dev/null
}

LAST=""
while true; do
  RAW=$(read_metadata)
  # shellcheck disable=SC2181
  if [ $? -ne 0 ] || [ -z "$RAW" ]; then RAW="[]"; fi
  STAMP=$(date -u +"%Y-%m-%dT%H:%M:%SZ")
  SNAP="{\"observedAt\":\"$STAMP\",\"sessions\":$RAW}"
  if [ "$SNAP" != "$LAST" ]; then
    LAST="$SNAP"
    echo "$SNAP"
  fi
  [ "$MODE" = "once" ] && break
  sleep "$(echo "$INTERVAL_MS/1000" | bc -l 2>/dev/null || echo 2)"
done
