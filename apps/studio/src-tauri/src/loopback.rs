use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

pub const DEFAULT_OUTPUT: &str = "desktop:default";

pub struct LoopbackGuard {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopbackGuard {
    pub fn start(ring: Arc<Mutex<VecDeque<f32>>>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let result = unsafe { capture_loop(ring, &worker_stop, ready_tx) };
            if let Err(error) = result { eprintln!("Desktop audio: {error}"); }
        });
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, thread: Some(thread) }),
            Ok(Err(error)) => { let _ = thread.join(); Err(error) }
            Err(_) => { stop.store(true, Ordering::SeqCst); Err("Desktop audio tidak merespons".into()) }
        }
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

impl Drop for LoopbackGuard {
    fn drop(&mut self) { self.stop(); }
}

unsafe fn capture_loop(
    ring: Arc<Mutex<VecDeque<f32>>>,
    stop: &AtomicBool,
    ready_tx: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    if let Err(error) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
        let message = error.to_string();
        let _ = ready_tx.send(Err(message.clone()));
        return Err(message);
    }
    let result = capture_initialized(ring, stop, ready_tx);
    CoUninitialize();
    result
}

unsafe fn capture_initialized(
    ring: Arc<Mutex<VecDeque<f32>>>,
    stop: &AtomicBool,
    ready_tx: mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let setup = || -> Result<(IAudioClient, IAudioCaptureClient, *mut WAVEFORMATEX, usize, usize, bool, u32), String> {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(|e| e.to_string())?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let format = client.GetMixFormat().map_err(|e| e.to_string())?;
        let wave = std::ptr::read_unaligned(format);
        let channels = usize::from(wave.nChannels);
        let bits = usize::from(wave.wBitsPerSample);
        let subtype = if wave.wFormatTag == 0xfffe {
            let extended = std::ptr::read_unaligned(format.cast::<WAVEFORMATEXTENSIBLE>());
            let guid = extended.SubFormat;
            guid.to_u128()
        } else { u128::from(wave.wFormatTag) };
        let float = subtype == 3 || subtype == 0x00000003_0000_0010_8000_00aa00389b71;
        let pcm = subtype == 1 || subtype == 0x00000001_0000_0010_8000_00aa00389b71;
        if channels == 0 || !((float && bits == 32) || (pcm && matches!(bits, 16 | 24 | 32))) {
            CoTaskMemFree(Some(format.cast()));
            return Err("Format output Windows tidak didukung untuk loopback".into());
        }
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 1_000_000, 0, format, None)
            .map_err(|e| e.to_string())?;
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.to_string())?;
        client.Start().map_err(|e| e.to_string())?;
        Ok((client, capture, format, channels, bits, float, wave.nSamplesPerSec))
    };
    let (client, capture, format, channels, bits, float, rate) = match setup() {
        Ok(value) => { let _ = ready_tx.send(Ok(())); value }
        Err(error) => { let _ = ready_tx.send(Err(error.clone())); return Err(error); }
    };
    let mut resampler = super::mixer::InputResampler::new();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(10));
        let mut received = false;
        while capture.GetNextPacketSize().map_err(|e| e.to_string())? > 0 {
            let mut data = std::ptr::null_mut();
            let mut frames = 0;
            let mut flags = 0;
            capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| e.to_string())?;
            let count = frames as usize * channels;
            let samples = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                vec![0.0; count]
            } else {
                (0..count).map(|i| sample_at(data, i, bits, float)).collect()
            };
            let converted = super::mixer::to_frames(samples.into_iter(), channels, rate as f32, &mut resampler);
            super::mixer::push_frames(&ring, &converted);
            capture.ReleaseBuffer(frames).map_err(|e| e.to_string())?;
            received = true;
        }
        if !received {
            super::mixer::push_frames(&ring, &vec![(0.0, 0.0); 441]);
        }
    }
    let _ = client.Stop();
    CoTaskMemFree(Some(format.cast()));
    Ok(())
}

unsafe fn sample_at(data: *mut u8, index: usize, bits: usize, float: bool) -> f32 {
    let ptr = data.add(index * (bits / 8));
    if float { return f32::from_le_bytes(std::slice::from_raw_parts(ptr, 4).try_into().unwrap()); }
    match bits {
        16 => i16::from_le_bytes(std::slice::from_raw_parts(ptr, 2).try_into().unwrap()) as f32 / 32768.0,
        24 => {
            let b = std::slice::from_raw_parts(ptr, 3);
            let v = ((b[0] as i32) | ((b[1] as i32) << 8) | ((b[2] as i32) << 16)) << 8 >> 8;
            v as f32 / 8_388_608.0
        }
        _ => i32::from_le_bytes(std::slice::from_raw_parts(ptr, 4).try_into().unwrap()) as f32 / 2_147_483_648.0,
    }
}
