//! Sound capture for recordings: the computer's own output (WASAPI loopback)
//! and the default microphone, both delivered as 48 kHz stereo f32.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::HSTRING;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoTaskMemFree, STGM_READ};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator, DEVICE_STATE_ACTIVE,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// How far a source may wander from the recording clock before it is
/// pulled back; less than this is timestamp jitter. 40 ms is under what
/// people notice as lip-sync error.
const DRIFT: i64 = RATE as i64 / 25;

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Source {
    /// Whatever the computer is playing.
    System,
    /// A microphone by device id; `None` is the Windows default.
    Microphone(Option<String>),
}

pub struct Mic {
    pub id: String,
    pub name: String,
}

/// Active microphones, the Windows default first.
pub fn microphones() -> Vec<Mic> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let Ok(devices) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else {
            return Vec::new();
        };
        let default = devices.GetDefaultAudioEndpoint(eCapture, eConsole).ok().and_then(|d| device_id(&d));
        let Ok(list) = devices.EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        let mut mics: Vec<Mic> = (0..list.GetCount().unwrap_or(0))
            .filter_map(|i| list.Item(i).ok())
            .filter_map(|d| {
                let id = device_id(&d)?;
                let store = d.OpenPropertyStore(STGM_READ).ok()?;
                let value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
                let text = PropVariantToStringAlloc(&value).ok()?;
                let name = text.to_string().unwrap_or_default();
                CoTaskMemFree(Some(text.0 as *const _));
                Some(Mic { id, name })
            })
            .collect();
        mics.sort_by_key(|m| Some(&m.id) != default.as_ref());
        mics
    }
}

unsafe fn device_id(d: &IMMDevice) -> Option<String> {
    unsafe {
        let raw = d.GetId().ok()?;
        let id = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const _));
        id
    }
}

/// A running capture. Interleaved stereo samples pile up in `samples` until
/// the recorder takes them.
pub struct Capture {
    pub samples: Arc<Mutex<VecDeque<f32>>>,
    stop: Arc<AtomicBool>,
    /// Why the device stopped mid-recording, until the recorder takes it.
    failure: Arc<Mutex<Option<String>>>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capturing; returns once the device is open (or failed to open).
    pub fn start(source: Source, clock: Instant) -> Result<Self, String> {
        let samples = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let failure = Arc::new(Mutex::new(None));
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = {
            let (samples, stop, failure) = (samples.clone(), stop.clone(), failure.clone());
            std::thread::spawn(move || {
                let client = match open(&source) {
                    Ok(c) => {
                        let _ = ready_tx.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut produced = 0;
                if let Err(e) = pump(&client, &samples, &stop, clock, &mut produced) {
                    let why = if e.code() == AUDCLNT_E_DEVICE_INVALIDATED { "it was disconnected".to_string() } else { e.to_string() };
                    *failure.lock().unwrap() = Some(why);
                    // The other source is mixed only as far as this one has
                    // samples, so keep this one's timeline moving with silence.
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(10));
                        keep_time(&samples, &mut produced, clock.elapsed());
                    }
                }
                unsafe {
                    let _ = client.0.Stop();
                }
            })
        };
        ready_rx
            .recv()
            .map_err(|_| "the sound device thread ended".to_string())??;
        Ok(Self { samples, stop, failure, thread: Some(thread) })
    }

    /// Why the device stopped mid-recording; reported once.
    pub fn take_failure(&self) -> Option<String> {
        self.failure.lock().unwrap().take()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Client(IAudioClient, IAudioCaptureClient);

/// What failed and why, never with Windows' text left blank.
fn fail(what: &str, e: windows::core::Error) -> String {
    format!("{what}: {}", crate::recorder::describe(&e))
}

fn open(source: &Source) -> Result<Client, String> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let devices: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| fail("sound devices", e))?;
        let device = match source {
            Source::System => devices
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| fail("no speakers or headphones found", e))?,
            Source::Microphone(None) => devices
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .map_err(|e| fail("no microphone found", e))?,
            Source::Microphone(Some(id)) => devices
                .GetDevice(&HSTRING::from(id.as_str()))
                .map_err(|e| fail("that microphone is gone", e))?,
        };
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| fail("open device", e))?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS as u16,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * CHANNELS as u32 * 4,
            nBlockAlign: CHANNELS as u16 * 4,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        // Windows converts whatever the device runs at into our format.
        let mut flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if *source == Source::System {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        let buffer_100ns = 2_000_000; // 200 ms
        client
            .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_100ns, 0, &format, None)
            .map_err(|e| fail("set up device", e))?;
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| fail("capture service", e))?;
        client.Start().map_err(|e| fail("start device", e))?;
        Ok(Client(client, capture))
    }
}

fn pump(
    client: &Client,
    samples: &Mutex<VecDeque<f32>>,
    stop: &AtomicBool,
    clock: Instant,
    produced: &mut u64,
) -> windows::core::Result<()> {
    // Devices start delivering after a warm-up (a Bluetooth headset can take
    // seconds), so the first packet is placed by its capture time rather than
    // by count, and so is the first one after a gap the device reports.
    let mut synced = false;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(10));
        unsafe {
            if client.1.GetNextPacketSize()? > 0 {
                // Packet timestamps are performance-counter time; read that
                // counter next to the recording clock to relate the two.
                let now_qpc = qpc_now();
                let now = clock.elapsed();
                while client.1.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut flags, mut qpc) = (0u32, 0u32, 0u64);
                    client.1.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                    let resync = !synced || flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
                    let at = if flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 == 0 && qpc != 0 {
                        Some(frame_at(now, now_qpc, qpc))
                    } else {
                        // No usable timestamp: take the packet as just arrived.
                        resync.then(|| frame_at(now, 0, 0) - frames as i64)
                    };
                    let (pad, skip) = at.map_or((0, 0), |at| place(*produced, at, frames as u64, resync));
                    synced = true;
                    let kept = frames as usize - skip as usize;
                    let mut q = samples.lock().unwrap();
                    q.extend(std::iter::repeat_n(0.0, pad as usize * CHANNELS));
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        q.extend(std::iter::repeat_n(0.0, kept * CHANNELS));
                    } else {
                        let all = std::slice::from_raw_parts(data as *const f32, frames as usize * CHANNELS);
                        q.extend(&all[skip as usize * CHANNELS..]);
                    }
                    drop(q);
                    *produced += pad + kept as u64;
                    client.1.ReleaseBuffer(frames)?;
                }
            }
        }
        // Loopback sends nothing while the computer is silent, and a
        // microphone nothing while it warms up or if it stalls. The mix only
        // goes as far as every queue, so fill the gap with silence; a packet
        // that then arrives late loses the overlap in `place`.
        keep_time(samples, produced, clock.elapsed());
    }
    Ok(())
}

// The `windows` crate only exposes these behind a feature this crate doesn't enable.
#[link(name = "kernel32")]
extern "system" {
    fn QueryPerformanceCounter(count: *mut i64) -> i32;
    fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
}

/// The performance counter in 100 ns units, the scale WASAPI stamps packets in.
fn qpc_now() -> u64 {
    let (mut count, mut frequency) = (0i64, 0i64);
    // Both always succeed on Windows XP and later.
    unsafe {
        QueryPerformanceCounter(&mut count);
        QueryPerformanceFrequency(&mut frequency);
    }
    (count as u128 * 10_000_000 / frequency as u128) as u64
}

/// The recording frame a packet's first sample belongs at, from its capture
/// time `qpc` and a counter reading `now_qpc` taken at recording time `now`
/// (counters in 100 ns units).
fn frame_at(now: Duration, now_qpc: u64, qpc: u64) -> i64 {
    let t = (now.as_nanos() / 100) as i64 - (now_qpc as i64 - qpc as i64);
    (t * RATE as i64).div_euclid(10_000_000)
}

/// Lines a packet of `frames` up with the recording clock: the frames of
/// silence to queue before it and the frames of it to drop. `at` is where its
/// first frame belongs and `produced` how far the queue has got. Within
/// `DRIFT` the sample count is trusted, unless `resync` (first packet, or a
/// gap the device reported) asks for an exact fit.
fn place(produced: u64, at: i64, frames: u64, resync: bool) -> (u64, u64) {
    let off = at - produced as i64;
    if !resync && off.abs() <= DRIFT {
        (0, 0)
    } else if off >= 0 {
        (off as u64, 0)
    } else {
        (0, off.unsigned_abs().min(frames))
    }
}

/// Pads `samples` with silence once it falls more than 100 ms behind
/// `elapsed`, so a source that sends nothing still keeps the recording's time.
fn keep_time(samples: &Mutex<VecDeque<f32>>, produced: &mut u64, elapsed: Duration) {
    let due = (elapsed.as_secs_f64() * RATE as f64) as u64;
    let slack = RATE as u64 / 10;
    if due > *produced + slack {
        let missing = due - *produced - slack / 2;
        samples.lock().unwrap().extend(std::iter::repeat_n(0.0, missing as usize * CHANNELS));
        *produced += missing;
    }
}

/// Takes what every capture has in common and adds it together.
pub fn mix(captures: &[Capture], out: &mut Vec<f32>) {
    let queues: Vec<&Mutex<VecDeque<f32>>> = captures.iter().map(|c| &*c.samples).collect();
    mix_queues(&queues, out);
}

fn mix_queues(queues: &[&Mutex<VecDeque<f32>>], out: &mut Vec<f32>) {
    out.clear();
    let ready = queues
        .iter()
        .map(|q| q.lock().unwrap().len())
        .min()
        .unwrap_or(0);
    let ready = ready - ready % CHANNELS;
    if ready == 0 {
        return;
    }
    out.resize(ready, 0.0);
    for q in queues {
        let mut q = q.lock().unwrap();
        for (o, s) in out.iter_mut().zip(q.drain(..ready)) {
            *o += s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device that died stops sending; its silence fill must keep mixing
    /// going for the other source and stop the other queue from growing.
    #[test]
    fn dead_source_does_not_stall_the_mix() {
        let (live, dead) = (Mutex::new(VecDeque::new()), Mutex::new(VecDeque::new()));
        let mut dead_produced = 0;
        let tick = RATE as usize / 100; // 10 ms of frames
        let mut out = Vec::new();
        let mut mixed = 0;
        for i in 1..=1000 {
            // 10 s of recording: the live source keeps delivering, the dead one only gets padded.
            live.lock().unwrap().extend(std::iter::repeat_n(0.5, tick * CHANNELS));
            keep_time(&dead, &mut dead_produced, Duration::from_millis(i * 10));
            mix_queues(&[&live, &dead], &mut out);
            mixed += out.len();
            assert!(out.iter().all(|&s| s == 0.5));
            assert!(live.lock().unwrap().len() <= (RATE as usize / 10 + tick) * CHANNELS, "live queue grew at tick {i}");
        }
        assert!(mixed >= (RATE as usize * 10 - RATE as usize / 10) * CHANNELS, "only {mixed} samples mixed");
    }

    #[test]
    fn device_errors_without_text_still_say_something() {
        assert_eq!(fail("set up device", windows::core::Error::empty()), "set up device: unknown error");
        let text = fail("set up device", windows::core::Error::from_hresult(windows::core::HRESULT(0x8889_0000u32 as i32)));
        assert!(text.len() > "set up device: ".len() + 3, "{text}");
    }

    #[test]
    fn packet_time_maps_onto_the_recording_clock() {
        // Captured 250 ms before a reading taken 1 s into the recording.
        let now_qpc = 123_456_789_000;
        assert_eq!(frame_at(Duration::from_secs(1), now_qpc, now_qpc - 2_500_000), RATE as i64 * 3 / 4);
        // Captured before the recording began.
        assert_eq!(frame_at(Duration::from_millis(100), now_qpc, now_qpc - 2_000_000), -(RATE as i64) / 10);
    }

    #[test]
    fn late_starting_source_is_padded_to_the_clock() {
        // A microphone whose first packet was captured 300 ms in: 300 ms of silence first.
        let at = RATE as i64 * 3 / 10;
        assert_eq!(place(0, at, 480, true), (at as u64, 0));
        // Later packets that follow on within jitter are queued as they come.
        assert_eq!(place(at as u64 + 480, at + 480 + 30, 480, false), (0, 0));
        assert_eq!(place(at as u64 + 480, at + 480 - 30, 480, false), (0, 0));
    }

    /// A microphone still warming up is kept on the clock with silence, and
    /// its first packet then lands exactly where it was captured.
    #[test]
    fn warming_up_source_keeps_time_then_lines_up() {
        let q = Mutex::new(VecDeque::new());
        let mut produced = 0;
        for ms in (10..=300).step_by(10) {
            keep_time(&q, &mut produced, Duration::from_millis(ms));
        }
        assert!(produced >= RATE as u64 / 10, "only {produced} frames of silence after 300 ms");
        // First packet: 10 ms captured 280 ms in.
        let at = RATE as i64 * 28 / 100;
        let (pad, skip) = place(produced, at, 480, true);
        assert_eq!(produced + pad + 480 - skip, at as u64 + 480);
    }

    #[test]
    fn gaps_and_drift_are_pulled_back_to_the_clock() {
        // The device reports a 200 ms glitch: silence covers it.
        assert_eq!(place(48_000, 48_000 + 9_600, 480, true), (9_600, 0));
        // Even a small reported gap is filled exactly.
        assert_eq!(place(48_000, 48_010, 480, true), (10, 0));
        // Drift past the limit without a flag is corrected too.
        assert_eq!(place(48_000, 48_000 + DRIFT + 1, 480, false), (DRIFT as u64 + 1, 0));
        // A packet overlapping what is already queued loses the overlap...
        assert_eq!(place(48_000, 48_000 - 100, 480, true), (0, 100));
        // ...or is dropped whole when it lies entirely in the past.
        assert_eq!(place(48_000, 40_000, 480, true), (0, 480));
        assert_eq!(place(1_000, -500, 480, true), (0, 480));
    }

    /// Needs a microphone; run with `cargo test -- --ignored`. The recording
    /// began half a second before the device opened, so its sound must be
    /// queued from half a second in.
    #[test]
    #[ignore]
    fn microphone_starts_on_the_recording_clock() {
        let clock = Instant::now() - Duration::from_millis(500);
        let cap = Capture::start(Source::Microphone(None), clock).expect("open microphone");
        std::thread::sleep(Duration::from_millis(1000));
        let got = cap.samples.lock().unwrap().len() as i64 / CHANNELS as i64;
        let due = (clock.elapsed().as_secs_f64() * RATE as f64) as i64;
        drop(cap);
        // Behind by no more than the device's buffering.
        assert!(got <= due && got > due - RATE as i64 / 10, "{got} frames queued, {due} due");
    }

    /// Needs sound devices; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn lists_microphones_by_name() {
        let mics = microphones();
        for m in &mics {
            println!("mic: {}", m.name);
        }
        assert!(mics.iter().all(|m| !m.name.is_empty() && !m.id.is_empty()));
    }

    /// Needs a sound output device; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn loopback_keeps_time_even_when_silent() {
        let clock = Instant::now();
        let cap = Capture::start(Source::System, clock).expect("open loopback");
        std::thread::sleep(Duration::from_millis(1000));
        let got = cap.samples.lock().unwrap().len() / CHANNELS;
        drop(cap);
        assert!(got > RATE as usize * 8 / 10, "only {got} frames in 1 s");
    }
}
