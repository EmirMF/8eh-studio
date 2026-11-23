// 8EH Studio — mesin mixer audio native (WASAPI via cpal).
//
// Topologi (mirip Voicemeeter, 2 input → 2 bus):
//
//   Input 1 (mic/MG16XU) ── gain fader ─┬─→ Bus A ── FFmpeg stdin ──→ Icecast (siaran)
//                                       └─→ Bus B ── WASAPI output ──→ headphone (monitor)
//   Input 2 (mic cadangan) ─ gain fader ─┘     (routing per-channel: A, B, atau A+B)
//
// Semua mixing di Rust pada rate internal 44100 stereo f32.
// Fader/routing diubah live lewat MixerParams (tanpa restart stream).

use crate::{BroadcastConfig, Config};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream};
use std::collections::VecDeque;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

pub const RATE: f32 = 44100.0;
const RING_CAP: usize = 44_100 * 2; // maks 1 detik stereo interleaved (2 * f32 per frame)

// ---------- parameter live ----------

#[derive(Clone, Copy)]
pub struct MixerParams {
    pub gain1: f32, // linear (bukan dB)
    pub gain2: f32,
    pub route1: u8, // bit 1 = bus A, bit 2 = bus B
    pub route2: u8,
    pub monitor_gain: f32,
}

impl Default for MixerParams {
    fn default() -> Self {
        Self {
            gain1: 1.0,
            gain2: 1.0,
            route1: 0b01,
            route2: 0b01,
            monitor_gain: 1.0,
        }
    }
}

// ---------- state bersama antar-thread ----------

pub struct MixerShared {
    ring1: Arc<Mutex<VecDeque<f32>>>,
    ring2: Arc<Mutex<VecDeque<f32>>>,
    monitor: Arc<Mutex<VecDeque<f32>>>,
    pub params: Mutex<MixerParams>,
    pub meters: Mutex<[f32; 4]>, // RMS linear: in1, in2, busA, busB
    pub last_clip: Mutex<[Option<std::time::Instant>; 2]>,
    pub has2: bool,
    pub stop: AtomicBool,
    stream_tx: Mutex<Option<SyncSender<Vec<u8>>>>,
    record_tx: Mutex<Option<SyncSender<Vec<u8>>>>,
    pub stream_live: Arc<AtomicBool>,
    pub record_live: Arc<AtomicBool>,
    pub monitor_error: Arc<Mutex<Option<String>>>,
}

impl MixerShared {
    fn new(has2: bool) -> Arc<Self> {
        Arc::new(Self {
            ring1: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            ring2: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            monitor: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            params: Mutex::new(MixerParams::default()),
            meters: Mutex::new([0.0; 4]),
            last_clip: Mutex::new([None, None]),
            has2,
            stop: AtomicBool::new(false),
            stream_tx: Mutex::new(None),
            record_tx: Mutex::new(None),
            stream_live: Arc::new(AtomicBool::new(false)),
            record_live: Arc::new(AtomicBool::new(false)),
            monitor_error: Arc::new(Mutex::new(None)),
        })
    }
}

// ---------- util ----------

pub(crate) fn push_frames(ring: &Mutex<VecDeque<f32>>, frames: &[(f32, f32)]) {
    let mut q = ring.lock().unwrap();
    for (l, r) in frames {
        q.push_back(*l);
        q.push_back(*r);
    }
    // cap: buang tertua
    while q.len() > RING_CAP {
        q.pop_front();
    }
}

fn take_or_silence(ring: &Mutex<VecDeque<f32>>, frames: usize) -> Vec<f32> {
    let mut q = ring.lock().unwrap();
    // Hindari memutar audio lama setelah callback perangkat sempat tertunda.
    let max_backlog = (RATE as usize / 10) * 2;
    if q.len() > max_backlog {
        let excess = (q.len() - max_backlog) & !1;
        for _ in 0..excess { q.pop_front(); }
    }
    let mut result = vec![0.0; frames * 2];
    for sample in &mut result {
        if let Some(value) = q.pop_front() { *sample = value; } else { break; }
    }
    result
}

pub(crate) struct InputResampler {
    next_position: f64,
    previous: Option<(f32, f32)>,
}

impl InputResampler {
    pub(crate) fn new() -> Self { Self { next_position: 0.0, previous: None } }

    fn process(&mut self, input: &[(f32, f32)], from_rate: f32) -> Vec<(f32, f32)> {
        if input.is_empty() { return Vec::new(); }
        if (from_rate - RATE).abs() < 1.0 {
            self.previous = input.last().copied();
            return input.to_vec();
        }
        let step = from_rate as f64 / RATE as f64;
        let mut position = self.next_position;
        let mut output = Vec::with_capacity((input.len() as f64 / step).ceil() as usize);
        while position < input.len() as f64 {
            let i0 = position.floor() as isize;
            let i1 = i0 + 1;
            if i1 >= input.len() as isize { break; }
            let first = if i0 < 0 { self.previous.unwrap_or(input[0]) } else { input[i0 as usize] };
            let second = input[i1 as usize];
            let t = (position - i0 as f64) as f32;
            output.push((first.0 + (second.0 - first.0) * t, first.1 + (second.1 - first.1) * t));
            position += step;
        }
        self.next_position = position - input.len() as f64;
        self.previous = input.last().copied();
        output
    }
}

fn find_input_device(name: &str) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let devices: Vec<(String, cpal::Device)> = host
        .input_devices()
        .map_err(|e| e.to_string())?
        .filter_map(|d| d.name().ok().map(|n| (n, d)))
        .collect();

    // 1) exact, 2) case-insensitive, 3) contains (arah mana pun) —
    // nama WASAPI bisa terlokalisasi/beda dari dshow.
    let lower = |s: &str| s.to_lowercase().replace(['-', '_'], " ");
    let needle = lower(name);

    if let Some((_, d)) = devices.iter().find(|(n, _)| n == name) {
        return Ok(d.clone());
    }
    if let Some((_, d)) = devices.iter().find(|(n, _)| lower(n) == needle) {
        return Ok(d.clone());
    }
    if let Some((_, d)) = devices
        .iter()
        .find(|(n, _)| lower(n).contains(&needle) || needle.contains(&lower(n)))
    {
        return Ok(d.clone());
    }

    let list: Vec<String> = devices.iter().map(|(n, _)| n.clone()).collect();
    Err(format!(
        "Perangkat input '{name}' tidak ditemukan. Yang tersedia: {}",
        list.join(", ")
    ))
}

fn find_output_device(name: &str) -> Result<cpal::Device, String> {
    let host = cpal::default_host();
    let devices: Vec<(String, cpal::Device)> = host
        .output_devices()
        .map_err(|e| e.to_string())?
        .filter_map(|d| d.name().ok().map(|n| (n, d)))
        .collect();

    let lower = |s: &str| s.to_lowercase().replace(['-', '_'], " ");
    let needle = lower(name);

    if let Some((_, d)) = devices.iter().find(|(n, _)| n == name) {
        return Ok(d.clone());
    }
    if let Some((_, d)) = devices.iter().find(|(n, _)| lower(n) == needle) {
        return Ok(d.clone());
    }
    if let Some((_, d)) = devices
        .iter()
        .find(|(n, _)| lower(n).contains(&needle) || needle.contains(&lower(n)))
    {
        return Ok(d.clone());
    }

    let list: Vec<String> = devices.iter().map(|(n, _)| n.clone()).collect();
    Err(format!(
        "Perangkat output '{name}' tidak ditemukan. Yang tersedia: {}",
        list.join(", ")
    ))
}

fn describe_device_error(kind: &str, name: &str, error: String) -> String {
    if error.contains("0x8889000A") {
        format!("{kind} '{name}' sedang dipakai aplikasi lain (0x8889000A). Tutup aplikasi seperti Voicemeeter yang memakai perangkat secara eksklusif, lalu coba lagi.")
    } else {
        format!("{kind} '{name}': {error}")
    }
}

pub fn list_inputs() -> Vec<String> {
    let host = cpal::default_host();
    host.input_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

pub fn list_outputs() -> Vec<String> {
    let host = cpal::default_host();
    host.output_devices()
        .map(|it| it.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// Cek nama input terhadap daftar WASAPI (untuk diagnosa headless).
/// Ok(true) = ketemu, Ok(false) = tidak.
pub fn check_input(name: &str) -> Result<bool, String> {
    if name.is_empty() {
        return Ok(false);
    }
    find_input_device(name).map(|_| true)
}

/// Bangun input stream; push stereo 44.1k ke ring.
fn _unused() {}

// ---------- sesi mixer berjalan ----------

/// Stream cpal tidak Send dan ber-affinity ke thread pembuatnya —
/// maka stream dibangun DAN dimiliki di thread guard khusus.
pub struct StreamGuard {
    stop_tx: std::sync::mpsc::Sender<()>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl StreamGuard {
    /// Bangun semua stream di thread baru; kirim error build via channel
    /// dan tunggu hasil agar error terdeteksi sebelum start() balik.
    pub fn start(
        build: impl FnOnce() -> Result<Vec<Stream>, String> + Send + 'static,
        monitor_name: String,
        monitor_ring: Arc<Mutex<VecDeque<f32>>>,
        shared: Arc<MixerShared>,
        monitor_block: Option<String>,
    ) -> Result<Self, String> {
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let handle = std::thread::spawn(move || {
            let streams = match build() {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _keep = streams; // stream input hidup selama thread ini hidup
            let mut monitor: Option<Stream> = None;
            let mut last_attempt = std::time::Instant::now() - Duration::from_secs(3);
            loop {
                if stop_rx.recv_timeout(Duration::from_millis(100)).is_ok() { break; }
                let p = *shared.params.lock().unwrap();
                let wants_monitor = p.route1 & 2 != 0 || (shared.has2 && p.route2 & 2 != 0);
                if !wants_monitor {
                    monitor = None;
                    monitor_ring.lock().unwrap().clear();
                    *shared.monitor_error.lock().unwrap() = None;
                    continue;
                }
                if let Some(reason) = monitor_block.as_ref() {
                    *shared.monitor_error.lock().unwrap() = Some(reason.clone());
                    continue;
                }
                if monitor_name.is_empty() {
                    *shared.monitor_error.lock().unwrap() = Some("Pilih output monitor di Pengaturan".into());
                    continue;
                }
                if monitor.is_none() && last_attempt.elapsed() >= Duration::from_secs(2) {
                    last_attempt = std::time::Instant::now();
                    let opened = find_output_device(&monitor_name)
                        .and_then(|device| make_output_stream(&device, &monitor_ring))
                        .and_then(|stream| { stream.play().map_err(|e| e.to_string())?; Ok(stream) });
                    match opened {
                        Ok(stream) => {
                            monitor = Some(stream);
                            *shared.monitor_error.lock().unwrap() = None;
                        }
                        Err(error) => {
                            *shared.monitor_error.lock().unwrap() = Some(describe_device_error("Output monitor", &monitor_name, error));
                        }
                    }
                }
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = handle.join();
                return Err(e);
            }
            Err(_) => {
                return Err("Thread mixer mati saat mulai".into());
            }
        }
        Ok(Self { stop_tx, handle: Some(handle) })
    }

    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct RunningMix {
    pub shared: Arc<MixerShared>,
    pub record_enabled: bool,
    pub session_started_at: Option<std::time::Instant>,
    pub device: String,
    pub device2: Option<String>,
    pub monitor_device: String,
    record: Option<Child>,
    stream: Option<Child>,
    record_writer: Option<std::thread::JoinHandle<()>>,
    mix_writer: Option<std::thread::JoinHandle<()>>,
    recording_temp: Option<std::path::PathBuf>,
    recording_final: Option<std::path::PathBuf>,
    guard: StreamGuard,
    loopback: Option<crate::loopback::LoopbackGuard>,
}

impl RunningMix {
    pub fn session_active(&self) -> bool { self.session_started_at.is_some() }

    pub fn matches_devices(&self, cfg: &Config) -> bool {
        self.device == cfg.device && self.device2 == cfg.device2 && self.monitor_device == cfg.monitor_device
    }

    pub fn disconnect_stream(&mut self) {
        *self.shared.stream_tx.lock().unwrap() = None;
        if let Some(mut child) = self.stream.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.shared.stream_live.store(false, Ordering::SeqCst);
    }

    pub fn refresh_record(&mut self) {
        if let Some(record) = self.record.as_mut() {
            if record.try_wait().map(|s| s.is_some()).unwrap_or(true) {
                self.shared.record_live.store(false, Ordering::SeqCst);
            }
        }
    }

    pub fn refresh_stream(&mut self) {
        if let Some(child) = self.stream.as_mut() {
            let exited = child.try_wait().map(|s| s.is_some()).unwrap_or(true);
            if exited || !self.shared.stream_live.load(Ordering::SeqCst) {
                if !exited { let _ = child.kill(); let _ = child.wait(); }
                self.shared.stream_live.store(false, Ordering::SeqCst);
                *self.shared.stream_tx.lock().unwrap() = None;
                self.stream = None;
            }
        }
    }

    pub fn retry_stream(&mut self, ffmpeg_path: &std::path::Path, source: &BroadcastConfig) -> Result<(), String> {
        if !self.session_active() { return Err("Belum ada sesi siaran".into()); }
        self.refresh_stream();
        if self.shared.stream_live.load(Ordering::SeqCst) { return Err("Siaran sudah terhubung".into()); }
        let (child, tx) = spawn_stream(ffmpeg_path, source, self.shared.stream_live.clone())?;
        *self.shared.stream_tx.lock().unwrap() = Some(tx);
        self.stream = Some(child);
        Ok(())
    }
}

impl RunningMix {
    pub fn finish_session(&mut self) -> Result<Option<std::path::PathBuf>, String> {
        if !self.session_active() { return Err("Belum ada sesi siaran".into()); }
        self.session_started_at = None;
        self.disconnect_stream();
        *self.shared.record_tx.lock().unwrap() = None;
        if let Some(writer) = self.record_writer.take() { let _ = writer.join(); }
        self.shared.record_live.store(false, Ordering::SeqCst);
        self.record_enabled = false;
        if let Some(mut record) = self.record.take() {
            let status = record.wait().map_err(|e| e.to_string())?;
            let temp = self.recording_temp.take().ok_or("File sementara rekaman tidak ditemukan")?;
            let final_path = self.recording_final.take().ok_or("Nama rekaman tidak ditemukan")?;
            if !status.success() { return Err(format!("Encoder rekaman gagal. File sementara: {}", temp.display())); }
            std::fs::rename(&temp, &final_path).map_err(|e| e.to_string())?;
            return Ok(Some(final_path));
        }
        Ok(None)
    }

    pub fn stop_preview(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.guard.stop();
        if let Some(mut loopback) = self.loopback.take() { loopback.stop(); }
        if let Some(writer) = self.mix_writer.take() { let _ = writer.join(); }
    }
}

pub fn stop(mix: &Mutex<Option<RunningMix>>) -> Result<Option<std::path::PathBuf>, String> {
    let mut slot = mix.lock().unwrap();
    if let Some(mut rm) = slot.take() {
        let saved = if rm.session_active() { rm.finish_session() } else { Ok(None) };
        rm.stop_preview();
        return saved;
    }
    Ok(None)
}

fn pcm_command(ffmpeg_path: &std::path::Path, bitrate: u32) -> Command {
    let mut command = Command::new(ffmpeg_path);
    command.args(["-hide_banner", "-loglevel", "error", "-f", "s16le", "-ar", "44100", "-ac", "2",
        "-i", "pipe:0", "-acodec", "libmp3lame", "-b:a", &format!("{bitrate}k"), "-f", "mp3"]);
    command.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    command
}

fn spawn_stream(ffmpeg_path: &std::path::Path, source: &BroadcastConfig, live: Arc<AtomicBool>) -> Result<(Child, SyncSender<Vec<u8>>), String> {
    let mut url = reqwest::Url::parse(&format!("icecast://{}:{}/{}", source.host, source.port, source.mount.trim_start_matches('/')))
        .map_err(|_| "Alamat Icecast tidak valid".to_string())?;
    url.set_username(&source.username).map_err(|_| "Username Icecast tidak valid".to_string())?;
    url.set_password(Some(&source.password)).map_err(|_| "Password Icecast tidak valid".to_string())?;
    let mut command = pcm_command(ffmpeg_path, source.bitrate_kbps);
    command.args(["-content_type", "audio/mpeg", url.as_str()]);
    let mut child = command.spawn().map_err(|e| format!("FFmpeg siaran gagal dijalankan: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("Pipe FFmpeg siaran gagal")?;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(8);
    live.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        for bytes in rx {
            if stdin.write_all(&bytes).is_err() { break; }
        }
        live.store(false, Ordering::SeqCst);
    });
    Ok((child, tx))
}

pub fn start_preview(
    cfg: &Config,
    gains: (f32, f32, f32),
    routes: (u8, u8),
) -> Result<RunningMix, String> {
    if cfg.device.is_empty() { return Err("Input 1 belum dipilih".into()); }
    if cfg.device2.as_deref() == Some(cfg.device.as_str()) { return Err("Input 1 dan 2 tidak boleh sama".into()); }
    let desktop = cfg.device == crate::loopback::DEFAULT_OUTPUT || cfg.device2.as_deref() == Some(crate::loopback::DEFAULT_OUTPUT);
    let mut monitor_block = None;
    if desktop && !cfg.monitor_device.is_empty() {
        let default_output = cpal::default_host().default_output_device().and_then(|d| d.name().ok());
        if default_output.as_deref() == Some(cfg.monitor_device.as_str()) {
            monitor_block = Some("Monitor Bus B memakai output yang sama dengan Desktop audio; pilih output lain".into());
        }
    }

    let has2 = !cfg.device2.as_deref().unwrap_or("").is_empty();
    let shared = MixerShared::new(has2);
    {
        let mut p = shared.params.lock().unwrap();
        p.gain1 = gains.0; p.gain2 = gains.1; p.monitor_gain = gains.2;
        p.route1 = routes.0; p.route2 = routes.1;
    }
    let name1 = cfg.device.clone();
    let name2 = cfg.device2.clone().unwrap_or_default();
    let mon_name = cfg.monitor_device.clone();
    let r1 = shared.ring1.clone();
    let r2 = shared.ring2.clone();
    let guard = StreamGuard::start(move || {
        let mut streams: Vec<Stream> = Vec::new();
        if name1 != crate::loopback::DEFAULT_OUTPUT {
            let dev1 = find_input_device(&name1)?;
            let s1 = make_input_stream(&dev1, &r1).map_err(|e| describe_device_error("Input", &name1, e))?;
            s1.play().map_err(|e| describe_device_error("Input", &name1, e.to_string()))?;
            streams.push(s1);
        }
        if !name2.is_empty() {
            if name2 != crate::loopback::DEFAULT_OUTPUT {
                let dev2 = find_input_device(&name2)?;
                let s2 = make_input_stream(&dev2, &r2).map_err(|e| describe_device_error("Input", &name2, e))?;
                s2.play().map_err(|e| describe_device_error("Input", &name2, e.to_string()))?;
                streams.push(s2);
            }
        }
        Ok(streams)
    }, mon_name, shared.monitor.clone(), shared.clone(), monitor_block)?;
    let loopback = if desktop {
        let ring = if cfg.device == crate::loopback::DEFAULT_OUTPUT { shared.ring1.clone() } else { shared.ring2.clone() };
        Some(crate::loopback::LoopbackGuard::start(ring)?)
    } else { None };

    let mix_writer = spawn_writer(&shared);
    Ok(RunningMix {
        shared, record_enabled: false, session_started_at: None,
        device: cfg.device.clone(), device2: cfg.device2.clone(), monitor_device: cfg.monitor_device.clone(),
        record: None, stream: None, record_writer: None, mix_writer: Some(mix_writer),
        recording_temp: None, recording_final: None, guard, loopback,
    })
}

impl RunningMix {
    pub fn start_session(
        &mut self,
        cfg: &Config,
        ffmpeg_path: &std::path::Path,
        source: &BroadcastConfig,
        recording_dir: &std::path::Path,
    ) -> Result<(), String> {
        if self.session_active() { return Err("Sesi sudah berjalan".into()); }
        if cfg.record_enabled {
            std::fs::create_dir_all(recording_dir).map_err(|e| format!("Folder rekaman: {e}"))?;
            let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S_%3f");
            let final_path = recording_dir.join(format!("8EH-Studio_{stamp}.mp3"));
            let temp = recording_dir.join(format!("8EH-Studio_{stamp}.part.mp3"));
            let mut command = pcm_command(ffmpeg_path, source.bitrate_kbps);
            command.arg(&temp);
            let mut record = command.spawn().map_err(|e| format!("FFmpeg rekaman gagal dijalankan: {e}"))?;
            let mut record_stdin = record.stdin.take().ok_or("Pipe FFmpeg rekaman gagal")?;
            let (record_tx, record_rx) = mpsc::sync_channel::<Vec<u8>>(512);
            let record_live = self.shared.record_live.clone();
            let writer = std::thread::spawn(move || {
                for bytes in record_rx {
                    if record_stdin.write_all(&bytes).is_err() { record_live.store(false, Ordering::SeqCst); break; }
                }
            });
            self.record = Some(record);
            self.record_writer = Some(writer);
            self.recording_temp = Some(temp);
            self.recording_final = Some(final_path);
            self.shared.record_live.store(true, Ordering::SeqCst);
            *self.shared.record_tx.lock().unwrap() = Some(record_tx);
        }
        self.record_enabled = cfg.record_enabled;
        let (stream, tx) = match spawn_stream(ffmpeg_path, source, self.shared.stream_live.clone()) {
            Ok((stream, tx)) => (Some(stream), Some(tx)),
            Err(_) => (None, None),
        };
        self.stream = stream;
        *self.shared.stream_tx.lock().unwrap() = tx;
        self.session_started_at = Some(std::time::Instant::now());
        Ok(())
    }
}

/// Thread mixing: ring1+ring2 → (gain, route) → busA → ffmpeg stdin (s16)
///                                            → busB → ring monitor
pub fn spawn_writer(shared: &Arc<MixerShared>) -> std::thread::JoinHandle<()> {
    let shared = shared.clone();
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        let mut last_tick = std::time::Instant::now();
        let mut frame_fraction = 0.0f64;
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(8));
            let now = std::time::Instant::now();
            let due = last_tick.elapsed().as_secs_f64().min(0.05) * RATE as f64 + frame_fraction;
            last_tick = now;
            let n = due.floor() as usize;
            frame_fraction = due.fract();
            if n == 0 { continue; }

            let p = *shared.params.lock().unwrap();
            let seg1 = take_or_silence(&shared.ring1, n);
            let seg2 = if shared.has2 { take_or_silence(&shared.ring2, n) } else { vec![0.0; n * 2] };

            buf.clear();
            let mut sum_sq = [0f64; 4];
            let mut clipped = [false; 2];
            let mut mon = Vec::with_capacity(n * 2);
            for i in 0..n {
                let l1 = seg1[i * 2] * p.gain1;
                let r1 = seg1[i * 2 + 1] * p.gain1;
                let l2 = seg2[i * 2] * p.gain2;
                let r2 = seg2[i * 2 + 1] * p.gain2;

                // meter input post-fader
                sum_sq[0] += (l1 * l1 + r1 * r1) as f64;
                if shared.has2 {
                    sum_sq[1] += (l2 * l2 + r2 * r2) as f64;
                }

                let la = if p.route1 & 1 != 0 { l1 } else { 0.0 }
                    + if shared.has2 && p.route2 & 1 != 0 { l2 } else { 0.0 };
                let ra = if p.route1 & 1 != 0 { r1 } else { 0.0 }
                    + if shared.has2 && p.route2 & 1 != 0 { r2 } else { 0.0 };

                let lb = if p.route1 & 2 != 0 { l1 } else { 0.0 }
                    + if shared.has2 && p.route2 & 2 != 0 { l2 } else { 0.0 };
                let rb = if p.route1 & 2 != 0 { r1 } else { 0.0 }
                    + if shared.has2 && p.route2 & 2 != 0 { r2 } else { 0.0 };

                sum_sq[2] += (la * la + ra * ra) as f64;
                sum_sq[3] += (lb * lb + rb * rb) as f64;
                clipped[0] |= la.abs() >= 1.0 || ra.abs() >= 1.0;
                clipped[1] |= (lb * p.monitor_gain).abs() >= 1.0 || (rb * p.monitor_gain).abs() >= 1.0;

                let clamp = |v: f32| v.clamp(-1.0, 1.0);
                // bus A → s16 LE ke ffmpeg
                let sl = (clamp(la) * 32767.0) as i16;
                let sr = (clamp(ra) * 32767.0) as i16;
                buf.extend_from_slice(&sl.to_le_bytes());
                buf.extend_from_slice(&sr.to_le_bytes());
                // bus B → f32 monitor
                mon.push(clamp(lb * p.monitor_gain));
                mon.push(clamp(rb * p.monitor_gain));
            }

            // RMS update (linear amplitudo rata2 stereo)
            {
                let mut m = shared.meters.lock().unwrap();
                for (idx, ss) in sum_sq.iter().enumerate() {
                    let rms = (*ss / (n as f64 * 2.0)).sqrt() as f32;
                    m[idx] = rms;
                }
            }
            if clipped[0] || clipped[1] {
                let mut last = shared.last_clip.lock().unwrap();
                for i in 0..2 { if clipped[i] { last[i] = Some(std::time::Instant::now()); } }
            }

            push_frames(&shared.monitor, &mon.chunks(2).map(|c| (c[0], c[1])).collect::<Vec<_>>());
            let record = shared.record_tx.lock().unwrap().as_ref().cloned();
            if let Some(tx) = record {
                match tx.try_send(buf.clone()) {
                    Ok(()) => {}
                    Err(TrySendError::Full(_)) => { shared.record_live.store(false, Ordering::SeqCst); }
                    Err(TrySendError::Disconnected(_)) => {
                        *shared.record_tx.lock().unwrap() = None;
                        shared.record_live.store(false, Ordering::SeqCst);
                    }
                }
            }
            let mut stream = shared.stream_tx.lock().unwrap();
            if let Some(tx) = stream.as_ref() {
                match tx.try_send(buf.clone()) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => {
                        *stream = None;
                        shared.stream_live.store(false, Ordering::SeqCst);
                    }
                }
            }
        }
    })
}

// ---------- stream builder per sample format ----------

fn make_input_stream(
    device: &cpal::Device,
    ring: &Arc<Mutex<VecDeque<f32>>>,
) -> Result<Stream, String> {
    let cfg = device.default_input_config().map_err(|e| e.to_string())?;
    let rate = cfg.sample_rate().0 as f32;
    let ch = cfg.channels().max(1) as usize;
    let scfg: cpal::StreamConfig = cfg.clone().into();
    let ring = ring.clone();

    match cfg.sample_format() {
        SampleFormat::F32 => {
            let cb_ring = ring.clone();
            let mut resampler = InputResampler::new();
            device
                .build_input_stream(
                    &scfg,
                    move |data: &[f32], _| {
                        let frames = to_frames(data.iter().copied(), ch, rate, &mut resampler);
                        push_frames(&cb_ring, &frames);
                    },
                    |err| eprintln!("input err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::I16 => {
            let cb_ring = ring.clone();
            let mut resampler = InputResampler::new();
            device
                .build_input_stream(
                    &scfg,
                    move |data: &[i16], _| {
                        let frames = to_frames(data.iter().map(|s| f32::from_sample(*s)), ch, rate, &mut resampler);
                        push_frames(&cb_ring, &frames);
                    },
                    |err| eprintln!("input err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::U16 => {
            let cb_ring = ring.clone();
            let mut resampler = InputResampler::new();
            device
                .build_input_stream(
                    &scfg,
                    move |data: &[u16], _| {
                        let frames = to_frames(
                            data.iter()
                                .map(|s| f32::from_sample(*s)),
                            ch,
                            rate,
                            &mut resampler,
                        );
                        push_frames(&cb_ring, &frames);
                    },
                    |err| eprintln!("input err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        other => Err(format!("Format sample input tidak didukung: {other:?}")),
    }
}

fn make_output_stream(
    device: &cpal::Device,
    ring: &Arc<Mutex<VecDeque<f32>>>,
) -> Result<Stream, String> {
    let cfg = device.default_output_config().map_err(|e| e.to_string())?;
    let rate = cfg.sample_rate().0 as f32;
    let ch = cfg.channels().max(1) as usize;
    let scfg: cpal::StreamConfig = cfg.clone().into();
    let ring = ring.clone();

    match cfg.sample_format() {
        SampleFormat::F32 => {
            let cb_ring = ring.clone();
            let mut resampler = OutputResampler::new(rate);
            device
                .build_output_stream(
                    &scfg,
                    move |data: &mut [f32], _| fill_output(data, &cb_ring, ch, &mut resampler),
                    |err| eprintln!("output err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::I16 => {
            let cb_ring = ring.clone();
            let mut resampler = OutputResampler::new(rate);
            device
                .build_output_stream(
                    &scfg,
                    move |data: &mut [i16], _| {
                        let nf = data.len();
                        let mut tmp = vec![0f32; nf];
                        fill_output(&mut tmp, &cb_ring, ch, &mut resampler);
                        for (o, s) in data.iter_mut().zip(tmp.iter()) {
                            *o = i16::from_sample(*s);
                        }
                    },
                    |err| eprintln!("output err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::U16 => {
            let cb_ring = ring.clone();
            let mut resampler = OutputResampler::new(rate);
            device
                .build_output_stream(
                    &scfg,
                    move |data: &mut [u16], _| {
                        let nf = data.len();
                        let mut tmp = vec![0f32; nf];
                        fill_output(&mut tmp, &cb_ring, ch, &mut resampler);
                        for (o, s) in data.iter_mut().zip(tmp.iter()) {
                            *o = u16::from_sample(*s);
                        }
                    },
                    |err| eprintln!("output err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        other => Err(format!("Format sample output tidak didukung: {other:?}")),
    }
}

struct OutputResampler {
    step: f64,
    phase: f64,
    current: (f32, f32),
    next: (f32, f32),
    ready: bool,
}

impl OutputResampler {
    fn new(output_rate: f32) -> Self {
        Self { step: RATE as f64 / output_rate as f64, phase: 0.0, current: (0.0, 0.0), next: (0.0, 0.0), ready: false }
    }

    fn pop(q: &mut VecDeque<f32>) -> Option<(f32, f32)> {
        if q.len() < 2 { return None; }
        Some((q.pop_front()?, q.pop_front()?))
    }

    fn frame(&mut self, q: &mut VecDeque<f32>) -> (f32, f32) {
        if !self.ready {
            if q.len() < (RATE as usize / 50) * 2 { return (0.0, 0.0); }
            self.current = Self::pop(q).unwrap_or((0.0, 0.0));
            self.next = Self::pop(q).unwrap_or(self.current);
            self.phase = 0.0;
            self.ready = true;
        }
        let t = self.phase as f32;
        let value = (
            self.current.0 + (self.next.0 - self.current.0) * t,
            self.current.1 + (self.next.1 - self.current.1) * t,
        );
        self.phase += self.step;
        while self.phase >= 1.0 {
            self.phase -= 1.0;
            self.current = self.next;
            if let Some(next) = Self::pop(q) { self.next = next; }
            else { self.ready = false; break; }
        }
        value
    }
}

/// Ambil 44,1 kHz dari ring monitor dengan fase kontinu, lalu sebar ke channel output.
fn fill_output(data: &mut [f32], ring: &Arc<Mutex<VecDeque<f32>>>, ch: usize, resampler: &mut OutputResampler) {
    let mut q = ring.lock().unwrap();
    if q.len() > (RATE as usize / 7) * 2 {
        let excess = ((q.len() - (RATE as usize / 20) * 2) / 2) * 2;
        for _ in 0..excess { q.pop_front(); }
        resampler.ready = false;
    }
    for frame in data.chunks_mut(ch.max(1)) {
        let (l, r) = resampler.frame(&mut q);
        for (side, out) in frame.iter_mut().enumerate() {
            *out = match (ch, side) {
                (1, _) => (l + r) * 0.5,
                (_, 0) => l,
                (_, 1) => r,
                _ => (l + r) * 0.5,
            };
        }
    }
}

/// interleaved mentah (ch berapapun) → frames stereo 44.1k
pub(crate) fn to_frames<I: Iterator<Item = f32>>(
    samples: I,
    ch: usize,
    rate: f32,
    resampler: &mut InputResampler,
) -> Vec<(f32, f32)> {
    let raw: Vec<f32> = samples.collect();
    let mut stereo: Vec<(f32, f32)> = Vec::with_capacity(raw.len() / ch.max(1));
    let ch = ch.max(1);
    for frame in raw.chunks(ch) {
        let l = frame.first().copied().unwrap_or(0.0);
        let r = frame.get(1).copied().unwrap_or(l);
        stereo.push((l, r));
    }
    resampler.process(&stereo, rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crossings(samples: &[f32]) -> usize {
        samples.windows(2).filter(|pair| pair[0] <= 0.0 && pair[1] > 0.0).count()
    }

    #[test]
    fn input_resampling_keeps_pitch_across_irregular_callbacks() {
        let mut resampler = InputResampler::new();
        let source: Vec<(f32, f32)> = (0..48_000).map(|i| {
            let value = (std::f32::consts::TAU * 440.0 * i as f32 / 48_000.0).sin();
            (value, value)
        }).collect();
        let result: Vec<f32> = source.chunks(97)
            .flat_map(|chunk| resampler.process(chunk, 48_000.0))
            .map(|frame| frame.0).collect();
        assert!((result.len() as isize - 44_100).abs() <= 2);
        assert!((crossings(&result) as isize - 440).abs() <= 2);
    }

    #[test]
    fn monitor_resampling_keeps_pitch_at_48khz() {
        let mut ring = VecDeque::new();
        for i in 0..44_100 {
            let value = (std::f32::consts::TAU * 440.0 * i as f32 / 44_100.0).sin();
            ring.push_back(value);
            ring.push_back(value);
        }
        let mut resampler = OutputResampler::new(48_000.0);
        let output: Vec<f32> = (0..48_000).map(|_| resampler.frame(&mut ring).0).collect();
        assert!((crossings(&output) as isize - 440).abs() <= 2);
    }

    #[test]
    fn empty_second_input_does_not_hold_first_input() {
        let first = Mutex::new(VecDeque::from(vec![0.5; 882]));
        let second = Mutex::new(VecDeque::new());
        assert_eq!(take_or_silence(&first, 441)[0], 0.5);
        assert!(take_or_silence(&second, 441).iter().all(|sample| *sample == 0.0));
    }
}
