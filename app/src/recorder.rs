//! Screen recording to MP4 (H.264 + AAC) with Windows Media Foundation.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use eframe::egui::{Color32, ColorImage};
use windows::core::HSTRING;
use windows::core::HRESULT;
use windows::core::Interface;
use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_BOX,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
    DXGI_MODE_ROTATION_ROTATE90, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, BITMAPINFO,
    BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, DI_NORMAL, ICONINFO,
};

use crate::audio::{self, Capture, Source};

pub const FPS: u32 = 30;
const TICKS: i64 = 10_000_000; // Media Foundation counts time in 100 ns ticks.
/// Hardware H.264 encoders commonly stop at 4096 pixels per side.
const MAX_SIDE: i32 = 4096;
/// How far the sound track may trail the picture before silence fills it.
const AUDIO_GAP: i64 = TICKS / 2;

pub struct Settings {
    /// Screen pixels to record: x, y, width, height.
    pub area: [i32; 4],
    pub system_sound: bool,
    /// `Some(None)` records the default microphone, `Some(Some(id))` a chosen one.
    pub microphone: Option<Option<String>>,
    pub path: PathBuf,
}

pub struct Finished {
    pub path: PathBuf,
    pub seconds: f64,
    pub size: [u32; 2],
    pub first_frame: Option<ColorImage>,
    /// Why the recording ended before the user stopped it; the video up to
    /// that point was still saved.
    pub stopped_early: Option<String>,
}

pub enum Event {
    /// Recording is running. `area` is the desktop rectangle actually being
    /// recorded (x, y, w, h), which can be smaller than the one asked for;
    /// `notes` says why, and lists sound sources that couldn't start.
    Started { area: [i32; 4], notes: Vec<String> },
    /// A sound source stopped mid-recording; the rest carries on.
    Note(String),
    Done(Result<Finished, String>),
}

pub struct Recording {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub started: Instant,
}

impl Recording {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        self.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// H.264 level 5.1/5.2 frame size limit in 16×16 macroblocks (4096×2304).
/// The encoders here took 4096×2880 and 4096×4096 too (level 6 territory),
/// but level 5.2 is the largest frame players and hardware decoders broadly
/// accept, so a recording past it may encode fine and then not play.
const MAX_MACROBLOCKS: i32 = 36_864;

/// Even sizes are required by H.264; oversized areas are trimmed: each side
/// to `MAX_SIDE`, then the shorter side until the frame fits `MAX_MACROBLOCKS`.
// ceiling: trimming cuts off the right or bottom of a 5K/6K area; scale the
// frames down instead if people record whole screens that large.
pub fn fit_area([x, y, w, h]: [i32; 4]) -> [i32; 4] {
    let (mut w, mut h) = (w.min(MAX_SIDE) & !1, h.min(MAX_SIDE) & !1);
    let mbs = |side: i32| (side + 15) / 16;
    if w > 0 && h > 0 && mbs(w) * mbs(h) > MAX_MACROBLOCKS {
        // The shorter side gives way: a 5K or 6K screen then keeps 16:9 at 4096×2304.
        if w > h {
            h = h.min(MAX_MACROBLOCKS / mbs(w) * 16) & !1;
        } else {
            w = w.min(MAX_MACROBLOCKS / mbs(h) * 16) & !1;
        }
    }
    [x, y, w, h]
}

pub fn start(settings: Settings, on_event: impl Fn(Event) + Send + 'static) -> Recording {
    let stop = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    let thread = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut settings = settings;
            // Claim a name nobody else has, so the clean-up below can only
            // ever remove this recording's own file.
            let result = crate::win::create_unique(&settings.path).and_then(|(path, file)| {
                drop(file);
                settings.path = path;
                let result = record(&settings, &stop, started, &on_event);
                if result.is_err() {
                    let _ = std::fs::remove_file(&settings.path);
                }
                result
            });
            on_event(Event::Done(result));
        })
    };
    Recording { stop, thread: Some(thread), started }
}

fn record(
    s: &Settings,
    stop: &AtomicBool,
    clock: Instant,
    on_event: &impl Fn(Event),
) -> Result<Finished, String> {
    let mut grab = Grabber::new(fit_area(s.area)).map_err(|e| format!("couldn't read the screen: {e}"))?;
    let [_, _, w, h] = grab.area;
    if w < 32 || h < 32 {
        return Err("the area is too small to record".into());
    }
    let mut captures = Vec::new();
    let mut names = Vec::new();
    let mut notes = Vec::new();
    if grab.area != fit_area(s.area) {
        notes.push("Only the part on one screen is recorded".to_string());
    }
    let wanted = [
        s.system_sound.then_some((Source::System, "the computer's sound")),
        s.microphone.clone().map(|id| (Source::Microphone(id), "the microphone")),
    ];
    for (source, name) in wanted.into_iter().flatten() {
        match Capture::start(source, clock) {
            Ok(c) => {
                captures.push(c);
                names.push(name);
            }
            Err(e) => notes.push(format!("Recording without {name} ({e})")),
        }
    }
    let mut enc = Encoder::new(&s.path, w as u32, h as u32, !captures.is_empty())
        .map_err(|e| format!("couldn't start the video encoder: {}", describe(&e)))?;
    on_event(Event::Started { area: grab.area, notes });

    let frame_time = Duration::from_secs(1) / FPS;
    let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
    let mut mixed = Vec::new();
    let mut first_frame = None;
    let mut next = clock;
    let mut last_video = -1i64;
    let werr = |e: windows::core::Error| format!("couldn't write the video: {}", describe(&e));
    let stopped_early = pump(&mut enc, stop, |enc| {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        }
        // Skip ticks we've fallen behind on rather than bunching frames up.
        while next <= Instant::now() {
            next += frame_time;
        }
        let bgra = grab.frame().map_err(|e| format!("couldn't read the screen: {}", describe(&e)))?;
        if first_frame.is_none() {
            first_frame = Some(to_color_image(bgra, w as usize, h as usize));
        }
        bgra_to_nv12(bgra, w as usize, h as usize, &mut nv12);
        let t = (clock.elapsed().as_secs_f64() * TICKS as f64) as i64;
        let wrote = t > last_video;
        if wrote {
            enc.video(&nv12, t).map_err(werr)?;
            last_video = t;
        }
        audio::mix(&captures, &mut mixed);
        enc.audio(&mixed).map_err(werr)?;
        for (c, name) in captures.iter().zip(&names) {
            if let Some(e) = c.take_failure() {
                on_event(Event::Note(format!("Lost {name} ({e}); recording continues without it")));
            }
        }
        Ok(wrote)
    })?;
    let seconds = clock.elapsed().as_secs_f64();
    drop(captures);
    enc.finish().map_err(|e| finalize_failed(&e, stopped_early.as_deref()))?;
    Ok(Finished { path: s.path.clone(), seconds, size: [w as u32, h as u32], first_frame, stopped_early })
}

/// Runs `step` (which reports whether it wrote a video frame) until `stop` is
/// set or it fails. A failure after the first video frame ends the recording
/// early rather than losing it: the reason comes back and the caller still
/// finalizes the file. With no frame written there is nothing worth keeping.
fn pump(
    enc: &mut Encoder,
    stop: &AtomicBool,
    mut step: impl FnMut(&mut Encoder) -> Result<bool, String>,
) -> Result<Option<String>, String> {
    let mut frames = 0u64;
    while !stop.load(Ordering::Relaxed) {
        match step(enc) {
            Ok(wrote) => frames += wrote as u64,
            Err(e) if frames == 0 => return Err(e),
            Err(e) => return Ok(Some(e)),
        }
    }
    if frames == 0 {
        return Err("nothing was recorded".into());
    }
    Ok(None)
}

/// Why the file couldn't be finished, with the reason the recording had
/// already stopped early, if it had: that is usually the cause of both.
fn finalize_failed(e: &windows::core::Error, stopped_early: Option<&str>) -> String {
    let failed = format!("couldn't write the video: {}", describe(e));
    match stopped_early {
        Some(why) => format!("the recording stopped early ({why}), then {failed}"),
        None => failed,
    }
}

/// `Error::empty()` reads as "The operation completed successfully." and
/// some codes carry no text; never show either to the user.
pub(crate) fn describe(e: &windows::core::Error) -> String {
    let code = e.code().0;
    if code == 0 {
        return "unknown error".into();
    }
    let text = e.message();
    let text = text.trim();
    if text.is_empty() { format!("error {:#010x}", code as u32) } else { text.to_string() }
}

/// Why a `Grabber` could not be set up. Failures that Windows reports with no
/// error code (a missing monitor, a failed allocation) get their own wording
/// here instead of `Error::empty()`'s empty text.
#[derive(Debug)]
enum GrabError {
    Win(windows::core::Error),
    /// The area's centre is not on any monitor.
    NoMonitor,
    /// The graphics driver gave back no device or context.
    NoDevice,
    /// The memory bitmap for drawing the pointer could not be allocated.
    NoBitmap,
}

impl From<windows::core::Error> for GrabError {
    fn from(e: windows::core::Error) -> Self {
        Self::Win(e)
    }
}

impl std::fmt::Display for GrabError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Win(e) => f.write_str(&describe(e)),
            Self::NoMonitor => f.write_str("the area isn't on any screen"),
            Self::NoDevice => f.write_str("the graphics driver didn't provide a device"),
            Self::NoBitmap => f.write_str("not enough memory for the recording buffer"),
        }
    }
}

/// Wait between attempts to re-open a lost desktop duplication: short at
/// first so a dismissed UAC prompt resumes quickly, then capped.
const REOPEN_FIRST: Duration = Duration::from_millis(100);
const REOPEN_MAX: Duration = Duration::from_secs(1);
/// Longest wait for the first screen image before copying it another way:
/// a still screen sends none, so this delays the start of such recordings.
const FIRST_FRAME_WAIT: Duration = Duration::from_millis(250);

/// Reads one monitor through DXGI Desktop Duplication (GPU copy, no stalls
/// from other windows redrawing) and draws the mouse pointer on top.
struct Grabber {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    /// `None` while the desktop is out of reach (UAC, lock screen, mode
    /// change); the last image keeps being recorded until it comes back.
    dup: Option<IDXGIOutputDuplication>,
    /// Next attempt to re-open a lost duplication, and the wait after that.
    reopen_at: Instant,
    reopen_wait: Duration,
    /// How the monitor is turned; duplication hands over its image unturned.
    rotation: DXGI_MODE_ROTATION,
    /// CPU-readable copy of the area as it lies in the unturned image, and its size.
    staging: ID3D11Texture2D,
    staging_size: [i32; 2],
    /// Area relative to the monitor's top-left: x, y, w, h.
    local: [i32; 4],
    /// The same area on the desktop.
    area: [i32; 4],
    /// Last screen image without the pointer, BGRA rows.
    clean: Vec<u8>,
    have_frame: bool,
    mem: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    pixels: *mut u8,
}

impl Grabber {
    /// Uses the monitor holding the area's centre and trims the area to it.
    // ceiling: one monitor per recording; spanning monitors would need one
    // duplication per output stitched together.
    fn new(area: [i32; 4]) -> Result<Self, GrabError> {
        let [x, y, w, h] = area;
        let (cx, cy) = (x + w / 2, y + h / 2);
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut found = None;
            let mut a = 0;
            'search: while let Ok(adapter) = factory.EnumAdapters1(a) {
                let mut o = 0;
                while let Ok(output) = adapter.EnumOutputs(o) {
                    let r = output.GetDesc()?.DesktopCoordinates;
                    if cx >= r.left && cx < r.right && cy >= r.top && cy < r.bottom {
                        found = Some((adapter, output, r));
                        break 'search;
                    }
                    o += 1;
                }
                a += 1;
            }
            let (adapter, output, mon) = found.ok_or(GrabError::NoMonitor)?;
            let area = on_monitor(area, [mon.left, mon.top, mon.right, mon.bottom]);
            let local = [area[0] - mon.left, area[1] - mon.top, area[2], area[3]];

            let (mut device, mut context) = (None, None);
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or(GrabError::NoDevice)?;
            let context = context.ok_or(GrabError::NoDevice)?;
            let output: IDXGIOutput1 = output.cast()?;
            let dup = output.DuplicateOutput(&device)?;
            let rotation = dup.GetDesc().Rotation;
            let [_, _, sw, sh] = texture_rect(local, rotation, [0, 0]);
            let staging = make_staging(&device, [sw, sh])?;

            // A memory bitmap to draw the pointer into with GDI.
            let mem = CreateCompatibleDC(null_mut());
            let mut bi: BITMAPINFO = zeroed();
            bi.bmiHeader.biSize = size_of::<windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
            bi.bmiHeader.biWidth = area[2];
            bi.bmiHeader.biHeight = -area[3]; // top-down rows
            bi.bmiHeader.biPlanes = 1;
            bi.bmiHeader.biBitCount = 32;
            bi.bmiHeader.biCompression = BI_RGB;
            let mut pixels: *mut c_void = null_mut();
            let bitmap = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut pixels, null_mut(), 0);
            if bitmap.is_null() || pixels.is_null() {
                DeleteDC(mem);
                return Err(GrabError::NoBitmap);
            }
            let old = SelectObject(mem, bitmap);
            Ok(Self {
                device,
                context,
                output,
                dup: Some(dup),
                reopen_at: Instant::now(),
                reopen_wait: REOPEN_FIRST,
                rotation,
                staging,
                staging_size: [sw, sh],
                local,
                area,
                clean: vec![0; (area[2] * area[3] * 4) as usize],
                have_frame: false,
                mem,
                bitmap,
                old,
                pixels: pixels as *mut u8,
            })
        }
    }

    /// Pulls a new screen image if one is ready. Waits up to `wait_ms` for it.
    /// A lost desktop is not an error: the last image stays and re-opening is
    /// retried on later calls.
    fn refresh(&mut self, wait_ms: u32) -> windows::core::Result<()> {
        unsafe {
            let Some(dup) = &self.dup else { return self.reopen() };
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource = None;
            if let Err(e) = dup.AcquireNextFrame(wait_ms, &mut info, &mut resource) {
                return match e.code() {
                    DXGI_ERROR_WAIT_TIMEOUT => Ok(()),
                    // Resolution change, full-screen app, UAC, lock screen.
                    DXGI_ERROR_ACCESS_LOST => self.lost(),
                    _ => Err(e),
                };
            }
            let copied = (|| {
                // Pointer-only updates carry no new desktop image. The first
                // frame after `DuplicateOutput` can be one too, with a black texture.
                if info.LastPresentTime == 0 {
                    return Ok(());
                }
                let texture: ID3D11Texture2D = resource.ok_or_else(windows::core::Error::empty)?.cast()?;
                let mut desktop = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desktop);
                // ceiling: after a mode change the area keeps its old monitor-relative
                // position and the encoder its size; a smaller, moved or newly turned
                // desktop records the overlapping part (the rest keeps the last image).
                // Remap/scale the area if people record across display changes.
                let [_, _, w, h] = self.local;
                let rect = texture_rect(self.local, self.rotation, [desktop.Width, desktop.Height]);
                let [rx, ry, rw, rh] = rect;
                if [rw, rh] != self.staging_size {
                    // The monitor was turned between portrait and landscape.
                    self.staging = make_staging(&self.device, [rw, rh])?;
                    self.staging_size = [rw, rh];
                }
                // Only the copied part is read back: the rest of `staging` is
                // stale or, after a turn, blank.
                let Some(src) = clip_box(rect, [desktop.Width, desktop.Height]) else { return Ok(()) };
                let (dx, dy) = ((src.left as i32 - rx) as u32, (src.top as i32 - ry) as u32);
                self.context.CopySubresourceRegion(&self.staging, 0, dx, dy, 0, &texture, 0, Some(&src));
                let copied = [dx, dy, dx + src.right - src.left, dy + src.bottom - src.top].map(|v| v as usize);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let pitch = mapped.RowPitch as usize;
                let src = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (rh as usize - 1) + rw as usize * 4);
                unrotate(src, pitch, self.rotation, w as usize, h as usize, copied, &mut self.clean);
                self.context.Unmap(&self.staging, 0);
                self.have_frame = true;
                Ok(())
            })();
            if let Err(e) = dup.ReleaseFrame() {
                if e.code() != DXGI_ERROR_ACCESS_LOST {
                    return Err(e);
                }
                self.lost()?;
            }
            copied
        }
    }

    /// The Desktop Duplication docs require releasing the old duplication
    /// before creating a new one, or `DuplicateOutput` fails.
    fn lost(&mut self) -> windows::core::Result<()> {
        self.dup = None;
        self.reopen_at = Instant::now();
        self.reopen_wait = REOPEN_FIRST;
        self.reopen()
    }

    /// Tries to duplicate the output again once its retry time has come.
    fn reopen(&mut self) -> windows::core::Result<()> {
        let now = Instant::now();
        if now < self.reopen_at {
            return Ok(());
        }
        match unsafe { self.output.DuplicateOutput(&self.device) } {
            Ok(dup) => {
                self.rotation = unsafe { dup.GetDesc() }.Rotation;
                self.dup = Some(dup);
                Ok(())
            }
            Err(e) if reopen_is_transient(e.code()) => {
                self.reopen_at = now + self.reopen_wait;
                self.reopen_wait = next_reopen_wait(self.reopen_wait);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Waits up to `FIRST_FRAME_WAIT` for duplication's first image. On a
    /// still screen duplication hands over only a pointer update with a
    /// black texture until something changes, so it then falls back to one
    /// GDI copy rather than starting black.
    fn first_image(&mut self) -> windows::core::Result<()> {
        let deadline = Instant::now() + FIRST_FRAME_WAIT;
        while !self.have_frame {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return self.copy_with_gdi();
            }
            if self.dup.is_none() {
                std::thread::sleep(left.min(Duration::from_millis(10)));
            }
            self.refresh(left.as_millis().max(1) as u32)?;
        }
        Ok(())
    }

    /// Copies the area off the screen with GDI into `clean`. Upright already:
    /// GDI works in desktop coordinates, whatever way the monitor is turned.
    fn copy_with_gdi(&mut self) -> windows::core::Result<()> {
        let [x, y, w, h] = self.area;
        unsafe {
            let screen = GetDC(null_mut());
            if screen.is_null() {
                return Err(windows::core::Error::from_thread());
            }
            let ok = BitBlt(self.mem, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT);
            ReleaseDC(null_mut(), screen);
            if ok == 0 {
                return Err(windows::core::Error::from_thread());
            }
            let len = (w * h * 4) as usize;
            std::ptr::copy_nonoverlapping(self.pixels, self.clean.as_mut_ptr(), len);
        }
        self.have_frame = true;
        Ok(())
    }

    /// The area as BGRA rows, with the mouse pointer drawn in.
    fn frame(&mut self) -> windows::core::Result<&[u8]> {
        if self.have_frame {
            self.refresh(0)?;
        } else {
            self.first_image()?;
        }
        let [x, y, w, h] = self.area;
        unsafe {
            let len = (w * h * 4) as usize;
            std::ptr::copy_nonoverlapping(self.clean.as_ptr(), self.pixels, len);
            let mut ci: CURSORINFO = zeroed();
            ci.cbSize = size_of::<CURSORINFO>() as u32;
            if GetCursorInfo(&mut ci) != 0 && ci.flags & CURSOR_SHOWING != 0 {
                let mut ii: ICONINFO = zeroed();
                if GetIconInfo(ci.hCursor, &mut ii) != 0 {
                    let (hx, hy) = (ii.xHotspot as i32, ii.yHotspot as i32);
                    if !ii.hbmMask.is_null() {
                        DeleteObject(ii.hbmMask);
                    }
                    if !ii.hbmColor.is_null() {
                        DeleteObject(ii.hbmColor);
                    }
                    DrawIconEx(self.mem, ci.ptScreenPos.x - x - hx, ci.ptScreenPos.y - y - hy, ci.hCursor, 0, 0, 0, null_mut(), DI_NORMAL);
                }
            }
            Ok(std::slice::from_raw_parts(self.pixels, len))
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.mem);
        }
    }
}

/// The part of `area` (x, y, w, h) on the monitor `[left, top, right, bottom]`,
/// trimmed to a size the encoder takes.
fn on_monitor([x, y, w, h]: [i32; 4], [ml, mt, mr, mb]: [i32; 4]) -> [i32; 4] {
    let (left, top) = (x.max(ml), y.max(mt));
    let (right, bottom) = ((x + w).min(mr), (y + h).min(mb));
    fit_area([left, top, right - left, bottom - top])
}

/// `DuplicateOutput` failures that clear up by themselves: the secure desktop
/// (UAC, lock screen) is up, the display is mid mode change or held by a
/// full-screen app, the session is switched away, or the per-system limit of
/// duplications is reached for now.
// ceiling: DXGI_ERROR_DEVICE_REMOVED (GPU reset, driver update) still ends the
// recording; recreating the device and staging texture would cover it if that shows up.
fn reopen_is_transient(code: HRESULT) -> bool {
    [E_ACCESSDENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED].contains(&code)
}

fn next_reopen_wait(wait: Duration) -> Duration {
    (wait * 2).min(REOPEN_MAX)
}

/// The part of `local` (x, y, w, h) inside a desktop texture of `size`, as a
/// copy box; `None` when they don't overlap.
fn clip_box([x, y, w, h]: [i32; 4], [tw, th]: [u32; 2]) -> Option<D3D11_BOX> {
    let (tw, th) = (tw as i64, th as i64);
    let (left, right) = ((x as i64).clamp(0, tw), (x as i64 + w as i64).clamp(0, tw));
    let (top, bottom) = ((y as i64).clamp(0, th), (y as i64 + h as i64).clamp(0, th));
    (left < right && top < bottom).then_some(D3D11_BOX {
        left: left as u32,
        top: top as u32,
        front: 0,
        right: right as u32,
        bottom: bottom as u32,
        back: 1,
    })
}

fn make_staging(device: &ID3D11Device, [w, h]: [i32; 2]) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w as u32,
        Height: h as u32,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging))? };
    staging.ok_or_else(windows::core::Error::empty)
}

/// Where the area `local` (x, y, w, h on the monitor as people see it) lies
/// in the unturned duplication image of size `[tw, th]`, as x, y, w, h. The
/// mapping follows Microsoft's Desktop Duplication sample: turned 90°, the
/// desktop's top-left pixel sits at the image's bottom-left.
fn texture_rect([x, y, w, h]: [i32; 4], rotation: DXGI_MODE_ROTATION, [tw, th]: [u32; 2]) -> [i32; 4] {
    let (tw, th) = (tw as i32, th as i32);
    match rotation {
        DXGI_MODE_ROTATION_ROTATE90 => [y, th - x - w, h, w],
        DXGI_MODE_ROTATION_ROTATE180 => [tw - x - w, th - y - h, w, h],
        DXGI_MODE_ROTATION_ROTATE270 => [tw - y - h, x, h, w],
        _ => [x, y, w, h],
    }
}

/// Copies the area out of `src` (its `texture_rect`, rows `pitch` bytes
/// apart, BGRA) into `dst` as upright `w`×`h` BGRA rows. Only the part of
/// `src` inside `copied` (left, top, right, bottom) is taken; the rest of
/// `dst` keeps what it had.
fn unrotate(src: &[u8], pitch: usize, rotation: DXGI_MODE_ROTATION, w: usize, h: usize, [x0, y0, x1, y1]: [usize; 4], dst: &mut [u8]) {
    let turned = matches!(rotation, DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE180 | DXGI_MODE_ROTATION_ROTATE270);
    for (y, out) in dst.chunks_exact_mut(w * 4).take(h).enumerate() {
        if !turned {
            if (y0..y1).contains(&y) && x0 < x1 {
                out[x0 * 4..x1 * 4].copy_from_slice(&src[y * pitch + x0 * 4..][..(x1 - x0) * 4]);
            }
            continue;
        }
        for (x, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (sx, sy) = match rotation {
                DXGI_MODE_ROTATION_ROTATE90 => (y, w - 1 - x),
                DXGI_MODE_ROTATION_ROTATE180 => (w - 1 - x, h - 1 - y),
                _ => (h - 1 - y, x),
            };
            if (x0..x1).contains(&sx) && (y0..y1).contains(&sy) {
                px.copy_from_slice(&src[sy * pitch + sx * 4..][..4]);
            }
        }
    }
}

fn to_color_image(bgra: &[u8], w: usize, h: usize) -> ColorImage {
    let pixels = bgra.as_chunks::<4>().0.iter().map(|p| Color32::from_rgb(p[2], p[1], p[0])).collect();
    ColorImage::new([w, h], pixels)
}

/// BGRA → NV12 with BT.709 limited-range colours, the H.264 norm for HD.
fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, out: &mut [u8]) {
    let (luma, chroma) = out.split_at_mut(w * h);
    for (src, dst) in bgra.chunks_exact(w * 4).zip(luma.chunks_exact_mut(w)) {
        for (p, y) in src.as_chunks::<4>().0.iter().zip(dst.iter_mut()) {
            let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
            *y = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
        }
    }
    for cy in 0..h / 2 {
        let row0 = &bgra[cy * 2 * w * 4..][..w * 4];
        let row1 = &bgra[(cy * 2 + 1) * w * 4..][..w * 4];
        let dst = &mut chroma[cy * w..][..w];
        for cx in 0..w / 2 {
            let i = cx * 8;
            let sum = |o: usize| {
                row0[i + o] as i32 + row0[i + 4 + o] as i32 + row1[i + o] as i32 + row1[i + 4 + o] as i32
            };
            let (b, g, r) = ((sum(0) + 2) >> 2, (sum(1) + 2) >> 2, (sum(2) + 2) >> 2);
            dst[cx * 2] = (((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
            dst[cx * 2 + 1] = (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(0, 255) as u8;
        }
    }
}

/// Keeps Media Foundation started while it lives. `MFStartup` and
/// `MFShutdown` are counted per process, so every user holds its own.
struct MediaFoundation;

impl MediaFoundation {
    fn start() -> windows::core::Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
        }
        Ok(Self)
    }
}

impl Drop for MediaFoundation {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

struct Encoder {
    writer: IMFSinkWriter,
    video: u32,
    audio: Option<u32>,
    frame_bytes: u32,
    /// Sound frames written, silence from `fill_audio_gap` included.
    audio_frames: i64,
    /// Sound frames handed to `audio`; behind `audio_frames` after a fill.
    audio_given: i64,
    /// Last, so it drops after `writer`: fields drop in order.
    _mf: MediaFoundation,
}

impl Encoder {
    fn new(path: &Path, w: u32, h: u32, with_audio: bool) -> windows::core::Result<Self> {
        unsafe {
            let mf = MediaFoundation::start()?;
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.ok_or_else(windows::core::Error::empty)?;
            attrs.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
            let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attrs)?;

            let size = ((w as u64) << 32) | h as u64;
            let rate = ((FPS as u64) << 32) | 1;
            let square = (1u64 << 32) | 1;
            let out = MFCreateMediaType()?;
            out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            // About 0.12 bits per pixel per frame: crisp text, modest files.
            out.SetUINT32(&MF_MT_AVG_BITRATE, ((w * h) as f64 * FPS as f64 * 0.12) as u32)?;
            out.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32)?;
            out.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            out.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            out.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square)?;
            let video = writer.AddStream(&out)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w)?;
            input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size)?;
            input.SetUINT64(&MF_MT_FRAME_RATE, rate)?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, square)?;
            writer.SetInputMediaType(video, &input, None)?;

            let audio = if with_audio {
                let out = MFCreateMediaType()?;
                out.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                out.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
                out.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                out.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio::RATE)?;
                out.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, audio::CHANNELS as u32)?;
                out.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 24_000)?; // 192 kbit/s
                let stream = writer.AddStream(&out)?;
                let pcm = MFCreateMediaType()?;
                pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
                pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
                pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, audio::RATE)?;
                pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, audio::CHANNELS as u32)?;
                pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, audio::CHANNELS as u32 * 2)?;
                pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, audio::RATE * audio::CHANNELS as u32 * 2)?;
                writer.SetInputMediaType(stream, &pcm, None)?;
                Some(stream)
            } else {
                None
            };
            writer.BeginWriting()?;
            Ok(Self { writer, video, audio, frame_bytes: w * h * 3 / 2, audio_frames: 0, audio_given: 0, _mf: mf })
        }
    }

    /// The sink writer is left throttled: while the encoder is behind,
    /// `WriteSample` blocks rather than queueing without bound, and the
    /// recording loop then skips the ticks it missed (drops frames).
    fn write(&self, stream: u32, bytes: &[u8], time: i64, duration: i64) -> windows::core::Result<()> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut dst = null_mut();
            buffer.Lock(&mut dst, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(duration)?;
            self.writer.WriteSample(stream, &sample)
        }
    }

    fn video(&mut self, nv12: &[u8], time: i64) -> windows::core::Result<()> {
        debug_assert_eq!(nv12.len() as u32, self.frame_bytes);
        self.fill_audio_gap(time)?;
        self.write(self.video, nv12, time, TICKS / FPS as i64)
    }

    /// The throttled sink writer holds video back while the sound track
    /// trails it by a few seconds, and the recorder would then hang inside
    /// `WriteSample`. A sound source that is still warming up or stuck leaves
    /// the mix empty, so silence fills the track up to `AUDIO_GAP` behind the
    /// picture; `audio` then skips the sound that silence stands in for.
    fn fill_audio_gap(&mut self, video_time: i64) -> windows::core::Result<()> {
        let Some(stream) = self.audio else { return Ok(()) };
        let due = (video_time - AUDIO_GAP) * audio::RATE as i64 / TICKS;
        if due <= self.audio_frames {
            return Ok(());
        }
        let frames = due - self.audio_frames;
        self.write_pcm(stream, &vec![0u8; frames as usize * audio::CHANNELS * 2], frames)
    }

    /// Interleaved stereo f32, following on from the last call. Time comes
    /// from the sample count so the sound track never drifts from its own clock.
    fn audio(&mut self, samples: &[f32]) -> windows::core::Result<()> {
        let Some(stream) = self.audio else { return Ok(()) };
        let given = (samples.len() / audio::CHANNELS) as i64;
        let skip = (self.audio_frames - self.audio_given).clamp(0, given);
        self.audio_given += given;
        let samples = &samples[skip as usize * audio::CHANNELS..];
        if samples.is_empty() {
            return Ok(());
        }
        let pcm: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
            .collect();
        self.write_pcm(stream, &pcm, given - skip)
    }

    /// 16-bit stereo PCM placed right after the sound written so far.
    fn write_pcm(&mut self, stream: u32, pcm: &[u8], frames: i64) -> windows::core::Result<()> {
        let time = self.audio_frames * TICKS / audio::RATE as i64;
        let duration = frames * TICKS / audio::RATE as i64;
        self.audio_frames += frames;
        self.write(stream, pcm, time, duration)
    }

    fn finish(self) -> windows::core::Result<()> {
        unsafe { self.writer.Finalize() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_white_and_black() {
        let (w, h) = (4, 2);
        let mut bgra = vec![255u8; w * h * 4];
        bgra[..8].fill(0); // two black pixels top-left
        let mut out = vec![0u8; w * h * 3 / 2];
        bgra_to_nv12(&bgra, w, h, &mut out);
        assert_eq!(out[0], 16, "black luma");
        assert_eq!(out[3], 235, "white luma");
        assert!(out[w * h..].iter().all(|&c| (126..=130).contains(&c)), "grey has neutral chroma");
    }

    #[test]
    fn area_is_even_and_bounded() {
        assert_eq!(fit_area([5, 7, 1001, 9999]), [5, 7, 1000, 4096]);
        // 4K fits as it is.
        assert_eq!(fit_area([0, 0, 3840, 2160]), [0, 0, 3840, 2160]);
        assert_eq!(fit_area([0, 0, 4096, 2304]), [0, 0, 4096, 2304]);
    }

    #[test]
    fn area_stays_within_h264_frame_size() {
        // 5K and 6K screens keep 16:9: landscape loses the bottom, portrait the right.
        assert_eq!(fit_area([0, 0, 5120, 2880]), [0, 0, 4096, 2304]);
        assert_eq!(fit_area([0, 0, 6016, 3384]), [0, 0, 4096, 2304]);
        assert_eq!(fit_area([0, 0, 2880, 5120]), [0, 0, 2304, 4096]);
        assert_eq!(fit_area([0, 0, 4096, 4000]), [0, 0, 4096, 2304]);
        assert_eq!(fit_area([0, 0, 4000, 2600]), [0, 0, 4000, 2352]);
        for [_, _, w, h] in [fit_area([0, 0, 4095, 4095]), fit_area([0, 0, 3001, 3999]), fit_area([0, 0, 4000, 2600])] {
            assert!(w % 2 == 0 && h % 2 == 0, "{w}x{h} must be even");
            assert!(((w + 15) / 16) * ((h + 15) / 16) <= MAX_MACROBLOCKS, "{w}x{h} is over the limit");
        }
    }

    #[test]
    fn area_is_trimmed_to_its_monitor() {
        let mon = [0, 0, 1920, 1080];
        // Inside: only the even trim.
        assert_eq!(on_monitor([101, 51, 301, 201], mon), [101, 51, 300, 200]);
        // Hanging off the right onto a second monitor: cut at the edge.
        assert_eq!(on_monitor([1700, 100, 600, 400], mon), [1700, 100, 220, 400]);
        // A monitor left of and above the primary has negative coordinates.
        assert_eq!(on_monitor([-300, -200, 500, 500], [-1280, -1024, 0, 0]), [-300, -200, 300, 200]);
    }

    #[test]
    fn lost_desktop_errors_are_retried_not_fatal() {
        for code in [E_ACCESSDENIED, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED] {
            assert!(reopen_is_transient(code), "{code:?} should be retried");
        }
        use windows::Win32::Foundation::{E_INVALIDARG, E_OUTOFMEMORY};
        use windows::Win32::Graphics::Dxgi::DXGI_ERROR_DEVICE_REMOVED;
        for code in [E_INVALIDARG, E_OUTOFMEMORY, DXGI_ERROR_DEVICE_REMOVED] {
            assert!(!reopen_is_transient(code), "{code:?} should end the recording");
        }
    }

    #[test]
    fn reopen_wait_doubles_up_to_a_cap() {
        let mut wait = REOPEN_FIRST;
        let mut seen = vec![wait];
        for _ in 0..6 {
            wait = next_reopen_wait(wait);
            seen.push(wait);
        }
        let ms: Vec<u128> = seen.iter().map(|d| d.as_millis()).collect();
        assert_eq!(ms, [100, 200, 400, 800, 1000, 1000, 1000]);
    }

    #[test]
    fn copy_box_stays_inside_the_desktop() {
        let b = |b: D3D11_BOX| [b.left, b.top, b.right, b.bottom];
        // Unchanged desktop: the whole area.
        assert_eq!(clip_box([100, 50, 200, 100], [1920, 1080]).map(b), Some([100, 50, 300, 150]));
        // Desktop shrank under the area: only the overlap.
        assert_eq!(clip_box([100, 50, 200, 100], [250, 120]).map(b), Some([100, 50, 250, 120]));
        // Desktop shrank past the area entirely: nothing to copy.
        assert_eq!(clip_box([1600, 900, 200, 100], [1280, 720]), None);
        assert_eq!(clip_box([100, 50, 200, 100], [0, 0]), None);
    }

    /// A 7×5 desktop with every pixel numbered, handed over unturned the way
    /// the Desktop Duplication sample describes for each rotation.
    fn unturned(rotation: DXGI_MODE_ROTATION) -> (Vec<u32>, [u32; 2]) {
        let (dw, dh) = (7usize, 5usize);
        let size = if matches!(rotation, DXGI_MODE_ROTATION_ROTATE90 | DXGI_MODE_ROTATION_ROTATE270) { [dh, dw] } else { [dw, dh] };
        let mut tex = vec![0u32; dw * dh];
        for ty in 0..size[1] {
            for tx in 0..size[0] {
                // The sample's texture → desktop mapping (SetDirtyVert).
                let (dx, dy) = match rotation {
                    DXGI_MODE_ROTATION_ROTATE90 => (dw - 1 - ty, tx),
                    DXGI_MODE_ROTATION_ROTATE180 => (dw - 1 - tx, dh - 1 - ty),
                    DXGI_MODE_ROTATION_ROTATE270 => (ty, dh - 1 - tx),
                    _ => (tx, ty),
                };
                tex[ty * size[0] + tx] = (dy * dw + dx) as u32;
            }
        }
        (tex, [size[0] as u32, size[1] as u32])
    }

    #[test]
    fn turned_monitors_record_upright() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
        let local = [2, 1, 4, 3]; // x, y, w, h on the 7×5 desktop as seen
        for rotation in [DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270] {
            let (tex, size) = unturned(rotation);
            let [rx, ry, rw, rh] = texture_rect(local, rotation, size);
            // What CopySubresourceRegion would put in the staging texture, with padded rows.
            let pitch = rw as usize * 4 + 8;
            let mut staging = vec![0u8; pitch * rh as usize];
            for sy in 0..rh as usize {
                for sx in 0..rw as usize {
                    let v = tex[(ry as usize + sy) * size[0] as usize + rx as usize + sx];
                    staging[sy * pitch + sx * 4..][..4].copy_from_slice(&v.to_le_bytes());
                }
            }
            let mut clean = vec![0u8; 4 * 3 * 4];
            unrotate(&staging, pitch, rotation, 4, 3, [0, 0, rw as usize, rh as usize], &mut clean);
            let got: Vec<u32> = clean.as_chunks::<4>().0.iter().map(|p| u32::from_le_bytes(*p)).collect();
            let want: Vec<u32> = (1..4).flat_map(|y| (2..6).map(move |x| y * 7 + x)).collect();
            assert_eq!(got, want, "{rotation:?}");
        }
    }

    /// Turned between portrait and landscape onto a desktop that covers only
    /// part of the area: the fresh staging texture is blank outside the
    /// copied part, and those pixels must keep the last image, not go black.
    #[test]
    fn partly_covered_area_keeps_the_last_image() {
        use windows::Win32::Graphics::Dxgi::Common::DXGI_MODE_ROTATION_IDENTITY;
        const LAST: u32 = 0xdead_beef;
        let local = [2, 1, 4, 3];
        for rotation in [DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270] {
            let (tex, size) = unturned(rotation);
            let [rx, ry, rw, rh] = texture_rect(local, rotation, size);
            // Only the left part of the staging texture got this frame; the rest is blank.
            let copied = [0, 0, rw as usize - 1, rh as usize];
            let pitch = rw as usize * 4;
            let mut staging = vec![0u8; pitch * rh as usize];
            for sy in 0..copied[3] {
                for sx in 0..copied[2] {
                    let v = tex[(ry as usize + sy) * size[0] as usize + rx as usize + sx];
                    staging[sy * pitch + sx * 4..][..4].copy_from_slice(&v.to_le_bytes());
                }
            }
            let mut clean: Vec<u8> = std::iter::repeat_n(LAST.to_le_bytes(), 4 * 3).flatten().collect();
            unrotate(&staging, pitch, rotation, 4, 3, copied, &mut clean);
            let got: Vec<u32> = clean.as_chunks::<4>().0.iter().map(|p| u32::from_le_bytes(*p)).collect();
            let want: Vec<u32> = (1..4).flat_map(|y| (2..6).map(move |x| y * 7 + x)).collect();
            let fresh = got.iter().zip(&want).filter(|(g, w)| g == w).count();
            assert!(got.iter().zip(&want).all(|(g, w)| g == w || *g == LAST), "{rotation:?}: {got:?}");
            assert_eq!(fresh, copied[2] * copied[3], "{rotation:?}: {got:?}");
        }
    }

    #[test]
    fn turned_area_maps_into_the_unturned_image() {
        // A 1080×1920 portrait monitor arrives as a 1920×1080 image.
        let local = [100, 200, 300, 400];
        assert_eq!(texture_rect(local, DXGI_MODE_ROTATION_ROTATE90, [1920, 1080]), [200, 1080 - 100 - 300, 400, 300]);
        assert_eq!(texture_rect(local, DXGI_MODE_ROTATION_ROTATE270, [1920, 1080]), [1920 - 200 - 400, 100, 400, 300]);
        assert_eq!(texture_rect(local, DXGI_MODE_ROTATION_ROTATE180, [1920, 1080]), [1920 - 100 - 300, 1080 - 200 - 400, 300, 400]);
        // Size isn't needed for the unturned cases, or to know the staging size.
        assert_eq!(texture_rect(local, DXGI_MODE_ROTATION(0), [0, 0]), local);
        assert_eq!(texture_rect(local, DXGI_MODE_ROTATION_ROTATE90, [0, 0])[2..], [400, 300]);
    }

    /// Needs a desktop. Takes the path a lost duplication takes and checks
    /// frames keep coming after re-opening.
    #[test]
    #[ignore]
    fn reopens_after_lost_duplication() {
        let _desktop = desktop();
        let (w, h) = (640, 480);
        let mut g = Grabber::new([0, 0, w, h]).expect("grabber");
        g.frame().expect("first frame");
        // Why the old duplication must go first: a second one is refused while it lives.
        assert!(unsafe { g.output.DuplicateOutput(&g.device) }.is_err());
        g.lost().expect("re-open");
        assert!(g.dup.is_some(), "duplication should be re-opened");
        for _ in 0..3 {
            assert_eq!(g.frame().expect("frame after re-open").len(), (w * h * 4) as usize);
        }
    }

    /// One desktop duplication per output per process: tests that open one
    /// take turns.
    fn desktop() -> std::sync::MutexGuard<'static, ()> {
        static DESKTOP: std::sync::Mutex<()> = std::sync::Mutex::new(());
        DESKTOP.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Needs a desktop. The first frame is a real screen image, from
    /// duplication or else GDI, and arrives within the bounded wait.
    #[test]
    #[ignore]
    fn first_frame_is_a_real_image() {
        let _desktop = desktop();
        let (w, h) = (640, 480);
        let mut g = Grabber::new([0, 0, w, h]).expect("grabber");
        g.copy_with_gdi().expect("GDI copy");
        assert!(g.have_frame);
        assert!(g.clean.as_chunks::<4>().0.iter().any(|p| p[..3] != [0, 0, 0]), "GDI copy of the desktop is all black");
        drop(g);
        let mut g = Grabber::new([0, 0, w, h]).expect("grabber");
        let t = Instant::now();
        assert_eq!(g.frame().expect("first frame").len(), (w * h * 4) as usize);
        assert!(g.have_frame, "first frame must hold a screen image");
        assert!(t.elapsed() < FIRST_FRAME_WAIT + Duration::from_millis(250), "first frame took {:?}", t.elapsed());
        assert!(g.clean.as_chunks::<4>().0.iter().any(|p| p[..3] != [0, 0, 0]), "first frame is all black");
    }

    /// Needs a desktop and sound devices. A 3 s recording of the real screen
    /// with system sound runs in real time and keeps close to 30 fps with
    /// the sink writer throttled.
    #[test]
    #[ignore]
    fn records_the_desktop_in_real_time() {
        let _desktop = desktop();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-realtime.mp4");
        let _ = std::fs::remove_file(&path);
        let settings = Settings { area: [0, 0, 1280, 720], system_sound: true, microphone: None, path: path.clone() };
        let (tx, rx) = std::sync::mpsc::channel();
        let mut rec = start(settings, move |e| {
            if let Event::Done(r) = e {
                let _ = tx.send(r);
            }
        });
        std::thread::sleep(Duration::from_secs(3));
        rec.stop();
        let done = rx.recv_timeout(Duration::from_secs(10)).expect("recording should finish").expect("recording");
        drop(rec);
        let (frames, span) = unsafe { count_video_frames(&done.path) }.expect("decode");
        std::fs::remove_file(&done.path).unwrap();
        assert!((2.9..4.0).contains(&done.seconds), "took {} s", done.seconds);
        let thumb = done.first_frame.expect("thumbnail");
        assert!(thumb.pixels.iter().any(|&c| c != Color32::BLACK), "thumbnail is all black");
        // The clock starts before the grabber and encoder are set up, so
        // measure the rate over the frames themselves.
        assert!(span > 1.5, "video spans only {span:.2} s");
        let fps = (frames - 1) as f64 / span;
        assert!(fps > 27.0, "{frames} frames over {span:.2} s ({fps:.1} fps)");
    }

    /// Writes a 2 s clip of moving bars with a tone, without touching the
    /// screen or sound devices.
    #[test]
    fn encodes_synthetic_mp4() {
        // Kept in the build folder so the clip can be inspected by hand.
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-synthetic.mp4");
        let (w, h) = (320u32, 240u32);
        let mut enc = Encoder::new(&path, w, h, true).expect("encoder");
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        for f in 0..(2 * FPS) {
            let bgra: Vec<u8> = (0..w * h)
                .flat_map(|i| {
                    // White band on top (to check which way is up), then
                    // blue and red halves sliding sideways.
                    if i / w < 24 {
                        return [255, 255, 255, 255];
                    }
                    let x = (i % w + f * 8) % w;
                    let v = if x < w / 2 { 230 } else { 30 };
                    [v, 80, 255 - v, 255]
                })
                .collect();
            bgra_to_nv12(&bgra, w as usize, h as usize, &mut nv12);
            enc.video(&nv12, f as i64 * TICKS / FPS as i64).unwrap();
            let n = audio::RATE as usize / FPS as usize;
            let tone: Vec<f32> = (0..n * 2)
                .map(|i| {
                    let t = (f as usize * n + i / 2) as f32 / audio::RATE as f32;
                    (t * 440.0 * std::f32::consts::TAU).sin() * 0.3
                })
                .collect();
            enc.audio(&tone).unwrap();
        }
        enc.finish().unwrap();
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(len > 10_000, "mp4 is only {len} bytes");

        // Decode the first frame back and check colours and orientation.
        let px = unsafe { first_frame_bgra(&path, h as usize) }.expect("decode");
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            (px[i + 2] as i32, px[i + 1] as i32, px[i] as i32)
        };
        let (r, g, b) = at(160, 8);
        assert!(r > 220 && g > 220 && b > 220, "top band should be white, got {:?}", (r, g, b));
        let (r, _, b) = at(60, 150);
        assert!(b > r + 100, "left half should be blue, got {:?}", at(60, 150));
        let (r, _, b) = at(260, 150);
        assert!(r > b + 100, "right half should be red, got {:?}", at(260, 150));
    }

    /// A microphone that hasn't sent its first packet yet leaves the sound
    /// track empty while video keeps coming. The throttled sink writer must
    /// not wait on that track, and the file must still finalize and play.
    #[test]
    fn video_without_audio_samples_does_not_stall() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-no-audio-yet.mp4");
        let (w, h) = (320u32, 240u32);
        let (tx, rx) = std::sync::mpsc::channel();
        let written = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let worker = {
            let (path, written) = (path.clone(), written.clone());
            std::thread::spawn(move || {
                let result = (|| {
                    let mut enc = Encoder::new(&path, w, h, true).map_err(|e| describe(&e))?;
                    let nv12 = vec![128u8; (w * h * 3 / 2) as usize];
                    for f in 0..(5 * FPS) as i64 {
                        enc.video(&nv12, f * TICKS / FPS as i64).map_err(|e| format!("frame {f}: {}", describe(&e)))?;
                        enc.audio(&[]).map_err(|e| format!("audio at frame {f}: {}", describe(&e)))?;
                        written.store(f + 1, Ordering::Relaxed);
                    }
                    // Silence trails the last picture by the gap...
                    let last = (5 * FPS as i64 - 1) * TICKS / FPS as i64;
                    if enc.audio_frames != (last - AUDIO_GAP) * audio::RATE as i64 / TICKS {
                        return Err(format!("sound track holds {} frames of silence", enc.audio_frames));
                    }
                    // ...and the late sound that arrives then is skipped as far as the silence covers it.
                    enc.audio(&vec![0.25; 5 * audio::RATE as usize * audio::CHANNELS]).map_err(|e| describe(&e))?;
                    if enc.audio_frames != 5 * audio::RATE as i64 {
                        return Err(format!("sound track holds {} frames after catching up", enc.audio_frames));
                    }
                    enc.finish().map_err(|e| format!("finalize: {}", describe(&e)))
                })();
                let _ = tx.send(result);
            })
        };
        let Ok(result) = rx.recv_timeout(Duration::from_secs(30)) else {
            panic!("encoder stalled with an empty sound track after {} of {} frames", written.load(Ordering::Relaxed), 5 * FPS);
        };
        worker.join().unwrap();
        result.expect("encode without sound samples");
        let (frames, span) = unsafe { count_video_frames(&path) }.expect("decode");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(frames, 5 * FPS, "spans {span:.2} s");
    }

    /// Stopped before any sound arrived: the empty sound track must not
    /// keep the file from finalizing.
    #[test]
    fn empty_sound_track_still_finalizes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-empty-audio.mp4");
        let (w, h) = (64u32, 64u32);
        let mut enc = Encoder::new(&path, w, h, true).expect("encoder");
        let nv12 = vec![128u8; (w * h * 3 / 2) as usize];
        for f in 0..3 {
            enc.video(&nv12, f * TICKS / FPS as i64).unwrap();
        }
        enc.finish().expect("finalize with no sound samples");
        let (frames, _) = unsafe { count_video_frames(&path) }.expect("decode");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(frames, 3);
    }

    /// A failure partway through must still leave a playable MP4 holding the
    /// frames written before it.
    #[test]
    fn error_midway_keeps_the_video() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-stopped-early.mp4");
        let (w, h) = (320u32, 240u32);
        let mut enc = Encoder::new(&path, w, h, false).expect("encoder");
        let bgra: Vec<u8> = (0..w * h).flat_map(|_| [230, 80, 25, 255]).collect();
        let mut nv12 = vec![0u8; (w * h * 3 / 2) as usize];
        bgra_to_nv12(&bgra, w as usize, h as usize, &mut nv12);
        let mut f = 0i64;
        let stop = AtomicBool::new(false);
        let reason = pump(&mut enc, &stop, |enc| {
            if f == 10 {
                return Err("couldn't read the screen: device removed".into());
            }
            enc.video(&nv12, f * TICKS / FPS as i64).map_err(|e| describe(&e))?;
            f += 1;
            Ok(true)
        });
        assert_eq!(reason, Ok(Some("couldn't read the screen: device removed".to_string())));
        enc.finish().expect("finalize after the error");
        let px = unsafe { first_frame_bgra(&path, h as usize) }.expect("decode");
        let i = ((120 * w + 160) * 4) as usize;
        let (r, b) = (px[i + 2] as i32, px[i] as i32);
        assert!(b > r + 100, "frame should be blue, got r={r} b={b}");
        std::fs::remove_file(&path).unwrap();
    }

    /// `start` claims the file name with an empty file before the encoder
    /// opens it; the sink writer must take that file over.
    #[test]
    fn encoder_fills_the_claimed_empty_file() {
        let want = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-claimed.mp4");
        let _ = std::fs::remove_file(&want);
        let (path, file) = crate::win::create_unique(&want).expect("claim");
        drop(file);
        assert_eq!(path, want);
        let (w, h) = (64u32, 64u32);
        let mut enc = Encoder::new(&path, w, h, false).expect("encoder over the claimed file");
        let nv12 = vec![128u8; (w * h * 3 / 2) as usize];
        for f in 0..5 {
            enc.video(&nv12, f * TICKS / FPS as i64).unwrap();
        }
        enc.finish().unwrap();
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        std::fs::remove_file(&path).unwrap();
        assert!(len > 500, "mp4 is only {len} bytes");
    }

    /// The largest frame `fit_area` lets through (a 5K screen) encodes and
    /// decodes back at full size.
    #[test]
    fn largest_area_encodes() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-largest.mp4");
        let [_, _, w, h] = fit_area([0, 0, 5120, 2880]);
        let (w, h) = (w as u32, h as u32);
        let mut enc = Encoder::new(&path, w, h, false).expect("encoder at the size limit");
        let nv12 = vec![100u8; (w * h * 3 / 2) as usize];
        for f in 0..3 {
            enc.video(&nv12, f * TICKS / FPS as i64).expect("frame at the size limit");
        }
        enc.finish().expect("finalize at the size limit");
        let px = unsafe { first_frame_bgra(&path, h as usize) }.expect("decode");
        std::fs::remove_file(&path).unwrap();
        assert_eq!(px.len(), (w * h * 4) as usize);
    }

    #[test]
    fn error_before_any_frame_is_a_failure() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("test-no-frames.mp4");
        let mut enc = Encoder::new(&path, 64, 64, false).expect("encoder");
        let stop = AtomicBool::new(false);
        assert_eq!(pump(&mut enc, &stop, |_| Err("boom".into())), Err("boom".to_string()));
        assert_eq!(pump(&mut enc, &AtomicBool::new(true), |_| Ok(true)), Err("nothing was recorded".to_string()));
        drop(enc);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn failed_finalize_keeps_the_early_stop_reason() {
        use windows::Win32::Foundation::E_FAIL;
        let e = windows::core::Error::from(E_FAIL);
        let text = finalize_failed(&e, Some("couldn't read the screen: device removed"));
        assert!(text.starts_with("the recording stopped early (couldn't read the screen: device removed), then couldn't write the video: "), "{text}");
        assert_eq!(finalize_failed(&e, None), format!("couldn't write the video: {}", describe(&e)));
    }

    #[test]
    fn blank_windows_errors_still_say_something() {
        assert_eq!(describe(&windows::core::Error::empty()), "unknown error");
    }

    #[test]
    fn grabber_setup_failures_say_what_went_wrong() {
        // Nowhere near any monitor: no device is needed to find that out.
        let err = Grabber::new([-200_000, -200_000, 100, 100]).err().expect("an area off every screen must be refused");
        assert!(matches!(err, GrabError::NoMonitor), "{err:?}");
        assert_eq!(err.to_string(), "the area isn't on any screen");
        for e in [GrabError::NoDevice, GrabError::NoBitmap, GrabError::Win(windows::core::Error::empty())] {
            let text = e.to_string();
            assert!(text.len() > 10 && !text.contains("completed successfully"), "{text}");
        }
    }

    /// Number of video frames and the seconds they span, first to last.
    unsafe fn count_video_frames(path: &Path) -> windows::core::Result<(u32, f64)> {
        let _mf = MediaFoundation::start()?;
        unsafe {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let (mut frames, mut first, mut last) = (0, None, 0i64);
            loop {
                let mut sample = None;
                let (mut flags, mut time) = (0u32, 0i64);
                reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
                if sample.is_some() {
                    frames += 1;
                    first.get_or_insert(time);
                    last = time;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    return Ok((frames, (last - first.unwrap_or(last)) as f64 / TICKS as f64));
                }
            }
        }
    }

    unsafe fn first_frame_bgra(path: &Path, height: usize) -> windows::core::Result<Vec<u8>> {
        let _mf = MediaFoundation::start()?;
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)?;
            let attrs = attrs.unwrap();
            attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attrs)?;
            let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
            let want = MFCreateMediaType()?;
            want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
            reader.SetCurrentMediaType(stream, None, &want)?;
            let got = reader.GetCurrentMediaType(stream)?;
            let stride = got.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| s as i32).unwrap_or(-1);
            let mut sample = None;
            let (mut actual, mut flags, mut time) = (0u32, 0u32, 0i64);
            reader.ReadSample(stream, 0, Some(&mut actual), Some(&mut flags), Some(&mut time), Some(&mut sample))?;
            let buffer = sample.unwrap().ConvertToContiguousBuffer()?;
            let (mut ptr, mut len) = (null_mut(), 0u32);
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let mut px = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;
            if stride < 0 {
                // Bottom-up rows: flip so row 0 is the top.
                let row = px.len() / height;
                px = px.chunks_exact(row).rev().flatten().copied().collect();
            }
            Ok(px)
        }
    }
}
