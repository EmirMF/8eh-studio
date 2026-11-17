#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};
use tauri::{
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, WebviewWindow,
};
use windows::Media::Control as smtc;

// ---------- state engine ----------

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
struct StatusPayload {
    push_ok: bool,
    last_error: String,
    mode: Mode,
    last_title: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct Config {
    url: String,
    token: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { url: String::new(), token: String::new() }
    }
}

// ---------- SMTC reader ----------

struct Playback {
    title: String,
    artist: String,
    playing: bool,
}

fn read_playback() -> Option<Playback> {
    // shortcut: blocking per poll (1s) di thread engine, bukan di UI.
    let manager = smtc::GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
        .ok()?
        .get()
        .ok()?;
    let sessions = manager.GetSessions().ok()?;
    let count = sessions.Size().ok()?;
    if count == 0 {
        return None;
    }

    // prioritas Spotify, sisanya session pertama
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
        Ok(info) => {
            matches!(
                info.PlaybackStatus(),
                Ok(smtc::GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing)
            )
        }
        Err(_) => false,
    };

    Some(Playback {
        title: props.Title().ok()?.to_string(),
        artist: props.Artist().unwrap_or_default().to_string(),
        playing,
    })
}

// ---------- HTTP ----------

fn send_post(
    client: &reqwest::blocking::Client,
    cfg: &Config,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let resp = client
        .post(format!("{}/api/studio/now-playing", cfg.url.trim_end_matches('/')))
        .header("X-Device-Token", &cfg.token)
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!(
            "HTTP {} {}",
            status.as_u16(),
            json["error"].as_str().unwrap_or("")
        ));
    }
    Ok(json)
}

// ---------- engine step (port dari companion-agent.ps1 yang sudah teruji) ----------

const DEBOUNCE: Duration = Duration::from_millis(2000);
const HEARTBEAT: Duration = Duration::from_secs(60);

fn engine_step(state: &mut EngineState, cfg: &Config, client: &reqwest::blocking::Client) {
    // 1. heartbeat
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
                return; // offline: jangan lanjut push lain
            }
        }
    }

    // 2. program mode
    if state.mode == Mode::Program {
        let key = format!("prog|{}", state.program_title);
        if key != state.last_sent_key {
            match send_post(
                client,
                cfg,
                serde_json::json!({ "mode": "program", "title": state.program_title }),
            ) {
                Ok(_) => {
                    state.last_sent_key = key;
                    state.push_ok = true;
                    state.last_error.clear();
                    last_title_set(state);
                }
                Err(e) => {
                    state.push_ok = false;
                    state.last_error = e;
                }
            }
        }
        return;
    }

    // 3. auto mode
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

fn last_title_set(state: &mut EngineState) {
    state.last_title = state.program_title.clone();
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

// ---------- konfigurasi ----------

fn config_path(app: &AppHandle) -> std::path::PathBuf {
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir).ok();
    dir.join("config.json")
}

fn load_config(app: &AppHandle) -> Config {
    let path = config_path(app);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn persist_config(app: &AppHandle, cfg: &Config) {
    let path = config_path(app);
    if let Ok(json) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(&path, json);
    }
}

// ---------- tauri commands ----------

#[tauri::command]
fn get_config(state: tauri::State<Mutex<Config>>) -> Config {
    state.inner().lock().unwrap().clone()
}

#[tauri::command]
fn save_config(
    app: AppHandle,
    state: tauri::State<Mutex<Config>>,
    url: String,
    token: String,
) {
    let mut cfg = state.lock().unwrap();
    cfg.url = url.trim().to_string();
    cfg.token = token.trim().to_string();
    persist_config(&app, &cfg);
}

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
fn get_status(state: tauri::State<Mutex<EngineState>>) -> StatusPayload {
    let st = state.lock().unwrap();
    StatusPayload {
        push_ok: st.push_ok,
        last_error: st.last_error.clone(),
        mode: st.mode,
        last_title: st.last_title.clone(),
    }
}

// ---------- main ----------

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(EngineState::default()))
        .invoke_handler(tauri::generate_handler![
            get_config,
            save_config,
            set_mode,
            get_status
        ])
        .setup(|app| {
            let cfg: Config = load_config(&app.handle());
            let has_config = !cfg.url.is_empty() && !cfg.token.is_empty();
            app.manage(Mutex::new(cfg));

            // engine thread
            let app_handle = app.handle().clone();
            std::thread::spawn(move || {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(10))
                    .build()
                    .expect("reqwest client");
                loop {
                    let cfg_state = app_handle.state::<Mutex<Config>>();
                    let cfg = cfg_state.lock().unwrap().clone();
                    if !cfg.url.is_empty() && !cfg.token.is_empty() {
                        let engine_state = app_handle.state::<Mutex<EngineState>>();
                        let mut st = engine_state.lock().unwrap();
                        engine_step(&mut st, &cfg, &client);
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
            });

            // tray: klik kiri buka jendela settings
            let settings_win: WebviewWindow = app.get_webview_window("settings").unwrap();
            let show_settings = {
                let w = settings_win.clone();
                move || {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            };

            let tray = TrayIconBuilder::with_id("main")
                .tooltip("8EH Companion")
                .show_menu_on_left_click(false)
                .on_tray_icon_event(move |_tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_settings();
                    }
                })
                .build(app)?;

            let _ = tray.set_icon(Some(app.default_window_icon().unwrap().clone()));

            // belum dikonfigurasi → buka settings saat first run
            if !has_config {
                settings_win.show().ok();
                settings_win.set_focus().ok();
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
