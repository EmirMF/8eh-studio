function read(name) {
  try {
    const app = Application(name);
    if (!app.running()) return null;
    const state = String(app.playerState());
    if (state !== "playing" && state !== "paused") return null;
    const track = app.currentTrack();
    const result = {
      title: String(track.name()),
      artist: String(track.artist()),
      playing: state === "playing",
      artwork_url: null
    };
    if (name === "Spotify") {
      try { result.artwork_url = String(track.artworkUrl()); } catch (_) {}
    }
    return result;
  } catch (_) { return null; }
}
function run(args) {
  return JSON.stringify(read(args[0]));
}
