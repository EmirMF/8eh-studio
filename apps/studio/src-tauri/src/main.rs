#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod mixer;
mod obs;
#[cfg(windows)]
#[path = "loopback.rs"]
mod loopback;
#[cfg(target_os = "macos")]
#[path = "loopback_macos.rs"]
mod loopback;

use mixer::{IcecastStatus, RunningMix};
use serde::{Deserialize, Serialize};
use base64::Engine as _;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
#[cfg(windows)]
use windows::Media::Control as smtc;

// konversi dB → linear (UI kirim dB, mixer pakai linear)
fn db_to_linear(db: f32) -> f32 {
    (10.0f32).powf(db / 20.0)
}

// ============================================================
// Konfigurasi
// ============================================================

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    // metadata agent
    url: String,
    token: String,
    // login (Google device link)
    device_id: String,   // identitas per instalasi app (rahasia perangkat)
    logged_user: String,
    scope: String,       // "broadcast" | "metadata" | "" (belum login)
    metadata_on: bool,  // bisa dimatikan dari dashboard (sinkron tiap heartbeat)
    metadata_source: String,
    obs_port: Option<u16>,
    obs_tiktok_output: Option<String>,
    obs_vertical_canvas: Option<String>,
    // broadcaster — input
    device: String,         // input 1 (utama)
    device2: Option<String>, // input 2 (opsional)
    record_enabled: bool,
    // monitor
    monitor_device: String,
    monitor_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: "https://www.8ehradioitb.com".into(),
            token: String::new(),
            device_id: String::new(),
            logged_user: String::new(),
            scope: String::new(),
            metadata_on: true,
            metadata_source: "spotify".into(),
            obs_port: None,
            obs_tiktok_output: None,
            obs_vertical_canvas: None,
            device: String::new(),
            device2: None,
            record_enabled: true,
            monitor_device: String::new(),
            monitor_enabled: false,
        }
    }
}

fn config_dir() -> std::path::PathBuf {
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME").map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Library/Application Support/com.eighteh.studio");
    }
    #[cfg(windows)]
    {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(appdata).join("com.eighteh.studio")
    }
}

fn load_config_file() -> Config {
    let path = config_dir().join("config.json");
    let mut loaded: Option<Config> = None;
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&text) {
            loaded = Some(cfg);
        }
    }
    let mut cfg = match loaded {
        Some(c) => c,
        None => {
            // migrasi dari companion lama
            let old = std::env::var("APPDATA")
                .ok()
                .map(|d| {
                    std::path::PathBuf::from(d)
                        .join("com.eighteh.companion")
                        .join("config.json")
                })
                .and_then(|p| std::fs::read_to_string(p).ok())
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
            let mut c = Config::default();
            if let Some(old) = old {
                c.url = old.get("url").and_then(|v| v.as_str()).unwrap_or("").into();
                c.token = old.get("token").and_then(|v| v.as_str()).unwrap_or("").into();
            }
            c
        }
    };
    // device_id permanen per instalasi — dipakai untuk Google device link
    if cfg.device_id.is_empty() {
        use rand::RngCore;
        let mut bytes = [0u8; 24];
        rand::thread_rng().fill_bytes(&mut bytes);
        cfg.device_id = bytes.iter().map(|b| format!("{b:02x}")).collect();
        persist_config(&cfg);
    }
    if cfg.url.is_empty() {
        cfg.url = Config::default().url;
    }
    // Persist again so credentials from older Studio versions are removed.
    persist_config(&cfg);
    cfg
}

fn persist_config(cfg: &Config) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(dir.join("config.json"), json);
    }
}

// ============================================================
// Engine metadata (port dari companion-agent yang sudah teruji)
// ============================================================

#[derive(Default)]
struct EngineState {
    mode: Mode,
    auto_source: String,
    program_title: String,
    last_sent_key: String,
    candidate_key: String,
    candidate_seen: Option<SystemTime>,
    candidate_title: String,
    candidate_artist: String,
    candidate_artwork: Option<Artwork>,
    last_heartbeat: Option<SystemTime>,
    push_ok: bool,
    last_error: String,
    last_title: String,
    last_artist: String,
    last_cover: String,
}

#[derive(Default, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Auto,
    Program,
    Fallback,
}

#[derive(Default, Clone)]
struct MetadataMode {
    mode: Mode,
    program_title: String,
}

impl EngineState {
    fn apply_mode(&mut self, request: &MetadataMode) {
        if self.mode == request.mode && self.program_title == request.program_title {
            return;
        }
        self.mode = request.mode;
        self.program_title.clone_from(&request.program_title);
        self.last_sent_key.clear();
        self.last_error.clear();
        reset_candidate(self);
    }
}

#[derive(Default, Clone, Serialize)]
struct MetadataStatus {
    push_ok: bool,
    last_error: String,
    mode: Mode,
    last_title: String,
    last_artist: String,
    last_cover: String,
    fallback_applied: bool,
}

impl From<&EngineState> for MetadataStatus {
    fn from(state: &EngineState) -> Self {
        Self {
            push_ok: state.push_ok,
            last_error: state.last_error.clone(),
            mode: state.mode,
            last_title: state.last_title.clone(),
            last_artist: state.last_artist.clone(),
            last_cover: state.last_cover.clone(),
            fallback_applied: state.mode == Mode::Fallback && state.last_sent_key == "clear",
        }
    }
}

#[derive(Serialize)]
struct MetadataSource {
    id: String,
    label: String,
}

fn fallback_title(title: &str) -> bool {
    title.trim().is_empty()
        || title.split(|c: char| !c.is_alphanumeric()).any(|word| {
            word.eq_ignore_ascii_case("jingle") || word.eq_ignore_ascii_case("bumper")
        })
}

#[cfg(test)]
mod metadata_tests {
    use super::*;

    #[test]
    fn old_settings_default_to_spotify_desktop() {
        let config: Config = serde_json::from_str(r#"{"record_enabled":false}"#).unwrap();
        assert_eq!(config.metadata_source, "spotify");
        assert!(!config.record_enabled);
    }

    #[test]
    fn only_whole_jingle_and_bumper_words_trigger_fallback() {
        for title in [" ", "JINGLE_01", "8EH - Bumper Pagi", "[BumPer]", "Radio/Jingle"] {
            assert!(fallback_title(title), "{title}");
        }
        for title in ["Jingles", "Bumpercar", "A Regular Song"] {
            assert!(!fallback_title(title), "{title}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn spotify_lock_distinguishes_desktop_from_web() {
        assert!(spotify_session("Spotify.exe"));
        assert!(spotify_session("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify"));
        assert!(!spotify_session("Chrome.Spotify"));
        assert!(!spotify_session("NotSpotify.exe"));
    }
}

struct Playback {
    title: String,
    artist: String,
    playing: bool,
    artwork: Option<Artwork>,
}

const MAX_ARTWORK_BYTES: u64 = 512 * 1024;

struct Artwork {
    mime_type: &'static str,
    data: String,
    fingerprint: u64,
}

fn artwork_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(windows)]
fn read_artwork(props: &smtc::GlobalSystemMediaTransportControlsSessionMediaProperties) -> Option<Artwork> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()?;
    if size == 0 || size > MAX_ARTWORK_BYTES {
        return None;
    }
    let input = stream.GetInputStreamAt(0).ok()?;
    let reader = windows::Storage::Streams::DataReader::CreateDataReader(&input).ok()?;
    if reader.LoadAsync(size as u32).ok()?.get().ok()? != size as u32 {
        return None;
    }
    let mut bytes = vec![0; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    let mime_type = artwork_mime(&bytes)?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(Artwork {
        mime_type,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        fingerprint: hasher.finish(),
    })
}

#[cfg(windows)]
fn windows_sessions() -> Option<Vec<(String, smtc::GlobalSystemMediaTransportControlsSession)>> {
    let manager = smtc::GlobalSystemMediaTransportControlsSessionManager::RequestAsync().ok()?.get().ok()?;
    let sessions = manager.GetSessions().ok()?;
    Some((0..sessions.Size().ok()?).filter_map(|index| {
        let session = sessions.GetAt(index).ok()?;
        let id = session.SourceAppUserModelId().ok()?.to_string();
        Some((id, session))
    }).collect())
}

#[cfg(windows)]
fn spotify_session(id: &str) -> bool {
    id.eq_ignore_ascii_case("Spotify.exe")
        || id.eq_ignore_ascii_case("SpotifyAB.SpotifyMusic_zpdnekdrzrea0!Spotify")
}

#[tauri::command]
fn list_metadata_sources() -> Vec<MetadataSource> {
    let mut sources = vec![MetadataSource { id: "spotify".into(), label: "Spotify Desktop".into() }];
    #[cfg(target_os = "macos")]
    sources.push(MetadataSource { id: "music".into(), label: "Apple Music".into() });
    #[cfg(windows)]
    for (id, _) in windows_sessions().unwrap_or_default() {
        let key = format!("windows:{}", id.to_lowercase());
        if !id.is_empty() && !spotify_session(&id) && !sources.iter().any(|source| source.id == key) {
            sources.push(MetadataSource { id: key, label: id });
        }
    }
    sources
}

#[cfg(windows)]
fn read_playback(source: &str) -> Option<Playback> {
    let is_playing = |session: &smtc::GlobalSystemMediaTransportControlsSession| {
        session.GetPlaybackInfo().ok().and_then(|info| info.PlaybackStatus().ok())
            == Some(smtc::GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
    };
    let (_, session) = windows_sessions()?.into_iter().find(|(id, session)| {
        let matches = if source == "spotify" {
            spotify_session(id)
        } else {
            source.strip_prefix("windows:").is_some_and(|selected| id.to_lowercase() == selected)
        };
        matches && is_playing(session)
    })?;
    let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let playing = is_playing(&session);
    let title = props.Title().ok()?.to_string();
    let artwork = if playing && !fallback_title(&title) { read_artwork(&props) } else { None };
    Some(Playback {
        title,
        artist: props.Artist().unwrap_or_default().to_string(),
        playing,
        artwork,
    })
}

#[cfg(target_os = "macos")]
fn read_playback(source: &str) -> Option<Playback> {
    #[derive(Deserialize)]
    struct MacTrack { title: String, artist: String, playing: bool, artwork_url: Option<String> }
    let app = match source { "spotify" => "Spotify", "music" => "Music", _ => return None };
    let output = std::process::Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-e", include_str!("metadata_macos.js"), app])
        .output().ok()?;
    if !output.status.success() { return None; }
    let track: MacTrack = serde_json::from_slice(&output.stdout).ok()?;
    let artwork = track.artwork_url.filter(|_| track.playing && !fallback_title(&track.title)).and_then(|url| {
        let response = reqwest::blocking::Client::builder().timeout(Duration::from_secs(2)).build().ok()?
            .get(url).send().ok()?;
        if !response.status().is_success() || response.content_length().unwrap_or(0) > MAX_ARTWORK_BYTES { return None; }
        let bytes = response.bytes().ok()?;
        if bytes.len() as u64 > MAX_ARTWORK_BYTES { return None; }
        let mime_type = artwork_mime(&bytes)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        Some(Artwork { mime_type, data: base64::engine::general_purpose::STANDARD.encode(&bytes), fingerprint: hasher.finish() })
    });
    Some(Playback { title: track.title, artist: track.artist, playing: track.playing, artwork })
}

fn send_post(client: &reqwest::blocking::Client, cfg: &Config, body: serde_json::Value) -> Result<serde_json::Value, String> {
    let resp = client
        .post(format!("{}/api/studio/now-playing", cfg.url.trim_end_matches('/')))
        .header("X-Device-Token", &cfg.token)
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {} {}", status.as_u16(), json["error"].as_str().unwrap_or("")));
    }
    Ok(json)
}

const DEBOUNCE: Duration = Duration::from_millis(2000);
const HEARTBEAT: Duration = Duration::from_secs(60);

fn engine_step(state: &mut EngineState, cfg: &Config, client: &reqwest::blocking::Client) {
    // device dimatikan metadata-nya dari dashboard / scope tanpa metadata → diem
    if !cfg.metadata_on || cfg.token.is_empty() {
        state.push_ok = false;
        state.last_error = if cfg.token.is_empty() {
            "Belum login".into()
        } else {
            "Metadata device ini dimatikan dari dashboard".into()
        };
        return;
    }
    if state.mode == Mode::Fallback {
        try_clear(state, cfg, client);
        return;
    }

    if state.mode == Mode::Program {
        let key = format!("prog|{}", state.program_title);
        if key != state.last_sent_key {
            match send_post(client, cfg, serde_json::json!({ "mode": "program", "title": state.program_title })) {
                Ok(response) => {
                    state.last_sent_key = key;
                    state.last_heartbeat = Some(SystemTime::now());
                    state.push_ok = true;
                    state.last_error.clear();
                    state.last_title = state.program_title.clone();
                    state.last_artist.clear();
                    state.last_cover = response["coverImage"].as_str().unwrap_or("").to_string();
                }
                Err(e) => {
                    state.push_ok = false;
                    state.last_error = e;
                }
            }
        } else {
            refresh_metadata_lease(state, cfg, client);
        }
        return;
    }

    if state.auto_source != cfg.metadata_source {
        if !state.auto_source.is_empty() {
            reset_candidate(state);
            try_clear(state, cfg, client);
            if state.last_sent_key != "clear" {
                return;
            }
        }
        state.auto_source = cfg.metadata_source.clone();
    }
    let Some(pb) = read_playback(&cfg.metadata_source).filter(|p| p.playing && !fallback_title(&p.title)) else {
        try_clear(state, cfg, client);
        return;
    };

    let key = format!("auto|{}|{}|{}", pb.title, pb.artist, pb.artwork.as_ref().map(|a| a.fingerprint).unwrap_or(0));
    if key == state.last_sent_key {
        reset_candidate(state);
        refresh_metadata_lease(state, cfg, client);
        return;
    }
    if key != state.candidate_key {
        state.candidate_key = key;
        state.candidate_seen = Some(SystemTime::now());
        state.candidate_title = pb.title;
        state.candidate_artist = pb.artist;
        state.candidate_artwork = pb.artwork;
        return;
    }
    if state
        .candidate_seen
        .map(|t| t.elapsed().unwrap_or(Duration::MAX) < DEBOUNCE)
        .unwrap_or(true)
    {
        return;
    }
    let mut body = serde_json::json!({
        "mode": "auto",
        "title": state.candidate_title,
        "artist": state.candidate_artist,
        "playbackState": "Playing",
    });
    if let Some(artwork) = &state.candidate_artwork {
        body["artwork"] = serde_json::json!({ "mimeType": artwork.mime_type, "data": artwork.data });
    }
    match send_post(
        client,
        cfg,
        body,
    ) {
        Ok(response) => {
            state.last_sent_key = state.candidate_key.clone();
            state.last_heartbeat = Some(SystemTime::now());
            state.candidate_key.clear();
            state.candidate_artwork = None;
            state.push_ok = true;
            state.last_error.clear();
            state.last_title = state.candidate_title.clone();
            state.last_artist = state.candidate_artist.clone();
            state.last_cover = response["coverImage"].as_str().unwrap_or("").to_string();
        }
        Err(e) => {
            state.push_ok = false;
            state.last_error = e;
        }
    }
}

fn reset_candidate(state: &mut EngineState) {
    state.candidate_key.clear();
    state.candidate_seen = None;
    state.candidate_artwork = None;
}

fn refresh_metadata_lease(state: &mut EngineState, cfg: &Config, client: &reqwest::blocking::Client) {
    if state.last_heartbeat.is_some_and(|time| time.elapsed().unwrap_or_default() < HEARTBEAT) {
        return;
    }
    state.last_heartbeat = Some(SystemTime::now());
    match send_post(client, cfg, serde_json::json!({ "mode": "heartbeat" })) {
        Ok(response) if response["locked"] == true => {
            state.last_sent_key.clear();
            state.push_ok = false;
            state.last_error = "Dashboard sedang mengontrol metadata".into();
        }
        Ok(_) => {
            state.push_ok = true;
            state.last_error.clear();
        }
        Err(error) => {
            state.last_sent_key.clear();
            state.push_ok = false;
            state.last_error = error;
        }
    }
}

fn try_clear(state: &mut EngineState, cfg: &Config, client: &reqwest::blocking::Client) {
    reset_candidate(state);
    if state.last_sent_key == "clear" {
        return;
    }
    match send_post(client, cfg, serde_json::json!({ "mode": "clear" })) {
        Ok(response) => {
            state.last_sent_key = "clear".into();
            state.push_ok = true;
            state.last_error.clear();
            state.last_title = response["title"].as_str().unwrap_or_default().into();
            state.last_artist = response["artist"].as_str().unwrap_or_default().into();
            state.last_cover = response["coverImage"].as_str().unwrap_or_default().into();
        }
        Err(error) => {
            state.push_ok = false;
            state.last_error = error;
        }
    }
}

// ============================================================
// State UI siaran
// ============================================================

#[derive(Default, Serialize, Clone)]
struct BroadcastUi {
    log: Vec<String>,
    last_recording: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BroadcastConfig {
    host: String,
    port: u16,
    mount: String,
    username: String,
    password: String,
    bitrate_kbps: u32,
}

fn device_get(cfg: &Config, route: &str) -> Result<reqwest::blocking::Response, String> {
    reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("{}{}", cfg.url.trim_end_matches('/'), route))
        .header("X-Device-Token", &cfg.token)
        .header("Cache-Control", "no-store")
        .send()
        .map_err(|e| format!("Tidak dapat menghubungi situs: {e}"))
}

fn fetch_broadcast_config(cfg: &Config) -> Result<BroadcastConfig, String> {
    let response = device_get(cfg, "/api/studio/broadcast-config")?;
    match response.status().as_u16() {
        401 => return Err("Login perangkat tidak berlaku. Login ulang.".into()),
        403 => return Err("Akun tidak memiliki akses siaran.".into()),
        503 => return Err("Konfigurasi siaran belum siap di situs.".into()),
        code if code >= 400 => return Err(format!("Situs mengembalikan HTTP {code}")),
        _ => {}
    }
    let source: BroadcastConfig = response.json().map_err(|_| "Format konfigurasi siaran tidak valid".to_string())?;
    if source.host.is_empty() || source.port == 0 || source.mount.is_empty()
        || source.username.is_empty() || source.password.is_empty()
        || !(32..=320).contains(&source.bitrate_kbps) {
        return Err("Konfigurasi siaran di situs belum lengkap".into());
    }
    Ok(source)
}

#[tauri::command]
fn validate_login(app: AppHandle) -> Result<serde_json::Value, String> {
    let state = app.state::<Mutex<Config>>();
    let cfg = state.lock().unwrap().clone();
    if cfg.token.is_empty() { return Ok(serde_json::json!({"status":"login"})); }
    let requested_token = cfg.token.clone();
    let requested_url = cfg.url.clone();
    let response = device_get(&cfg, "/api/studio/session")?;
    if response.status().as_u16() == 401 {
        let mut cfg = state.lock().unwrap();
        if cfg.token != requested_token || cfg.url != requested_url {
            return Ok(serde_json::json!({"status":"login"}));
        }
        cfg.token.clear(); cfg.scope.clear(); cfg.logged_user.clear();
        persist_config(&cfg);
        suspend_audio_for_access(&app);
        return Ok(serde_json::json!({"status":"login"}));
    }
    if !response.status().is_success() { return Err(format!("HTTP {}", response.status())); }
    let data: serde_json::Value = response.json().map_err(|e| e.to_string())?;
    let mut cfg = state.lock().unwrap();
    if cfg.token != requested_token || cfg.url != requested_url {
        return Ok(serde_json::json!({"status":"login"}));
    }
    cfg.scope = data["scope"].as_str().unwrap_or("").to_string();
    cfg.logged_user = data["user"]["name"].as_str().unwrap_or("Kru 8EH").to_string();
    cfg.metadata_on = data["metadataEnabled"].as_bool().unwrap_or(false);
    persist_config(&cfg);
    if cfg.scope != "broadcast" {
        suspend_audio_for_access(&app);
    }
    Ok(serde_json::json!({"status":"approved", "scope":cfg.scope}))
}

impl BroadcastUi {
    fn push_log(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 60 {
            self.log.remove(0);
        }
    }
}

fn linear_to_db(lin: f32) -> f32 {
    if lin <= 0.00001 {
        -90.0
    } else {
        20.0 * lin.log10()
    }
}

fn suspend_audio_for_access(app: &AppHandle) {
    let mix = app.state::<Mutex<Option<RunningMix>>>();
    let active = mix.lock().unwrap().as_ref().map(|rm| rm.session_active()).unwrap_or(false);
    if active {
        if let Some(rm) = mix.lock().unwrap().as_mut() { rm.disconnect_stream(); }
    } else {
        let _ = mixer::stop(&mix);
    }
}

// ============================================================
// Login — Google device link (alur /studio-link)
// ============================================================

/// Mulai login: buka browser ke /studio-link?dev=<device_id>.
/// Kru login Google di web → Setujui → app polling link-status → token masuk.
#[tauri::command]
fn login_google(app: AppHandle) -> Result<serde_json::Value, String> {
    if app.state::<Mutex<Option<RunningMix>>>().lock().unwrap().as_ref().map(|rm| rm.session_active()).unwrap_or(false) {
        return Err("Akhiri sesi sebelum login ulang".into());
    }
    let _ = mixer::stop(&app.state::<Mutex<Option<RunningMix>>>());
    let cfg_state = app.state::<Mutex<Config>>();
    let cfg = cfg_state.lock().unwrap().clone();
    if cfg.url.is_empty() {
        return Err("Isi Site URL dulu di bawah, lalu klik Login Google lagi.".into());
    }
    if cfg.device_id.is_empty() {
        return Err("Device ID belum dibuat — restart app.".into());
    }
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let attempt: String = bytes.iter().map(|b| format!("{b:02x}")).collect();

    let computer = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "8EH Studio".into());
    let mut link_url = reqwest::Url::parse(&format!("{}/studio-link", cfg.url.trim_end_matches('/')))
        .map_err(|_| "URL situs tidak valid".to_string())?;
    link_url.query_pairs_mut()
        .append_pair("dev", &cfg.device_id)
        .append_pair("name", &format!("8EH Studio - {computer}"))
        .append_pair("sid", &attempt);

    // ShellExecute memakai asosiasi URL Windows, bukan membuka File Explorer.
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide: Vec<u16> = std::ffi::OsStr::new(link_url.as_str()).encode_wide().chain(std::iter::once(0)).collect();
        let opened = unsafe {
            ShellExecuteW(None, windows::core::w!("open"), PCWSTR(wide.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL)
        };
        if opened.0 as usize <= 32 {
            return Err("Windows tidak dapat membuka browser default untuk login".into());
        }
    }
    #[cfg(target_os = "macos")]
    std::process::Command::new("/usr/bin/open").arg(link_url.as_str())
        .spawn().map_err(|e| format!("Browser tidak dapat dibuka: {e}"))?;
    {
        let cfg_state = app.state::<Mutex<Config>>();
        let mut current = cfg_state.lock().unwrap();
        current.token.clear();
        current.scope.clear();
        current.logged_user.clear();
        persist_config(&current);
    }
    *app.state::<Mutex<Option<String>>>().lock().unwrap() = Some(attempt);

    Ok(serde_json::json!({ "opened": true, "linkUrl": link_url.as_str() }))
}

/// Polling status link (dipanggil UI tiap 2s setelah login_google).
#[tauri::command]
fn link_status(app: AppHandle) -> Result<serde_json::Value, String> {
    let cfg_state = app.state::<Mutex<Config>>();
    let cfg = cfg_state.lock().unwrap().clone();
    if cfg.url.is_empty() || cfg.device_id.is_empty() {
        return Ok(serde_json::json!({ "status": "notfound" }));
    }
    let attempt = match app.state::<Mutex<Option<String>>>().lock().unwrap().clone() {
        Some(attempt) => attempt,
        None => return Ok(serde_json::json!({ "status": "notfound" })),
    };
    let url = format!(
        "{}/api/studio/link-status?sid={}",
        cfg.url.trim_end_matches('/'),
        attempt
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() { return Err(format!("HTTP {}", resp.status())); }
    let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;

    // approved → simpan token + scope
    if json["status"] == "approved" {
        let token = json["token"].as_str().unwrap_or("").to_string();
        let scope = json["scope"].as_str().unwrap_or("").to_string();
        let name = json["user"]["name"].as_str().unwrap_or("Kru 8EH").to_string();
        if token.is_empty() {
            return Ok(serde_json::json!({ "status": "revoked" }));
        }
        let cfg_state = app.state::<Mutex<Config>>();
        let mut cfg = cfg_state.lock().unwrap();
        cfg.token = token;
        cfg.scope = scope;
        cfg.logged_user = name;
        cfg.metadata_on = json["metadataEnabled"].as_bool().unwrap_or(true);
        persist_config(&cfg);
    }
    if matches!(json["status"].as_str(), Some("approved" | "denied" | "revoked")) {
        *app.state::<Mutex<Option<String>>>().lock().unwrap() = None;
    }
    Ok(json)
}

/// Logout lokal (hapus token; device tetap terdaftar di server sampai dicabut).
#[tauri::command]
fn logout(app: AppHandle) {
    *app.state::<Mutex<Option<String>>>().lock().unwrap() = None;
    let cfg_state = app.state::<Mutex<Config>>();
    let mut cfg = cfg_state.lock().unwrap();
    cfg.token.clear();
    cfg.scope.clear();
    cfg.logged_user.clear();
    persist_config(&cfg);
    drop(cfg);
    obs::disconnect(&app);
    suspend_audio_for_access(&app);
}

// ============================================================
// Commands mixer / siaran
// ============================================================

#[tauri::command]
fn get_config(state: tauri::State<Mutex<Config>>) -> Config {
    state.inner().lock().unwrap().clone()
}

#[tauri::command]
fn save_config(state: tauri::State<Mutex<Config>>, mut config: Config) {
    let mut cfg = state.lock().unwrap();
    config.metadata_source.clone_from(&cfg.metadata_source);
    config.obs_port = cfg.obs_port;
    config.obs_tiktok_output.clone_from(&cfg.obs_tiktok_output);
    config.obs_vertical_canvas.clone_from(&cfg.obs_vertical_canvas);
    *cfg = config;
    persist_config(&cfg);
}

#[tauri::command]
fn set_metadata_source(state: tauri::State<Mutex<Config>>, source: String) -> Result<(), String> {
    let supported = source == "spotify"
        || (cfg!(target_os = "macos") && source == "music")
        || (cfg!(windows) && source.strip_prefix("windows:").is_some_and(|id| !id.is_empty()));
    if !supported {
        return Err("Sumber Now Playing tidak didukung.".into());
    }
    let mut cfg = state.lock().unwrap();
    cfg.metadata_source = source;
    persist_config(&cfg);
    Ok(())
}

#[tauri::command]
fn list_input_devices() -> Vec<String> {
    mixer::list_inputs()
}

#[tauri::command]
fn list_output_devices() -> Vec<String> {
    mixer::list_outputs()
}

fn bundled_ffmpeg(app: &AppHandle) -> Result<std::path::PathBuf, String> {
    let bundled = app.path().resource_dir().map_err(|e| e.to_string())?
        .join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
    if bundled.is_file() { return Ok(bundled); }
    #[cfg(all(debug_assertions, target_os = "macos"))]
    {
        let local = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries/macos/ffmpeg");
        if local.is_file() { return Ok(local); }
    }
    #[cfg(debug_assertions)]
    { return Ok(std::path::PathBuf::from("ffmpeg")); }
    #[cfg(not(debug_assertions))]
    { Err("FFmpeg tidak ditemukan dalam instalasi Studio".into()) }
}

fn bounded_db(db: f32) -> f32 { if db.is_finite() { db.clamp(-60.0, 24.0) } else { 0.0 } }

fn prepare_audio_engine(
    slot: &mut Option<RunningMix>, cfg: &Config,
    gains: (f32, f32, f32), routes: (u8, u8),
) -> Result<bool, String> {
    if cfg.device.is_empty() { return Ok(false); }
    if let Some(rm) = slot.as_ref() {
        if rm.matches_devices(cfg) { return Ok(true); }
        if rm.session_active() { return Err("Pilihan perangkat baru berlaku setelah sesi diakhiri".into()); }
    }
    if let Some(mut old) = slot.take() { old.stop_preview(); }
    *slot = Some(mixer::start_preview(cfg, gains, routes)?);
    Ok(true)
}

#[tauri::command]
fn ensure_preview(
    app: AppHandle, gain1_db: f32, gain2_db: f32,
    route1: u8, route2: u8, monitor_db: f32,
) -> Result<bool, String> {
    let cfg = app.state::<Mutex<Config>>().lock().unwrap().clone();
    if cfg.token.is_empty() || cfg.scope != "broadcast" { return Ok(false); }
    let state = app.state::<Mutex<Option<RunningMix>>>();
    let mut slot = state.lock().unwrap();
    let gains = (db_to_linear(bounded_db(gain1_db)), db_to_linear(bounded_db(gain2_db)), db_to_linear(bounded_db(monitor_db)));
    let ready = prepare_audio_engine(&mut slot, &cfg, gains, (route1 & 3, route2 & 3))?;
    if let Some(rm) = slot.as_ref() {
        let mut p = rm.shared.params.lock().unwrap();
        p.gain1 = gains.0; p.gain2 = gains.1; p.monitor_gain = gains.2;
        p.route1 = route1 & 3; p.route2 = route2 & 3;
    }
    Ok(ready)
}

#[tauri::command(async)]
fn start_broadcast(app: AppHandle) -> Result<(), String> {
    let cfg = app.state::<Mutex<Config>>().lock().unwrap().clone();
    if cfg.token.is_empty() || cfg.logged_user.is_empty() { return Err("Login dulu untuk memulai sesi".into()); }
    if cfg.scope != "broadcast" { return Err("Akun tidak memiliki akses siaran".into()); }
    let mix_state = app.state::<Mutex<Option<RunningMix>>>();
    if mix_state.lock().unwrap().as_ref().map(|rm| rm.session_active()).unwrap_or(false) { return Err("Sesi sudah berjalan".into()); }
    let source = fetch_broadcast_config(&cfg)?;
    let ffmpeg = bundled_ffmpeg(&app)?;
    let recording_dir = app.path().audio_dir().map_err(|e| e.to_string())?.join("8EH");
    let mut slot = mix_state.lock().unwrap();
    if !prepare_audio_engine(&mut slot, &cfg, (1.0, 1.0, 1.0), (1, 1))? { return Err("Input 1 belum dipilih".into()); }
    let rm = slot.as_mut().ok_or("Pratinjau audio belum tersedia")?;
    rm.start_session(&cfg, &ffmpeg, &source, &recording_dir)?;
    let stream_error = rm.stream_error.clone();
    drop(slot);
    let ui_state = app.state::<Mutex<BroadcastUi>>();
    let mut ui = ui_state.lock().unwrap();
    ui.push_log(if cfg.record_enabled { "Sesi dimulai: merekam dan menghubungkan Icecast".into() }
        else { "Sesi siaran dimulai: menghubungkan Icecast".into() });
    if let Some(error) = stream_error {
        ui.push_log(error);
    }
    Ok(())
}

#[tauri::command(async)]
fn retry_broadcast(app: AppHandle) -> Result<(), String> {
    let cfg = app.state::<Mutex<Config>>().lock().unwrap().clone();
    if cfg.scope != "broadcast" || cfg.token.is_empty() { return Err("Login siaran diperlukan".into()); }
    let source = fetch_broadcast_config(&cfg)?;
    let ffmpeg = bundled_ffmpeg(&app)?;
    let state = app.state::<Mutex<Option<RunningMix>>>();
    let mut slot = state.lock().unwrap();
    let rm = slot.as_mut().ok_or("Belum ada sesi siaran")?;
    rm.retry_stream(&ffmpeg, &source)?;
    app.state::<Mutex<BroadcastUi>>().lock().unwrap().push_log("Mencoba menghubungkan siaran lagi".into());
    Ok(())
}

#[tauri::command]
fn stop_broadcast(app: AppHandle) -> Result<Option<String>, String> {
    let state = app.state::<Mutex<Option<RunningMix>>>();
    let mut slot = state.lock().unwrap();
    let saved = slot.as_mut().ok_or("Belum ada sesi siaran")?.finish_session()?;
    drop(slot);
    let saved = saved.map(|p| p.to_string_lossy().to_string());
    let ui_state = app.state::<Mutex<BroadcastUi>>();
    let mut ui = ui_state.lock().unwrap();
    if let Some(path) = &saved {
        ui.last_recording = path.clone();
        ui.push_log(format!("Rekaman tersimpan: {path}"));
    } else {
        ui.push_log("Sesi siaran diakhiri tanpa rekaman".into());
    }
    Ok(saved)
}

#[tauri::command]
fn open_recording_folder(app: AppHandle) -> Result<(), String> {
    let path = app.path().audio_dir().map_err(|e| e.to_string())?.join("8EH");
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    std::process::Command::new(if cfg!(windows) { "explorer.exe" } else { "open" })
        .arg(path).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Serialize, Clone)]
struct BroadcastStatus {
    running: bool,
    preview_active: bool,
    stream_status: IcecastStatus,
    stream_error: Option<String>,
    record_live: bool,
    record_overrun: bool,
    record_enabled: bool,
    secs: u64,
    meters_db: [f32; 4],
    clipping: [bool; 2],
    monitor_error: Option<String>,
    log: Vec<String>,
    last_recording: String,
}

#[tauri::command]
fn get_broadcast_status(app: AppHandle) -> BroadcastStatus {
    let mut running = false;
    let mut preview_active = false;
    let mut stream_status = IcecastStatus::OffAir;
    let mut stream_error = None;
    let mut record_live = false;
    let mut record_overrun = false;
    let mut record_enabled = false;
    let mut secs = 0;
    let mut meters = [0f32; 4];
    let mut clipping = [false; 2];
    let mut monitor_error = None;
    if let Some(rm) = app.state::<Mutex<Option<RunningMix>>>().lock().unwrap().as_mut() {
        rm.refresh_record();
        preview_active = true;
        running = rm.session_active();
        stream_status = rm.stream_status();
        stream_error = rm.stream_error.clone();
        record_live = rm.shared.record_live.load(std::sync::atomic::Ordering::SeqCst);
        record_overrun = rm.shared.record_overrun.load(std::sync::atomic::Ordering::SeqCst);
        record_enabled = rm.record_enabled;
        secs = rm.session_started_at.map(|started| started.elapsed().as_secs()).unwrap_or(0);
        meters = *rm.shared.meters.lock().unwrap();
        clipping = rm.shared.last_clip.lock().unwrap().map(|time| time.map(|t| t.elapsed().as_secs_f32() < 1.0).unwrap_or(false));
        monitor_error = rm.shared.monitor_error.lock().unwrap().clone();
    }
    let state = app.state::<Mutex<BroadcastUi>>();
    let ui = state.lock().unwrap();
    BroadcastStatus {
        running, preview_active, stream_status, stream_error, record_live, record_overrun, record_enabled, secs,
        meters_db: meters.map(linear_to_db), clipping, monitor_error,
        log: ui.log.clone(),
        last_recording: ui.last_recording.clone(),
    }
}

/// Ubah fader/routing live saat siaran jalan.
#[tauri::command]
fn set_mixer(
    app: AppHandle,
    gain1_db: f32,
    gain2_db: f32,
    route1: u8,
    route2: u8,
    monitor_db: f32,
) {
    let mix_state = app.state::<Mutex<Option<RunningMix>>>();
    let slot = mix_state.lock().unwrap();
    if let Some(rm) = slot.as_ref() {
        let mut p = rm.shared.params.lock().unwrap();
        p.gain1 = db_to_linear(bounded_db(gain1_db));
        p.gain2 = db_to_linear(bounded_db(gain2_db));
        p.route1 = route1 & 3;
        p.route2 = route2 & 3;
        p.monitor_gain = db_to_linear(bounded_db(monitor_db));
    }
}

// ============================================================
// Metadata commands
// ============================================================

#[tauri::command]
fn set_mode(state: tauri::State<Mutex<MetadataMode>>, program: Option<String>) {
    *state.lock().unwrap() = match program {
        Some(title) if !title.trim().is_empty() => MetadataMode {
            mode: Mode::Program,
            program_title: title.trim().into(),
        },
        _ => MetadataMode::default(),
    };
}

#[tauri::command]
fn show_broadcast_title(state: tauri::State<Mutex<MetadataMode>>) {
    *state.lock().unwrap() = MetadataMode { mode: Mode::Fallback, program_title: String::new() };
}

#[tauri::command]
fn get_metadata_status(
    state: tauri::State<Mutex<MetadataStatus>>,
    request: tauri::State<Mutex<MetadataMode>>,
) -> MetadataStatus {
    let mut status = state.lock().unwrap().clone();
    let requested = request.lock().unwrap().mode;
    if status.mode != requested {
        status.mode = requested;
        status.last_error.clear();
        status.fallback_applied = false;
    }
    status
}

// ============================================================
// Main
// ============================================================

fn main() {
    // mode headless
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--list-devices") {
        println!("=== INPUT (WASAPI/cpal) ===");
        for n in mixer::list_inputs() {
            println!("  {n}");
        }
        println!("=== OUTPUT ===");
        for n in mixer::list_outputs() {
            println!("  {n}");
        }
        let cfg = load_config_file();
        println!("=== config tersimpan ===");
        println!("  input1 : {}", cfg.device);
        println!("  input2 : {}", cfg.device2.unwrap_or_default());
        println!("  monitor: {}", cfg.monitor_device);
        match mixer::check_input(&cfg.device) {
            Ok(true) => println!("  input1 : COCOK"),
            Ok(false) => println!("  input1 : TIDAK DITEMUKAN — ganti via Pengaturan"),
            Err(e) => println!("  input1 : error {e}"),
        }
        return;
    }

    tauri::Builder::default()
        .manage(Mutex::new(load_config_file()))
        .manage(Mutex::new(MetadataMode::default()))
        .manage(Mutex::new(MetadataStatus::default()))
        .manage(Mutex::<Option<String>>::new(None))
        .manage(Mutex::new(BroadcastUi::default()))
        .manage(Mutex::new(obs::Control::default()))
        .manage(Mutex::<Option<RunningMix>>::new(None))
        .invoke_handler(tauri::generate_handler![
            login_google,
            link_status,
            validate_login,
            logout,
            get_config,
            save_config,
            list_input_devices,
            list_output_devices,
            ensure_preview,
            start_broadcast,
            retry_broadcast,
            stop_broadcast,
            open_recording_folder,
            get_broadcast_status,
            set_mixer,
            set_mode,
            list_metadata_sources,
            set_metadata_source,
            show_broadcast_title,
            get_metadata_status,
            obs::obs_connect,
            obs::obs_disconnect,
            obs::obs_status,
            obs::obs_save_tiktok,
            obs::obs_set_stream,
            obs::obs_set_scene
        ])
        .setup(|app| {
            #[cfg(windows)]
            if let Some(window) = app.get_webview_window("main") {
                let icon = tauri::image::Image::from_app_icon_resource(128)?;
                window.set_icon(icon)?;
            }
            let stream_app = app.handle().clone();
            std::thread::spawn(move || {
                loop {
                    let state = stream_app.state::<Mutex<Option<RunningMix>>>();
                    let mut mix = state.lock().unwrap();
                    if let Some(error) = mix.as_mut().and_then(RunningMix::refresh_stream) {
                        stream_app.state::<Mutex<BroadcastUi>>().lock().unwrap().push_log(error);
                    }
                    drop(mix);
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
            let app_handle = app.handle().clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .expect("reqwest client");
                let mut sync_tick = 0u32;
                let mut engine = EngineState::default();
                let mut target = (String::new(), String::new(), false);
                loop {
                    let cfg_state = app_handle.state::<Mutex<Config>>();
                    let cfg = cfg_state.lock().unwrap().clone();
                    if target.0 != cfg.url || target.1 != cfg.token || target.2 != cfg.metadata_on {
                        engine = EngineState::default();
                        target = (cfg.url.clone(), cfg.token.clone(), cfg.metadata_on);
                    }
                    let requested = app_handle.state::<Mutex<MetadataMode>>().lock().unwrap().clone();
                    engine.apply_mode(&requested);
                    engine_step(&mut engine, &cfg, &client);
                    *app_handle.state::<Mutex<MetadataStatus>>().lock().unwrap() = (&engine).into();
                    // tiap 30s: sinkron status device dari server —
                    // TERMASUK token (bisa berotasi kalau device di-approve ulang
                    // dari akun lain), metadataEnabled, dan revoked.
                    sync_tick = sync_tick.wrapping_add(1);
                    if !cfg.url.is_empty() && !cfg.device_id.is_empty() && !cfg.token.is_empty() && sync_tick % 30 == 0 {
                        let url = format!(
                            "{}/api/studio/link-status?dev={}",
                            cfg.url.trim_end_matches('/'),
                            cfg.device_id
                        );
                        if let Ok(resp) = client.get(&url).send() {
                            if let Ok(v) = resp.json::<serde_json::Value>() {
                                let mut c = cfg_state.lock().unwrap();
                                // Abaikan respons lama bila pengguna logout atau mulai login baru.
                                if c.token == cfg.token && c.url == cfg.url {
                                if v["status"] == "approved" {
                                    // token bisa ter-rotasi oleh re-approval — selalu segarkan
                                    if let Some(t) = v["token"].as_str() {
                                        if !t.is_empty() {
                                            c.token = t.to_string();
                                        }
                                    }
                                    c.metadata_on =
                                        v["metadataEnabled"].as_bool().unwrap_or(true);
                                    if let Some(s) = v["scope"].as_str() {
                                        c.scope = s.to_string();
                                    }
                                    if let Some(n) = v["user"]["name"].as_str() {
                                        if !n.is_empty() && c.logged_user.is_empty() {
                                            c.logged_user = n.to_string();
                                        }
                                    }
                                } else if v["status"] == "revoked" {
                                    c.token.clear();
                                    c.scope.clear();
                                    c.logged_user.clear();
                                }
                                let may_broadcast = !c.token.is_empty() && c.scope == "broadcast";
                                persist_config(&c);
                                drop(c);
                                if !may_broadcast {
                                    suspend_audio_for_access(&app_handle);
                                }
                                }
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.state::<Mutex<Option<RunningMix>>>().lock().unwrap().as_ref().map(|rm| rm.session_active()).unwrap_or(false) {
                    api.prevent_close();
                    let _ = window.emit("session-close-requested", ());
                } else {
                    let _ = mixer::stop(&window.state::<Mutex<Option<RunningMix>>>());
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running 8EH Studio");
}
