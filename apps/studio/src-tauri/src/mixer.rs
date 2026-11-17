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

use crate::Config;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream};
use std::collections::VecDeque;
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
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
    pub has2: bool,
    pub stop: AtomicBool,
    pub started_at: std::time::Instant,
}

impl MixerShared {
    fn new(has2: bool) -> Arc<Self> {
        Arc::new(Self {
            ring1: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            ring2: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            monitor: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            params: Mutex::new(MixerParams::default()),
            meters: Mutex::new([0.0; 4]),
            has2,
            stop: AtomicBool::new(false),
            started_at: std::time::Instant::now(),
        })
    }
}

// ---------- util ----------

fn push_frames(ring: &Mutex<VecDeque<f32>>, frames: &[(f32, f32)]) {
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

/// Resample naive linear (baik cukup untuk suara; perangkat umumnya 44.1/48kHz)
fn resample_stereo(input: &[(f32, f32)], from_rate: f32) -> Vec<(f32, f32)> {
    if (from_rate - RATE).abs() < 1.0 || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate / RATE;
    let out_len = ((input.len() as f32) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f32 * ratio;
        let i0 = pos.floor() as usize;
        let i1 = (i0 + 1).min(input.len() - 1);
        let t = pos - i0 as f32;
        let l = input[i0].0 + (input[i1].0 - input[i0].0) * t;
        let r = input[i0].1 + (input[i1].1 - input[i0].1) * t;
        out.push((l, r));
    }
    out
}

fn db_to_linear(db: f32) -> f32 {
    (10.0f32).powf(db / 20.0)
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
    pub fn start(build: impl FnOnce() -> Result<Vec<Stream>, String> + Send + 'static) -> Result<Self, String> {
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
            let _keep = streams; // stream hidup selama thread ini hidup
            let _ = stop_rx.recv(); // tunggu sinyal stop
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
    pub ffmpeg: Child,
    pub shared: Arc<MixerShared>,
    guard: StreamGuard,
}

impl RunningMix {
    pub fn elapsed_secs(&self) -> u64 {
        self.shared.started_at.elapsed().as_secs()
    }
}

pub fn is_alive(mix: &Mutex<Option<RunningMix>>) -> bool {
    let mut slot = mix.lock().unwrap();
    match slot.as_mut() {
        Some(rm) => {
            if rm.ffmpeg.try_wait().map(|r| r.is_none()).unwrap_or(false) {
                true
            } else {
                *slot = None;
                false
            }
        }
        None => false,
    }
}

pub fn stop(mix: &Mutex<Option<RunningMix>>) {
    let mut slot = mix.lock().unwrap();
    if let Some(mut rm) = slot.take() {
        rm.shared.stop.store(true, Ordering::SeqCst);
        let _ = rm.ffmpeg.kill();
        let _ = rm.ffmpeg.wait();
        rm.guard.stop(); // stream capture/playback berhenti
    }
}

/// Start sesi penuh: input streams + FFmpeg encoder (PCM stdin → MP3 → Icecast)
/// + monitor playback + writer thread mixing.
pub fn start(
    cfg: &Config,
    gains: (f32, f32, f32), // gain1, gain2, monitor (linear)
    routes: (u8, u8),
) -> Result<RunningMix, String> {
    if cfg.device.is_empty() {
        return Err("Input 1 belum dipilih".into());
    }
    if cfg.ice_password.is_empty() {
        return Err("Password Icecast belum diisi (tab Pengaturan)".into());
    }

    let has2 = !cfg.device2.as_deref().unwrap_or("").is_empty();
    let shared = MixerShared::new(has2);
    {
        let mut p = shared.params.lock().unwrap();
        p.gain1 = gains.0;
        p.gain2 = gains.1;
        p.monitor_gain = gains.2;
        p.route1 = routes.0;
        p.route2 = routes.1;
    }

    // ---- streams dibangun di thread guard (affinity + non-Send) ----
    let name1 = cfg.device.clone();
    let name2 = cfg.device2.clone().unwrap_or_default();
    let mon_name = cfg.monitor_device.clone();
    let r1 = shared.ring1.clone();
    let r2 = shared.ring2.clone();
    let rmon = shared.monitor.clone();
    let guard = StreamGuard::start(move || {
        let mut streams: Vec<Stream> = Vec::new();
        let dev1 = find_input_device(&name1)?;
        let s1 = make_input_stream(&dev1, &r1)?;
        s1.play().map_err(|e| e.to_string())?;
        streams.push(s1);
        if !name2.is_empty() {
            if name2 == name1 {
                return Err("Input 1 dan Input 2 tidak boleh sama".into());
            }
            let dev2 = find_input_device(&name2)?;
            let s2 = make_input_stream(&dev2, &r2)?;
            s2.play().map_err(|e| e.to_string())?;
            streams.push(s2);
        }
        if !mon_name.is_empty() {
            let odev = find_output_device(&mon_name)?;
            let s = make_output_stream(&odev, &rmon)?;
            s.play().map_err(|e| e.to_string())?;
            streams.push(s);
        }
        Ok(streams)
    })?;

    // ---- FFmpeg: PCM stdin -> MP3 -> Icecast ----
    let url = format!(
        "icecast://{}:{}@{}:{}/{}",
        cfg.ice_username,
        cfg.ice_password,
        cfg.ice_host,
        cfg.ice_port,
        cfg.ice_mount.trim_start_matches('/')
    );
    let mut ffmpeg = Command::new(&cfg.ffmpeg_path)
        .args([
            "-hide_banner",
            "-loglevel",
            "warning",
            "-f", "s16le",
            "-ar", "44100",
            "-ac", "2",
            "-i", "pipe:0",
            "-af", "aresample=44100",
            "-acodec", "libmp3lame",
            "-b:a", &format!("{}k", cfg.bitrate),
            "-ar", "44100",
            "-ac", "2",
            "-content_type", "audio/mpeg",
            "-f", "mp3",
            &url,
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn()
        .map_err(|e| format!("Gagal menjalankan FFmpeg: {e}"))?;

    let stdin = ffmpeg
        .stdin
        .take()
        .ok_or("tidak bisa pegang stdin ffmpeg")?;

    // writer thread: ring -> mix -> ffmpeg stdin + ring monitor
    spawn_writer(&shared, stdin);

    Ok(RunningMix {
        ffmpeg,
        shared,
        guard,
    })
}

/// Thread mixing: ring1+ring2 → (gain, route) → busA → ffmpeg stdin (s16)
///                                            → busB → ring monitor
pub fn spawn_writer(shared: &Arc<MixerShared>, mut stdin: ChildStdin) {
    let shared = shared.clone();
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::with_capacity(8192);
        loop {
            if shared.stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(16));

            let p = *shared.params.lock().unwrap();
            let (mut n1, mut n2) = (0usize, 0usize);
            let mut seg1: Vec<f32> = Vec::new();
            let mut seg2: Vec<f32> = Vec::new();
            {
                let q1 = shared.ring1.lock().unwrap();
                n1 = q1.len() / 2;
                if n1 > 0 {
                    let it = q1.iter().take(n1 * 2);
                    seg1 = it.cloned().collect();
                }
            }
            if shared.has2 {
                let q2 = shared.ring2.lock().unwrap();
                n2 = q2.len() / 2;
                if n2 > 0 {
                    let it = q2.iter().take(n2 * 2);
                    seg2 = it.cloned().collect();
                }
            }
            let n = if shared.has2 { n1.min(n2) } else { n1 };
            if n == 0 {
                continue;
            }
            // buang sisa frame yang tidak ter-campur (align kedua ring)
            {
                let mut q1 = shared.ring1.lock().unwrap();
                for _ in 0..n * 2 {
                    q1.pop_front();
                }
            }
            if shared.has2 {
                let mut q2 = shared.ring2.lock().unwrap();
                for _ in 0..n * 2 {
                    q2.pop_front();
                }
            }

            buf.clear();
            let mut sum_sq = [0f64; 4];
            let mut mon = Vec::with_capacity(n * 2);
            for i in 0..n {
                let l1 = if n1 > i { seg1[i * 2] * p.gain1 } else { 0.0 };
                let r1 = if n1 > i { seg1[i * 2 + 1] * p.gain1 } else { 0.0 };
                let l2 = if shared.has2 && n2 > i { seg2[i * 2] * p.gain2 } else { 0.0 };
                let r2 = if shared.has2 && n2 > i { seg2[i * 2 + 1] * p.gain2 } else { 0.0 };

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

            push_frames(&shared.monitor, &mon.chunks(2).map(|c| (c[0], c[1])).collect::<Vec<_>>());
            if stdin.write_all(&buf).is_err() {
                eprintln!("encoder terputus");
                shared.stop.store(true, Ordering::SeqCst);
                break;
            }
            let _ = stdin.flush();
        }
    });
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
            device
                .build_input_stream(
                    &scfg,
                    move |data: &[f32], _| {
                        let frames = to_frames(data.iter().copied(), ch, rate);
                        push_frames(&cb_ring, &frames);
                    },
                    |err| eprintln!("input err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::I16 => {
            let cb_ring = ring.clone();
            device
                .build_input_stream(
                    &scfg,
                    move |data: &[i16], _| {
                        let frames = to_frames(data.iter().map(|s| f32::from_sample(*s)), ch, rate);
                        push_frames(&cb_ring, &frames);
                    },
                    |err| eprintln!("input err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::U16 => {
            let cb_ring = ring.clone();
            device
                .build_input_stream(
                    &scfg,
                    move |data: &[u16], _| {
                        let frames = to_frames(
                            data.iter()
                                .map(|s| f32::from_sample(*s)),
                            ch,
                            rate,
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
            device
                .build_output_stream(
                    &scfg,
                    move |data: &mut [f32], _| fill_output(data, &cb_ring, ch, rate),
                    |err| eprintln!("output err: {err}"),
                    None,
                )
                .map_err(|e| e.to_string())
        }
        SampleFormat::I16 => {
            let cb_ring = ring.clone();
            device
                .build_output_stream(
                    &scfg,
                    move |data: &mut [i16], _| {
                        let nf = data.len();
                        let mut tmp = vec![0f32; nf];
                        fill_output(&mut tmp, &cb_ring, ch, rate);
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
            device
                .build_output_stream(
                    &scfg,
                    move |data: &mut [u16], _| {
                        let nf = data.len();
                        let mut tmp = vec![0f32; nf];
                        fill_output(&mut tmp, &cb_ring, ch, rate);
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

/// Ambil sampel dari ring monitor, resample ke rate output, sebar ke channel.
fn fill_output(data: &mut [f32], ring: &Arc<Mutex<VecDeque<f32>>>, ch: usize, rate: f32) {
    let need = if (rate - RATE).abs() < 1.0 {
        data.len() / ch.max(1)
    } else {
        ((data.len() as f32 / ch as f32) * (rate / RATE)).ceil() as usize
    };
    let mut got: Vec<f32> = Vec::with_capacity(need * 2);
    {
        let mut q = ring.lock().unwrap();
        let mut take = need * 2;
        if take > q.len() {
            take = q.len();
        }
        while take > 0 {
            got.push(q.pop_front().unwrap_or(0.0));
            take -= 1;
        }
    }
    // stereo interleaved → sebar ke semua channel output
    let frames = got.len() / 2;
    for (i, out) in data.iter_mut().enumerate() {
        let f = i / ch;
        let side = i % ch;
        let (l, r) = if f < frames {
            (got[f * 2], got[f * 2 + 1])
        } else {
            (0.0, 0.0)
        };
        *out = match (ch, side) {
            (1, _) => (l + r) * 0.5,
            (_, 0) => l,
            (_, 1) => r,
            _ => (l + r) * 0.5,
        };
    }
}

/// interleaved mentah (ch berapapun) → frames stereo 44.1k
fn to_frames<I: Iterator<Item = f32>>(
    samples: I,
    ch: usize,
    rate: f32,
) -> Vec<(f32, f32)> {
    let raw: Vec<f32> = samples.collect();
    let mut stereo: Vec<(f32, f32)> = Vec::with_capacity(raw.len() / ch.max(1));
    let ch = ch.max(1);
    for frame in raw.chunks(ch) {
        let l = frame.first().copied().unwrap_or(0.0);
        let r = frame.get(1).copied().unwrap_or(l);
        stereo.push((l, r));
    }
    resample_stereo(&stereo, rate)
}
