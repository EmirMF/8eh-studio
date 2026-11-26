use crate::{persist_config, Config};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tungstenite::{client, Message, WebSocket};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const AITUM: &str = "aitum-stream-suite";

#[derive(Default)]
pub struct Control {
    endpoint: Option<(u16, String)>,
    session: Option<Session>,
    retry_after: Option<Instant>,
}

struct Session {
    socket: WebSocket<TcpStream>,
    request_id: u64,
}

enum ObsError {
    Connection,
    Authentication,
    Rejected(String),
}

impl ObsError {
    fn message(self) -> String {
        match self {
            Self::Connection => "Koneksi ke OBS terputus atau WebSocket belum aktif.".into(),
            Self::Authentication => "Autentikasi OBS gagal. Periksa kata sandi WebSocket.".into(),
            Self::Rejected(error) => error,
        }
    }
}

impl Session {
    fn connect(port: u16, password: &str) -> Result<Self, ObsError> {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let tcp =
            TcpStream::connect_timeout(&address, IO_TIMEOUT).map_err(|_| ObsError::Connection)?;
        tcp.set_read_timeout(Some(IO_TIMEOUT))
            .map_err(|_| ObsError::Connection)?;
        tcp.set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|_| ObsError::Connection)?;
        let (socket, _) =
            client(format!("ws://{address}"), tcp).map_err(|_| ObsError::Connection)?;
        let mut session = Self {
            socket,
            request_id: 0,
        };
        let hello = session.receive(0, None)?;
        let mut identify = json!({ "rpcVersion": 1, "eventSubscriptions": 0 });
        let requires_password = hello["authentication"].is_object();
        if requires_password {
            let auth = &hello["authentication"];
            let salt = auth["salt"].as_str().ok_or(ObsError::Connection)?;
            let challenge = auth["challenge"].as_str().ok_or(ObsError::Connection)?;
            let secret = STANDARD.encode(Sha256::digest(format!("{password}{salt}")));
            identify["authentication"] =
                json!(STANDARD.encode(Sha256::digest(format!("{secret}{challenge}"))));
        }
        session.send(1, identify)?;
        session.receive(2, None).map_err(|error| {
            if requires_password {
                ObsError::Authentication
            } else {
                error
            }
        })?;
        Ok(session)
    }

    fn send(&mut self, op: u8, data: Value) -> Result<(), ObsError> {
        self.socket
            .send(Message::text(json!({ "op": op, "d": data }).to_string()))
            .map_err(|_| ObsError::Connection)
    }

    fn receive(&mut self, op: u8, request_id: Option<&str>) -> Result<Value, ObsError> {
        let deadline = Instant::now() + IO_TIMEOUT;
        while Instant::now() < deadline {
            let message = self.socket.read().map_err(|_| ObsError::Connection)?;
            if message.is_close() {
                return Err(ObsError::Connection);
            }
            if let Message::Text(text) = message {
                let value: Value = serde_json::from_str(&text).map_err(|_| ObsError::Connection)?;
                let data = &value["d"];
                if value["op"] == op && request_id.is_none_or(|id| data["requestId"] == id) {
                    return Ok(data.clone());
                }
            }
        }
        Err(ObsError::Connection)
    }

    fn request(&mut self, kind: &str, data: Value) -> Result<Value, ObsError> {
        self.request_id += 1;
        let id = self.request_id.to_string();
        self.send(
            6,
            json!({ "requestType": kind, "requestId": id, "requestData": data }),
        )?;
        let reply = self.receive(7, Some(&id))?;
        if reply["requestStatus"]["result"] != true {
            let code = reply["requestStatus"]["code"].as_u64().unwrap_or_default();
            return Err(ObsError::Rejected(format!(
                "OBS menolak perintah {kind} ({code})."
            )));
        }
        Ok(reply["responseData"].clone())
    }

    fn vendor(&mut self, kind: &str, data: Value) -> Result<Value, ObsError> {
        let reply = self.request(
            "CallVendorRequest",
            json!({
                "vendorName": AITUM, "requestType": kind, "requestData": data
            }),
        )?;
        let data = &reply["responseData"];
        if data["success"] != true {
            return Err(ObsError::Rejected(format!("Aitum tidak dapat menjalankan {kind}. Periksa pengaturan canvas dan output di OBS.")));
        }
        Ok(data.clone())
    }

    fn snapshot(&mut self, config: &Config) -> Result<Snapshot, ObsError> {
        let stream = self.request("GetStreamStatus", json!({}))?;
        let scenes = self.request("GetSceneList", json!({}))?;
        let mut snapshot = Snapshot {
            website: OutputStatus {
                active: stream["outputActive"] == true,
                reconnecting: stream["outputReconnecting"] == true,
                bytes: stream["outputBytes"].as_u64().unwrap_or_default(),
            },
            website_scene: scenes["currentProgramSceneName"]
                .as_str()
                .map(str::to_owned),
            website_scenes: scene_names(&scenes["scenes"], "sceneName"),
            ..Snapshot::default()
        };
        let outputs = match self.vendor("get_outputs", json!({})) {
            Ok(outputs) => outputs,
            Err(ObsError::Rejected(error)) => {
                snapshot.aitum_error = Some(error);
                return Ok(snapshot);
            }
            Err(error) => return Err(error),
        };
        snapshot.aitum_available = true;
        let outputs: Vec<AitumOutput> = serde_json::from_value(outputs["outputs"].clone())
            .map_err(|_| ObsError::Rejected("Format output Aitum tidak didukung.".into()))?;
        for output in outputs.into_iter().filter(|output| output.kind == "stream") {
            if Some(&output.name) == config.obs_tiktok_output.as_ref() {
                snapshot.tiktok = Some(OutputStatus {
                    active: output.active,
                    reconnecting: output.reconnecting,
                    bytes: output.total_bytes,
                });
            }
            snapshot.tiktok_outputs.push(output.name);
        }
        let canvases = match self.vendor("get_canvas", json!({})) {
            Ok(canvases) => canvases,
            Err(ObsError::Rejected(error)) => {
                snapshot.aitum_error = Some(error);
                return Ok(snapshot);
            }
            Err(error) => return Err(error),
        };
        snapshot.vertical_canvases =
            serde_json::from_value::<Vec<Canvas>>(canvases["canvas"].clone())
                .map_err(|_| ObsError::Rejected("Format canvas Aitum tidak didukung.".into()))?
                .into_iter()
                .filter(|canvas| canvas.kind == "extra" && canvas.height > canvas.width)
                .collect();
        if let Some(canvas) = config.obs_vertical_canvas.as_ref().filter(|id| {
            snapshot
                .vertical_canvases
                .iter()
                .any(|canvas| &canvas.uuid == *id)
        }) {
            let scenes = self.vendor("get_scenes", json!({ "canvas": canvas }))?;
            let current = self.vendor("current_scene", json!({ "canvas": canvas }))?;
            snapshot.tiktok_scenes = scene_names(&scenes["scenes"], "name");
            snapshot.tiktok_scene = current["scene"].as_str().map(str::to_owned);
        }
        Ok(snapshot)
    }
}

fn scene_names(value: &Value, key: &str) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|scene| scene[key].as_str().map(str::to_owned))
        .collect()
}

#[derive(Default, Serialize)]
struct OutputStatus {
    active: bool,
    reconnecting: bool,
    bytes: u64,
}

#[derive(Deserialize)]
struct AitumOutput {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    active: bool,
    reconnecting: bool,
    total_bytes: u64,
}

#[derive(Deserialize, Serialize)]
struct Canvas {
    uuid: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    width: u32,
    #[serde(default)]
    height: u32,
}

#[derive(Default, Serialize)]
pub struct Snapshot {
    website: OutputStatus,
    website_scenes: Vec<String>,
    website_scene: Option<String>,
    tiktok: Option<OutputStatus>,
    tiktok_outputs: Vec<String>,
    tiktok_scenes: Vec<String>,
    tiktok_scene: Option<String>,
    vertical_canvases: Vec<Canvas>,
    aitum_available: bool,
    aitum_error: Option<String>,
}

impl Control {
    fn run<T>(
        &mut self,
        work: impl FnOnce(&mut Session) -> Result<T, ObsError>,
    ) -> Result<T, String> {
        if self.session.is_none() {
            let (port, password) = self
                .endpoint
                .as_ref()
                .ok_or("Hubungkan Studio ke OBS terlebih dahulu.")?;
            if self.retry_after.is_some_and(|time| Instant::now() < time) {
                return Err("Menunggu koneksi ke OBS…".into());
            }
            self.retry_after = Some(Instant::now() + Duration::from_secs(3));
            self.session = Some(Session::connect(*port, password).map_err(ObsError::message)?);
        }
        let result = work(self.session.as_mut().unwrap());
        if matches!(&result, Err(ObsError::Connection)) {
            self.session = None;
        }
        result.map_err(ObsError::message)
    }
}

fn authorized_config(app: &AppHandle) -> Result<Config, String> {
    let config = app.state::<Mutex<Config>>().lock().unwrap().clone();
    if config.scope != "broadcast" || config.token.is_empty() || config.logged_user.is_empty() {
        return Err("Akses siaran diperlukan untuk mengontrol OBS.".into());
    }
    Ok(config)
}

async fn run_control<T: Send + 'static>(
    app: AppHandle,
    work: impl FnOnce(&mut Control, &Config, &AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<Mutex<Control>>();
        let mut control = state.lock().unwrap();
        let config = match authorized_config(&app) {
            Ok(config) => config,
            Err(error) => {
                *control = Control::default();
                return Err(error);
            }
        };
        work(&mut control, &config, &app)
    })
    .await
    .map_err(|_| "Pekerjaan OBS berhenti.".to_string())?
}

pub fn disconnect(app: &AppHandle) {
    *app.state::<Mutex<Control>>().lock().unwrap() = Control::default();
}

#[tauri::command]
pub async fn obs_connect(app: AppHandle, port: u16, password: String) -> Result<Snapshot, String> {
    if port == 0 {
        return Err("Port OBS harus antara 1 dan 65535.".into());
    }
    run_control(app, move |control, config, app| {
        let session = Session::connect(port, &password).map_err(ObsError::message)?;
        let mut connected = Control {
            endpoint: Some((port, password)),
            session: Some(session),
            retry_after: None,
        };
        let snapshot = connected.run(|session| session.snapshot(config))?;
        *control = connected;
        let state = app.state::<Mutex<Config>>();
        let mut saved = state.lock().unwrap();
        saved.obs_port = Some(port);
        persist_config(&saved);
        Ok(snapshot)
    })
    .await
}

#[tauri::command]
pub async fn obs_disconnect(app: AppHandle) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || disconnect(&app))
        .await
        .map_err(|_| "Gagal memutus koneksi kontrol OBS.".to_string())
}

#[tauri::command]
pub async fn obs_status(app: AppHandle) -> Result<Snapshot, String> {
    run_control(app, |control, config, _| {
        control.run(|session| session.snapshot(config))
    })
    .await
}

#[tauri::command]
pub async fn obs_save_tiktok(
    app: AppHandle,
    output: Option<String>,
    canvas: Option<String>,
) -> Result<(), String> {
    run_control(app, move |control, config, app| {
        // Clearing the selection also works after OBS removes or renames an output.
        if output.is_some() || canvas.is_some() {
            let snapshot = control.run(|session| session.snapshot(config))?;
            if output
                .as_ref()
                .is_some_and(|name| !snapshot.tiktok_outputs.contains(name))
                || canvas.as_ref().is_some_and(|id| {
                    !snapshot
                        .vertical_canvases
                        .iter()
                        .any(|canvas| &canvas.uuid == id)
                })
            {
                return Err("Output atau canvas tidak ditemukan di OBS.".into());
            }
        }
        let state = app.state::<Mutex<Config>>();
        let mut config = state.lock().unwrap();
        config.obs_tiktok_output = output;
        config.obs_vertical_canvas = canvas;
        persist_config(&config);
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Destination {
    Website,
    Tiktok,
}

#[tauri::command]
pub async fn obs_set_stream(
    app: AppHandle,
    destination: Destination,
    start: bool,
) -> Result<(), String> {
    run_control(app, move |control, config, _| {
        // A fresh status poll reconnects. Do not start/stop from a stale UI snapshot.
        let session = control
            .session
            .as_mut()
            .ok_or("Tunggu sampai status OBS tersambung kembali.")?;
        let result = match destination {
            Destination::Website => {
                session.request(if start { "StartStream" } else { "StopStream" }, json!({}))
            }
            Destination::Tiktok => {
                let output = config
                    .obs_tiktok_output
                    .as_ref()
                    .ok_or("Pilih output TikTok terlebih dahulu.")?;
                if start {
                    let snapshot = control.run(|session| session.snapshot(config))?;
                    let valid_canvas = snapshot
                        .vertical_canvases
                        .iter()
                        .any(|canvas| Some(&canvas.uuid) == config.obs_vertical_canvas.as_ref());
                    if snapshot.tiktok.is_none() || !valid_canvas {
                        return Err(
                            "Pilih output dan canvas TikTok vertikal yang tersedia di OBS.".into(),
                        );
                    }
                }
                control.session.as_mut().unwrap().vendor(
                    if start { "start_output" } else { "stop_output" },
                    json!({ "output": output }),
                )
            }
        };
        finish_command(control, result)
    })
    .await
}

#[tauri::command]
pub async fn obs_set_scene(
    app: AppHandle,
    destination: Destination,
    scene: String,
) -> Result<(), String> {
    run_control(app, move |control, config, _| {
        let session = control
            .session
            .as_mut()
            .ok_or("Tunggu sampai status OBS tersambung kembali.")?;
        let result = match destination {
            Destination::Website => {
                session.request("SetCurrentProgramScene", json!({ "sceneName": scene }))
            }
            Destination::Tiktok => {
                let canvas = config
                    .obs_vertical_canvas
                    .as_ref()
                    .ok_or("Pilih canvas vertikal terlebih dahulu.")?;
                session.vendor("switch_scene", json!({ "canvas": canvas, "scene": scene }))
            }
        };
        finish_command(control, result)
    })
    .await
}

fn finish_command(control: &mut Control, result: Result<Value, ObsError>) -> Result<(), String> {
    if matches!(&result, Err(ObsError::Connection)) {
        control.session = None;
    }
    result.map(|_| ()).map_err(ObsError::message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn authenticates_and_reads_both_outputs_and_scenes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let fake_obs = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            tcp.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            let mut socket = tungstenite::accept(tcp).unwrap();
            socket.send(Message::text(json!({
                "op": 0, "d": { "rpcVersion": 1, "authentication": { "salt": "salt", "challenge": "challenge" } }
            }).to_string())).unwrap();
            let identify: Value =
                serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
            assert_eq!(identify["op"], 1);
            assert_eq!(
                identify["d"]["authentication"],
                "zTM5ki6L2vVvBQiTG9ckH1Lh64AbnCf6XZ226UmnkIA="
            );
            socket
                .send(Message::text(
                    json!({
                        "op": 2, "d": { "negotiatedRpcVersion": 1 }
                    })
                    .to_string(),
                ))
                .unwrap();

            let requests = [
                (
                    "GetStreamStatus",
                    "",
                    json!({ "outputActive": true, "outputBytes": 512 }),
                ),
                (
                    "GetSceneList",
                    "",
                    json!({ "currentProgramSceneName": "Studio", "scenes": [{ "sceneName": "Studio" }] }),
                ),
                (
                    "CallVendorRequest",
                    "get_outputs",
                    json!({ "outputs": [
                    { "name": "TikTok", "type": "stream", "active": true, "reconnecting": true, "total_bytes": 256 },
                    { "name": "Recorder", "type": "record", "active": false, "reconnecting": false, "total_bytes": 0 }
                ] }),
                ),
                (
                    "CallVendorRequest",
                    "get_canvas",
                    json!({ "canvas": [
                    { "type": "extra", "name": "TikTok Portrait", "uuid": "vertical", "width": 720, "height": 1280 },
                    { "type": "clone", "name": "Clone", "uuid": "clone", "width": 720, "height": 1280 }
                ] }),
                ),
                (
                    "CallVendorRequest",
                    "get_scenes",
                    json!({ "scenes": [{ "name": "Portrait" }] }),
                ),
                (
                    "CallVendorRequest",
                    "current_scene",
                    json!({ "scene": "Portrait" }),
                ),
            ];
            for (kind, vendor, mut response) in requests {
                let request: Value =
                    serde_json::from_str(socket.read().unwrap().to_text().unwrap()).unwrap();
                let data = &request["d"];
                assert_eq!(request["op"], 6);
                assert_eq!(data["requestType"], kind);
                if !vendor.is_empty() {
                    assert_eq!(data["requestData"]["vendorName"], AITUM);
                    assert_eq!(data["requestData"]["requestType"], vendor);
                    if matches!(vendor, "get_scenes" | "current_scene") {
                        assert_eq!(data["requestData"]["requestData"]["canvas"], "vertical");
                    }
                    response["success"] = json!(true);
                    response = json!({ "responseData": response });
                }
                socket.send(Message::text(json!({
                    "op": 7, "d": {
                        "requestId": data["requestId"], "requestStatus": { "result": true }, "responseData": response
                    }
                }).to_string())).unwrap();
            }
        });

        let mut session = Session::connect(port, "password")
            .map_err(ObsError::message)
            .unwrap();
        let config = Config {
            obs_tiktok_output: Some("TikTok".into()),
            obs_vertical_canvas: Some("vertical".into()),
            ..Config::default()
        };
        let snapshot = session
            .snapshot(&config)
            .map_err(ObsError::message)
            .unwrap();
        assert!(snapshot.website.active);
        assert_eq!(snapshot.website_scene.as_deref(), Some("Studio"));
        assert_eq!(snapshot.tiktok_outputs, ["TikTok"]);
        assert!(snapshot.tiktok.unwrap().reconnecting);
        assert_eq!(snapshot.tiktok_scene.as_deref(), Some("Portrait"));
        assert_eq!(snapshot.vertical_canvases.len(), 1);
        fake_obs.join().unwrap();
    }
}
