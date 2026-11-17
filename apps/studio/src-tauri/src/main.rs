#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod mixer;

use mixer::RunningMix;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Manager};
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
    // broadcaster — input
    device: String,         // input 1 (utama)
    device2: Option<String>, // input 2 (opsional)
    // bus
    ice_host: String,
    ice_port: u16,
    ice_username: String,
    ice_password: String,
    ice_mount: String,
    bitrate: u32,
    // monitor
    monitor_device: String,
    monitor_enabled: bool,
    // ffmpeg
    ffmpeg_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            url: String::new(),
            token: String::new(),
            device_id: String::new(),
            logged_user: String::new(),
            scope: String::new(),
            metadata_on: true,
            device: String::new(),
            device2: None,
            ice_host: "stream.8ehradioitb.com".into(),
            ice_port: 8000,
            ice_username: "source".into(),
            ice_password: String::new(),
            ice_mount: "/radio.mp3".into(),
            bitrate: 192,
            monitor_device: String::new(),
            monitor_enabled: false,
            ffmpeg_path: "ffmpeg".into(),
        }
    }
}

fn config_dir() -> std::path::PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
    std::path::PathBuf::from(appdata).join("com.eighteh.studio")
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
    program_title: String,
    last_sent_key: String,
    candidate_key: String,
    candidate_seen: Option<SystemTime>,
    candidate_title: String,
    candidate_artist: String,
    last_heartbeat: Option<SystemTime>,
    push_ok: bool,
    last_error: String,
    last_title: String,
}

#[derive(Default, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Auto,
    Program,
}

#[derive(Serialize)]
struct MetadataStatus {
    push_ok: bool,
    last_error: String,
    mode: Mode,
    last_title: String,
}

struct Playback {
    title: String,
    artist: String,
    playing: bool,
}

fn read_playback() -> Option<Playback> {
    let manager = smtc::GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .ok()?
        .get()
        .ok()?;
    let sessions = manager.GetSessions().ok()?;
    let count = sessions.Size().ok()?;
    if count == 0 {
        return None;
    }
    let mut chosen: Option<smtc::GlobalSystemMediaTransportControlsSession> = None;
    for i in 0..count {
        let s = sessions.GetAt(i).ok()?;
        let src = s.SourceAppUserModelId().ok().map(|v| v.to_string())?;
        if src.to_lowercase().contains("spotify") {
            chosen = Some(s);
            break;
        }
        if chosen.is_none() {
            chosen = Some(s);
        }
    }
    let session = chosen?;
    let props = session.TryGetMediaPropertiesAsync().ok()?.get().ok()?;
    let playing = match session.GetPlaybackInfo() {
        Ok(info) => matches!(
            info.PlaybackStatus(),
            Ok(smtc::GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
        ),
        Err(_) => false,
    };
    Some(Playback {
        title: props.Title().ok()?.to_string(),
        artist: props.Artist().unwrap_or_default().to_string(),
        playing,
    })
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
    let need_beat = state
        .last_heartbeat
        .map(|t| t.elapsed().unwrap_or(Duration::MAX) >= HEARTBEAT)
        .unwrap_or(true);
    if need_beat {
        match send_post(client, cfg, serde_json::json!({ "mode": "heartbeat" })) {
            Ok(_) => {
                state.last_heartbeat = Some(SystemTime::now());
                state.push_ok = true;
                state.last_error.clear();
            }
            Err(e) => {
                state.push_ok = false;
                state.last_error = e;
                return;
            }
        }
    }

    if state.mode == Mode::Program {
        let key = format!("prog|{}", state.program_title);
        if key != state.last_sent_key {
            match send_post(client, cfg, serde_json::json!({ "mode": "program", "title": state.program_title })) {
                Ok(_) => {
                    state.last_sent_key = key;
                    state.push_ok = true;
                    state.last_error.clear();
                    state.last_title = state.program_title.clone();
                }
                Err(e) => {
                    state.push_ok = false;
                    state.last_error = e;
                }
            }
        }
        return;
    }

    let pb = match read_playback() {
        Some(p) => p,
        None => {
            try_clear(state, cfg, client);
            return;
        }
    };
    if !pb.playing {
        try_clear(state, cfg, client);
        return;
    }

    let key = format!("{}|{}", pb.title, pb.artist);
    if key == state.last_sent_key {
        return;
    }
    if key != state.candidate_key {
        state.candidate_key = key;
        state.candidate_seen = Some(SystemTime::now());
        state.candidate_title = pb.title;
        state.candidate_artist = pb.artist;
        return;
    }
    if state
        .candidate_seen
        .map(|t| t.elapsed().unwrap_or(Duration::MAX) < DEBOUNCE)
        .unwrap_or(true)
    {
        return;
    }
    match send_post(
        client,
        cfg,
        serde_json::json!({
            "mode": "auto",
            "title": state.candidate_title,
            "artist": state.candidate_artist,
            "playbackState": "Playing",
        }),
    ) {
        Ok(_) => {
            state.last_sent_key = state.candidate_key.clone();
            state.candidate_key.clear();
            state.push_ok = true;
            state.last_error.clear();
            state.last_title = state.candidate_title.clone();
        }
        Err(e) => {
            state.push_ok = false;
            state.last_error = e;
        }
    }
}

fn try_clear(state: &mut EngineState, cfg: &Config, client: &reqwest::blocking::Client) {
    if state.last_sent_key == "clear" {
        return;
    }
    if send_post(client, cfg, serde_json::json!({ "mode": "clear" })).is_ok() {
        state.last_sent_key = "clear".into();
        state.push_ok = true;
        state.last_error.clear();
        state.last_title.clear();
    }
}

// ============================================================
// State UI siaran
// ============================================================

#[derive(Default, Serialize, Clone)]
struct BroadcastUi {
    running: bool,
    started_secs: Option<u64>,
    meters: [f32; 4], // in1, in2, busA, busB (linear RMS)
    log: Vec<String>,
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

// ============================================================
// Login — Google device link (alur /studio-link)
// ============================================================

/// Mulai login: buka browser ke /studio-link?dev=<device_id>.
/// Kru login Google di web → Setujui → app polling link-status → token masuk.
#[tauri::command]
fn login_google(app: AppHandle) -> Result<serde_json::Value, String> {
    let cfg_state = app.state::<Mutex<Config>>();
    let cfg = cfg_state.lock().unwrap().clone();
    if cfg.url.is_empty() {
        return Err("Isi Site URL dulu di bawah, lalu klik Login Google lagi.".into());
    }
    if cfg.device_id.is_empty() {
        return Err("Device ID belum dibuat — restart app.".into());
    }

    let computer = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC".into());
    let link_url = format!(
        "{}/studio-link?dev={}&name={}",
        cfg.url.trim_end_matches('/'),
        cfg.device_id,
        format!("8EH Studio - {computer}")
            .replace(' ', "%20")
    );

    // open browser default
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", &link_url])
            .creation_flags(0x0800_0000)
            .spawn();
    }

    Ok(serde_json::json!({ "opened": true, "linkUrl": link_url }))
}

/// Polling status link (dipanggil UI tiap 2s setelah login_google).
#[tauri::command]
fn link_status(app: AppHandle) -> Result<serde_json::Value, String> {
    let cfg_state = app.state::<Mutex<Config>>();
    let cfg = cfg_state.lock().unwrap().clone();
    if cfg.url.is_empty() || cfg.device_id.is_empty() {
        return Ok(serde_json::json!({ "status": "notfound" }));
    }
    let url = format!(
        "{}/api/studio/link-status?dev={}",
        cfg.url.trim_end_matches('/'),
        cfg.device_id
    );
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(&url)
        .send()
        .map_err(|e| e.to_string())?;
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
    Ok(json)
}

/// Logout lokal (hapus token; device tetap terdaftar di server sampai dicabut).
#[tauri::command]
fn logout(app: AppHandle) {
    let cfg_state = app.state::<Mutex<Config>>();
    let mut cfg = cfg_state.lock().unwrap();
    cfg.token.clear();
    cfg.scope.clear();
    cfg.logged_user.clear();
    persist_config(&cfg);
}

// ============================================================
// Commands mixer / siaran
// ============================================================

#[tauri::command]
fn get_config(state: tauri::State<Mutex<Config>>) -> Config {
    state.inner().lock().unwrap().clone()
}

#[tauri::command]
fn save_config(state: tauri::State<Mutex<Config>>, config: Config) {
    let mut cfg = state.lock().unwrap();
    *cfg = config;
    persist_config(&cfg);
}

#[tauri::command]
fn list_input_devices() -> Vec<String> {
    mixer::list_inputs()
}

#[tauri::command]
fn list_output_devices() -> Vec<String> {
    mixer::list_outputs()
}

#[tauri::command]
fn start_broadcast(app: AppHandle) -> Result<(), String> {
    let cfg = {
        let s = app.state::<Mutex<Config>>();
        let x = s.lock().unwrap().clone(); x
    };
    if cfg.logged_user.is_empty() || cfg.token.is_empty() {
        return Err("Belum login (tab Pengaturan → Login dengan Google)".into());
    }
    if cfg.scope != "broadcast" {
        return Err(
            "Akun kamu hanya berhak kirim metadata (role Music). Siaran hanya untuk Technic/Developer.".into(),
        );
    }

    let ui_state = app.state::<Mutex<BroadcastUi>>();
    {
        let mut ui = ui_state.lock().unwrap();
        ui.push_log("Memulai mixer…".into());
    }

    let mix_state = app.state::<Mutex<Option<RunningMix>>>();
    if mix_state.lock().unwrap().is_some() {
        return Err("Siaran sudah berjalan".into());
    }

    let gains = (1.0f32, 1.0f32, 1.0f32);
    let routes = (0b01u8, 0b01u8);

    let mut rm = mixer::start(&cfg, gains, routes)?;

    // log ffmpeg stderr ke UI
    if let Some(stderr) = rm.ffmpeg.stderr.take() {
        let app_h = app.clone();
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(|l| l.ok()) {
                if line.trim().is_empty() { continue; }
                let ui_state = app_h.state::<Mutex<BroadcastUi>>();
                ui_state.lock().unwrap().push_log(line);
            }
            // EOF = ffmpeg mati
            let ui_state = app_h.state::<Mutex<BroadcastUi>>();
            let mut ui = ui_state.lock().unwrap();
            ui.running = false;
            ui.push_log("Encoder berhenti — cek kredensial/koneksi".into());
        });
    }

    // thread writer mixing sudah dijalankan di dalam mixer::start

    // update UI
    {
        let mut ui = ui_state.lock().unwrap();
        ui.running = true;
        ui.started_secs = Some(rm.shared.started_at.elapsed().as_secs());
        ui.push_log(format!(
            "ON AIR: {} ({} kb/s){}",
            cfg.device,
            cfg.bitrate,
            if cfg.device2.as_deref().map(|d| !d.is_empty()).unwrap_or(false) {
                format!(" + {}", cfg.device2.clone().unwrap())
            } else { String::new() }
        ));
    }

    *mix_state.lock().unwrap() = Some(rm);
    Ok(())
}

#[tauri::command]
fn stop_broadcast(app: AppHandle) -> Result<(), String> {
    let mix_state = app.state::<Mutex<Option<RunningMix>>>();
    mixer::stop(&mix_state);
    let ui_state = app.state::<Mutex<BroadcastUi>>();
    let mut ui = ui_state.lock().unwrap();
    ui.running = false;
    ui.push_log("OFF AIR".into());
    Ok(())
}

#[derive(Serialize, Clone)]
struct BroadcastStatus {
    running: bool,
    secs: u64,
    meters_db: [f32; 4],
    log: Vec<String>,
}

#[tauri::command]
fn get_broadcast_status(app: AppHandle) -> BroadcastStatus {
    let mut running = false;
    let mut secs = 0u64;
    let mut meters = [0f32; 4];
    {
        let mix_state = app.state::<Mutex<Option<RunningMix>>>();
        let mut slot = mix_state.lock().unwrap();
        // bersihkan sesi mati
        let dead = match slot.as_mut() {
            Some(rm) => rm.ffmpeg.try_wait().map(|r| r.is_some()).unwrap_or(true),
            None => false,
        };
        if dead {
            *slot = None;
        }
        if let Some(rm) = slot.as_ref() {
            running = true;
            secs = rm.shared.started_at.elapsed().as_secs();
            let m = rm.shared.meters.lock().unwrap();
            meters = *m;
        }
    }
    let ui_state = app.state::<Mutex<BroadcastUi>>();
    let ui = ui_state.lock().unwrap();
    if ui.running {
        running = true;
    }
    BroadcastStatus {
        running,
        secs,
        meters_db: [
            linear_to_db(meters[0]),
            linear_to_db(meters[1]),
            linear_to_db(meters[2]),
            linear_to_db(meters[3]),
        ],
        log: ui.log.clone(),
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
        p.gain1 = db_to_linear(gain1_db);
        p.gain2 = db_to_linear(gain2_db);
        p.route1 = route1;
        p.route2 = route2;
        p.monitor_gain = db_to_linear(monitor_db);
    }
}

// ============================================================
// Metadata commands
// ============================================================

#[tauri::command]
fn set_mode(state: tauri::State<Mutex<EngineState>>, program: Option<String>) {
    let mut st = state.lock().unwrap();
    match program {
        Some(title) if !title.trim().is_empty() => {
            st.mode = Mode::Program;
            st.program_title = title.trim().to_string();
            st.last_sent_key.clear();
        }
        _ => {
            st.mode = Mode::Auto;
            st.program_title.clear();
            st.last_sent_key.clear();
            st.candidate_key.clear();
        }
    }
}

#[tauri::command]
fn get_metadata_status(state: tauri::State<Mutex<EngineState>>) -> MetadataStatus {
    let st = state.lock().unwrap();
    MetadataStatus {
        push_ok: st.push_ok,
        last_error: st.last_error.clone(),
        mode: st.mode,
        last_title: st.last_title.clone(),
    }
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
        .manage(Mutex::new(EngineState::default()))
        .manage(Mutex::new(BroadcastUi::default()))
        .manage(Mutex::<Option<RunningMix>>::new(None))
        .invoke_handler(tauri::generate_handler![
            login_google,
            link_status,
            logout,
            get_config,
            save_config,
            list_input_devices,
            list_output_devices,
            start_broadcast,
            stop_broadcast,
            get_broadcast_status,
            set_mixer,
            set_mode,
            get_metadata_status
        ])
        .setup(|app| {
            let app_handle = app.handle().clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .expect("reqwest client");
                let mut sync_tick = 0u32;
                loop {
                    let cfg_state = app_handle.state::<Mutex<Config>>();
                    let cfg = cfg_state.lock().unwrap().clone();
                    if !cfg.url.is_empty() && !cfg.token.is_empty() {
                        let engine_state = app_handle.state::<Mutex<EngineState>>();
                        let mut st = engine_state.lock().unwrap();
                        engine_step(&mut st, &cfg, &client);
                    }
                    // tiap 30s: sinkron status device dari server —
                    // TERMASUK token (bisa berotasi kalau device di-approve ulang
                    // dari akun lain), metadataEnabled, dan revoked.
                    sync_tick = sync_tick.wrapping_add(1);
                    if !cfg.url.is_empty() && !cfg.device_id.is_empty() && sync_tick % 30 == 0 {
                        let url = format!(
                            "{}/api/studio/link-status?dev={}",
                            cfg.url.trim_end_matches('/'),
                            cfg.device_id
                        );
                        if let Ok(resp) = client.get(&url).send() {
                            if let Ok(v) = resp.json::<serde_json::Value>() {
                                let mut c = cfg_state.lock().unwrap();
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
                                persist_config(&c);
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                let mix_state = window.state::<Mutex<Option<RunningMix>>>();
                mixer::stop(&mix_state);
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running 8EH Studio");
}
