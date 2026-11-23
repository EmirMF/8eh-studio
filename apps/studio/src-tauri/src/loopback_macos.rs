use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const DEFAULT_OUTPUT: &str = "desktop:default";

pub struct LoopbackGuard {
    child: Child,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackGuard {
    pub fn start(ring: Arc<Mutex<VecDeque<f32>>>) -> Result<Self, String> {
        let bundled = std::env::current_exe().map_err(|e| e.to_string())?
            .parent().ok_or("Lokasi aplikasi tidak ditemukan")?
            .join("../Resources/desktop-capture");
        let helper = if bundled.is_file() { bundled } else {
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries/macos/desktop-capture")
        };
        let mut child = Command::new(&helper).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().map_err(|e| format!("Desktop audio helper gagal dijalankan: {e}"))?;
        let stderr = child.stderr.take().ok_or("Status desktop audio tidak tersedia")?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut lines = BufReader::new(stderr).lines();
            let first = lines.next().and_then(Result::ok).unwrap_or_else(|| "Helper berhenti".into());
            let _ = tx.send(first);
            for line in lines.flatten() { eprintln!("desktop audio: {line}"); }
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(message) if message == "READY" => {}
            Ok(message) => { let _ = child.kill(); let _ = child.wait(); return Err(message); }
            Err(_) => { let _ = child.kill(); let _ = child.wait(); return Err("Izin Perekaman Layar belum diberikan. Buka System Settings > Privacy & Security > Screen Recording, izinkan 8EH Studio, lalu buka ulang aplikasi.".into()); }
        }
        let mut stdout = child.stdout.take().ok_or("Audio desktop tidak tersedia")?;
        let thread = std::thread::spawn(move || {
            let mut bytes = [0u8; 8192];
            let mut leftover = Vec::new();
            let mut resampler = super::mixer::InputResampler::new();
            loop {
                let n = match stdout.read(&mut bytes) { Ok(0) | Err(_) => break, Ok(n) => n };
                leftover.extend_from_slice(&bytes[..n]);
                let complete = (leftover.len() / 8) * 8;
                let samples = leftover[..complete].chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
                    .collect::<Vec<_>>();
                leftover.drain(..complete);
                let frames = super::mixer::to_frames(samples.into_iter(), 2, 44_100.0, &mut resampler);
                super::mixer::push_frames(&ring, &frames);
            }
        });
        Ok(Self { child, thread: Some(thread) })
    }

    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

impl Drop for LoopbackGuard { fn drop(&mut self) { self.stop(); } }
