//! Thin Win32 layer: screen grab, window list, clipboard, save dialog, folders.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui::{self, Color32, ColorImage};
use windows_sys::Win32::Foundation::{GlobalFree, ERROR_FILE_NOT_FOUND, HWND, LPARAM, LRESULT, POINT, RECT, SYSTEMTIME, WPARAM};
use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmSetWindowAttribute, DWMWA_CLOAK, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    GetDC, GetDIBits, GetMonitorInfoW, ScreenToClient, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_DIB;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{TrackMouseEvent, TME_LEAVE, TME_NONCLIENT, TRACKMOUSEEVENT};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, FOLDERID_LocalAppData, FOLDERID_Pictures, FOLDERID_RoamingAppData, FOLDERID_Videos, SHGetKnownFolderPath};
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GetClientRect, GetWindowThreadProcessId, IsZoomed, PostMessageW, GWL_STYLE, HTCAPTION, HTCLIENT, HTMAXBUTTON, HTTOP, HTTOPLEFT, HTTOPRIGHT,
    MINMAXINFO, SC_MAXIMIZE, SC_RESTORE, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOOWNERZORDER, WM_DPICHANGED, WM_GETMINMAXINFO, WM_MOUSEMOVE, WM_NCCALCSIZE,
    WM_NCDESTROY, WM_NCHITTEST, WM_NCLBUTTONDBLCLK, WM_NCLBUTTONDOWN, WM_NCLBUTTONUP, WM_NCMOUSELEAVE, WM_NCMOUSEMOVE, WM_SYSCOMMAND, WS_THICKFRAME,
    GetCursorPos, GetWindowPlacement, WINDOWPLACEMENT, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, GetSystemMetrics, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, IsIconic, ShowWindow, SW_SHOWNOACTIVATE, IsWindowVisible, SetWindowPos, GWL_EXSTYLE, MessageBoxW, MB_ICONWARNING, MB_OK,

    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};

/// A frozen picture of every monitor plus the windows that were on it.
pub struct Grab {
    pub image: ColorImage,
    /// Virtual-screen position of the image's top-left pixel.
    pub origin: [i32; 2],
    /// Visible top-level windows, front to back, as [left, top, right, bottom]
    /// in image pixels.
    pub windows: Vec<[i32; 4]>,
}

pub fn grab_screen() -> Result<Grab, String> {
    let (x, y, w, h) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return Err(format!("screen size reported as {w}x{h}"));
    }
    let mut bgra = vec![0u8; w as usize * h as usize * 4];
    unsafe {
        let screen = GetDC(null_mut());
        if screen.is_null() {
            return Err("could not open the screen".into());
        }
        let mem = CreateCompatibleDC(screen);
        let bmp = CreateCompatibleBitmap(screen, w, h);
        let old = SelectObject(mem, bmp);
        let blt = BitBlt(mem, 0, 0, w, h, screen, x, y, SRCCOPY | CAPTUREBLT);
        let mut bi: BITMAPINFO = zeroed();
        bi.bmiHeader = bitmap_header(w, -h);
        let rows = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            bgra.as_mut_ptr() as *mut c_void,
            &mut bi,
            DIB_RGB_COLORS,
        );
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
        if blt == 0 || rows != h {
            return Err("the screen could not be copied".into());
        }
    }
    let pixels = bgra
        .as_chunks::<4>()
        .0
        .iter()
        .map(|p| Color32::from_rgb(p[2], p[1], p[0]))
        .collect();
    let image = ColorImage::new([w as usize, h as usize], pixels);
    let windows = list_windows()
        .into_iter()
        .map(|r| [r[0] - x, r[1] - y, r[2] - x, r[3] - y])
        .collect();
    Ok(Grab { image, origin: [x, y], windows })
}

fn bitmap_header(w: i32, h: i32) -> BITMAPINFOHEADER {
    BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: h,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: 0,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    }
}

fn list_windows() -> Vec<[i32; 4]> {
    unsafe extern "system" fn each(hwnd: HWND, lparam: LPARAM) -> i32 {
        let out = &mut *(lparam as *mut Vec<[i32; 4]>);
        if IsWindowVisible(hwnd) == 0 || IsIconic(hwnd) != 0 {
            return 1;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT != 0 {
            return 1;
        }
        // Untitled tool windows are mostly invisible overlays and shadows.
        if ex & WS_EX_TOOLWINDOW != 0 && GetWindowTextLengthW(hwnd) == 0 {
            return 1;
        }
        let mut cloaked: u32 = 0;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED as _,
            &mut cloaked as *mut u32 as *mut c_void,
            size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return 1;
        }
        let mut r: RECT = zeroed();
        // The extended frame excludes the invisible resize border around windows.
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS as _,
            &mut r as *mut RECT as *mut c_void,
            size_of::<RECT>() as u32,
        ) != 0
        {
            GetWindowRect(hwnd, &mut r);
        }
        if r.right - r.left >= 16 && r.bottom - r.top >= 16 {
            out.push([r.left, r.top, r.right, r.bottom]);
        }
        1
    }
    let mut out: Vec<[i32; 4]> = Vec::new();
    unsafe {
        EnumWindows(Some(each), &mut out as *mut _ as LPARAM);
    }
    out
}

/// Moves and sizes the window so its drawing area covers exactly the given
/// screen pixels. Returns true once it already does. Needs repeating after
/// Windows rescales a window that lands on a monitor with other scaling.
pub fn fit_client(hwnd: HWND, x: i32, y: i32, w: i32, h: i32) -> bool {
    unsafe {
        let mut client: RECT = zeroed();
        let mut at = POINT { x: 0, y: 0 };
        let mut outer: RECT = zeroed();
        if GetClientRect(hwnd, &mut client) == 0
            || ClientToScreen(hwnd, &mut at) == 0
            || GetWindowRect(hwnd, &mut outer) == 0
        {
            return true;
        }
        if (at.x, at.y, client.right, client.bottom) == (x, y, w, h) {
            return true;
        }
        SetWindowPos(
            hwnd,
            null_mut(),
            outer.left + x - at.x,
            outer.top + y - at.y,
            outer.right - outer.left + w - client.right,
            outer.bottom - outer.top + h - client.bottom,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        false
    }
}

/// Window's top-left corner in screen pixels. A minimized window reports
/// -32000,-32000 from `GetWindowRect`, so its restored position is used instead.
pub fn window_corner(hwnd: HWND) -> Option<[i32; 2]> {
    unsafe {
        if IsIconic(hwnd) == 0 {
            let mut r: RECT = zeroed();
            return (GetWindowRect(hwnd, &mut r) != 0).then_some([r.left, r.top]);
        }
        let mut wp: WINDOWPLACEMENT = zeroed();
        wp.length = size_of::<WINDOWPLACEMENT>() as u32;
        if GetWindowPlacement(hwnd, &mut wp) == 0 {
            return None;
        }
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        if GetMonitorInfoW(primary, &mut mi) == 0 {
            return None;
        }
        let r = wp.rcNormalPosition;
        Some(workspace_to_screen([r.left, r.top], [mi.rcWork.left - mi.rcMonitor.left, mi.rcWork.top - mi.rcMonitor.top]))
    }
}

/// Un-minimizes the window without activating it, so a hotkey snip from another
/// app can show its countdown without taking focus.
pub fn restore_inactive(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        }
    }
}

/// `rcNormalPosition` is in workspace coordinates: relative to the primary
/// monitor's work area, which the taskbar can push away from the screen origin.
fn workspace_to_screen(corner: [i32; 2], work_offset: [i32; 2]) -> [i32; 2] {
    [corner[0] + work_offset[0], corner[1] + work_offset[1]]
}

/// Puts the window's top-left at `corner`, then pulls it fully onto the
/// work area of whichever monitor it ends up on.
pub fn place_on_screen(hwnd: HWND, corner: Option<[i32; 2]>) {
    unsafe {
        if let Some([x, y]) = corner {
            SetWindowPos(hwnd, null_mut(), x, y, 0, 0, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE);
        }
        let mut r: RECT = zeroed();
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if GetWindowRect(hwnd, &mut r) == 0 || GetMonitorInfoW(mon, &mut mi) == 0 {
            return;
        }
        let work = mi.rcWork;
        let w = (r.right - r.left).min(work.right - work.left);
        let h = (r.bottom - r.top).min(work.bottom - work.top);
        let x = r.left.clamp(work.left, work.right - w);
        let y = r.top.clamp(work.top, work.bottom - h);
        if (x, y, w, h) != (r.left, r.top, r.right - r.left, r.bottom - r.top) {
            SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
}

/// Puts the picture on the clipboard as a bitmap (for every app) and as PNG
/// (keeps transparency for apps that read it).
pub fn copy_image(owner: HWND, img: &ColorImage) -> Result<(), String> {
    copy_image_with_png(owner, img, &encode_png(img)?)
}

/// `copy_image` with the PNG already encoded, for a caller that also saves it.
pub fn copy_image_with_png(owner: HWND, img: &ColorImage, png: &[u8]) -> Result<(), String> {
    unsafe {
        open_clipboard(owner)?;
        EmptyClipboard();
        let result = set_dib(img).and_then(|_| set_clipboard(png_format(), png));
        CloseClipboard();
        result
    }
}

/// The first half of a copy that can't wait for the PNG: the picture goes on the clipboard as a
/// bitmap only, which every app pastes. Returns the clipboard's sequence number for `add_png`.
pub fn copy_bitmap(owner: HWND, img: &ColorImage) -> Result<u32, String> {
    unsafe {
        open_clipboard(owner)?;
        EmptyClipboard();
        let result = set_dib(img);
        CloseClipboard();
        // Windows counts the change when the clipboard is closed.
        result.map(|_| GetClipboardSequenceNumber())
    }
}

/// The second half: adds the PNG (which keeps transparency) to the bitmap `copy_bitmap` put on the
/// clipboard. Leaves the clipboard alone, and is done, when something else was copied since: the
/// sequence number moved on, or (should another copy have slipped in just before `copy_bitmap` read
/// it) the clipboard has another owner.
pub fn add_png(owner: HWND, seq: u32, png: &[u8]) -> Result<(), String> {
    use windows_sys::Win32::System::DataExchange::GetClipboardOwner;
    let ours = || unsafe { GetClipboardSequenceNumber() == seq && GetClipboardOwner() == owner };
    unsafe {
        if !ours() {
            return Ok(());
        }
        open_clipboard(owner)?;
        // Checked again with the clipboard held, so nothing can be copied in between.
        let result = if ours() { set_clipboard(png_format(), png) } else { Ok(()) };
        CloseClipboard();
        result
    }
}

/// Puts the picture on the open clipboard as a bottom-up 32-bit DIB, converted straight into the
/// clipboard's memory (no second 33 MB copy for a 4K snip).
unsafe fn set_dib(img: &ColorImage) -> Result<(), String> {
    let [w, h] = img.size;
    set_clipboard_with(CF_DIB as u32, size_of::<BITMAPINFOHEADER>() + w * h * 4, |out| write_dib(img, out))
}

/// `img` as a DIB into `out` (header, then bottom-up rows); transparent areas become white so they
/// don't paste as black.
fn write_dib(img: &ColorImage, out: &mut [u8]) {
    let [w, h] = img.size;
    let header = bitmap_header(w as i32, h as i32);
    let (head, rows) = out.split_at_mut(size_of::<BITMAPINFOHEADER>());
    head.copy_from_slice(unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, size_of::<BITMAPINFOHEADER>()) });
    if w == 0 {
        return;
    }
    for (src, dst) in img.pixels.chunks_exact(w).rev().zip(rows.chunks_exact_mut(w * 4)) {
        for (c, d) in src.iter().zip(dst.as_chunks_mut::<4>().0) {
            let [r, g, b, a] = c.to_array();
            // Opaque (nearly every pixel of a snip): premultiplied is the colour itself.
            let px = if a == 255 {
                [b, g, r, 255]
            } else {
                let [r, g, b, a] = c.to_srgba_unmultiplied();
                let mix = |v: u8| ((v as u32 * a as u32 + 255 * (255 - a as u32)) / 255) as u8;
                [mix(b), mix(g), mix(r), 255]
            };
            *d = px;
        }
    }
}

fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(wide("PNG").as_ptr()) }
}

/// Opens the clipboard for `owner`, retrying for a moment while another program has it open.
unsafe fn open_clipboard(owner: HWND) -> Result<(), String> {
    for _ in 0..10 {
        if OpenClipboard(owner) != 0 {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    Err("the clipboard is busy".into())
}

unsafe fn set_clipboard(format: u32, bytes: &[u8]) -> Result<(), String> {
    set_clipboard_with(format, bytes.len(), |dst| dst.copy_from_slice(bytes))
}

/// Puts `len` bytes, written by `fill`, on the open clipboard as `format`.
unsafe fn set_clipboard_with(format: u32, len: usize, fill: impl FnOnce(&mut [u8])) -> Result<(), String> {
    let mem = GlobalAlloc(GMEM_MOVEABLE, len);
    if mem.is_null() {
        return Err("out of memory for the clipboard".into());
    }
    let dst = GlobalLock(mem) as *mut u8;
    if dst.is_null() {
        GlobalFree(mem);
        return Err("couldn't lock clipboard memory".into());
    }
    fill(std::slice::from_raw_parts_mut(dst, len));
    GlobalUnlock(mem);
    if SetClipboardData(format, mem).is_null() {
        GlobalFree(mem);
        return Err("the clipboard refused the picture".into());
    }
    Ok(())
}

pub fn encode_png(img: &ColorImage) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    let rgba: Vec<u8> = img
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    writer.write_image_data(&rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn save_png(path: &std::path::Path, img: &ColorImage) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, encode_png(img)?).map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `bytes` into a new file at `path`, or at the first free "name (2).png", "name (3).png"…
/// beside it, and returns where it went. Never overwrites.
pub fn save_new(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let (path, mut file) = create_unique(path)?;
    if let Err(e) = file.write_all(bytes) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("{}: {e}", path.display()));
    }
    Ok(path)
}

/// Creates `path` (and its folder) as a new empty file, or the first free
/// numbered name beside it the way Explorer numbers copies. Create-new
/// semantics mean two snips or recordings made in the same second can never
/// land on, or later delete, each other's file.
pub fn create_unique(path: &Path) -> Result<(PathBuf, std::fs::File), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for n in 1..=1000 {
        let candidate = numbered(path, n);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{}: {e}", candidate.display())),
        }
    }
    Err(format!("{}: too many files with this name", path.display()))
}

/// `path` itself for `n` = 1, else "stem (n).ext".
fn numbered(path: &Path, n: u32) -> PathBuf {
    if n <= 1 {
        return path.to_path_buf();
    }
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(format!(" ({n})"));
    if let Some(ext) = path.extension() {
        name.push(".");
        name.push(ext);
    }
    path.with_file_name(name)
}

pub fn pictures_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Pictures)
}

pub fn videos_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_Videos)
}

/// `%APPDATA%`: settings that follow the person between PCs.
pub fn roaming_app_data_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_RoamingAppData)
}

/// `%LOCALAPPDATA%`: this PC's caches (notification pictures, the icon file).
pub fn local_app_data_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData)
}

fn known_folder(id: &windows_sys::core::GUID) -> Option<PathBuf> {
    unsafe {
        let mut raw: *mut u16 = null_mut();
        let hr = SHGetKnownFolderPath(id, 0, null_mut(), &mut raw);
        let path = if hr == 0 && !raw.is_null() {
            let len = (0..).take_while(|&i| *raw.add(i) != 0).count();
            Some(PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(raw, len))))
        } else {
            None
        };
        CoTaskMemFree(raw as *const c_void);
        path
    }
}

/// "2026-11-05 141530" in local time, for file names.
pub fn timestamp() -> String {
    let mut t: SYSTEMTIME = unsafe { zeroed() };
    unsafe { GetLocalTime(&mut t) };
    format!(
        "{:04}-{:02}-{:02} {:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Native "Save as" dialog, modal to `owner`. `Ok(None)` when the person cancels; an error when
/// the dialog failed (a path longer than it can hand back, say).
/// `kind` is (description, extension), e.g. ("PNG image", "png"). Runs on any thread: the dialog's
/// shell views need a single-threaded COM apartment, which a worker thread gets here for the call.
/// `owner` may belong to another thread; it is disabled while the dialog is up.
pub fn ask_save_path(
    owner: HWND,
    start_dir: Option<&std::path::Path>,
    name: &str,
    (description, extension): (&str, &str),
) -> Result<Option<PathBuf>, String> {
    use windows_sys::Win32::UI::Controls::Dialogs::{CommDlgExtendedError, FNERR_BUFFERTOOSMALL};
    let mut buf = [0u16; 1024];
    let initial = wide(name);
    buf[..initial.len()].copy_from_slice(&initial);
    let filter: Vec<u16> =
        format!("{description} (*.{extension})\0*.{extension}\0\0").encode_utf16().collect();
    let ext = wide(extension);
    let dir = start_dir.map(|d| wide(&d.to_string_lossy()));
    let mut ofn: OPENFILENAMEW = unsafe { zeroed() };
    ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrDefExt = ext.as_ptr();
    ofn.lpstrInitialDir = dir.as_ref().map_or(null(), |d| d.as_ptr());
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;
    // S_OK or S_FALSE: this call joined (or re-entered) an apartment and must leave it; a thread already
    // in another kind of apartment (RPC_E_CHANGED_MODE) is left as it was.
    let com = unsafe { CoInitializeEx(null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32) } >= 0;
    let chosen = unsafe { GetSaveFileNameW(&mut ofn) } != 0;
    // Read before anything else can call into the dialog library on this thread.
    let failure = if chosen { 0 } else { unsafe { CommDlgExtendedError() } };
    if com {
        unsafe { CoUninitialize() };
    }
    match failure {
        _ if chosen => {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok(Some(PathBuf::from(String::from_utf16_lossy(&buf[..len]))))
        }
        0 => Ok(None),
        FNERR_BUFFERTOOSMALL => Err(format!("the path is longer than the {} characters Save can take", buf.len() - 1)),
        code => Err(format!("the Save dialog failed (error {code:#x})")),
    }
}

#[link(name = "user32")]
extern "system" {
    fn SetProcessDpiAwarenessContext(value: isize) -> i32;
}

#[link(name = "imm32")]
extern "system" {
    fn ImmGetContext(hwnd: HWND) -> isize;
    fn ImmNotifyIME(himc: isize, action: u32, index: u32, value: u32) -> i32;
    fn ImmReleaseContext(hwnd: HWND, himc: isize) -> i32;
}

/// Throws away what the window's input method is composing, without committing it: the app has
/// already taken it in as typed. Call on the window's own thread.
pub fn cancel_ime_composition(hwnd: HWND) {
    const NI_COMPOSITIONSTR: u32 = 0x15;
    const CPS_CANCEL: u32 = 0x4;
    unsafe {
        let himc = ImmGetContext(hwnd);
        if himc != 0 {
            ImmNotifyIME(himc, NI_COMPOSITIONSTR, CPS_CANCEL, 0);
            ImmReleaseContext(hwnd, himc);
        }
    }
}

/// Per-monitor DPI awareness (v2), as winit sets it when the window is made; done first so the tray
/// icon, loaded before any window exists, comes out at the screen's real size instead of a blurry 16 px.
pub fn become_dpi_aware() {
    const PER_MONITOR_AWARE_V2: isize = -4;
    unsafe { SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2) };
}

/// Cloaks or uncloaks a window: a cloaked window may be shown, but DWM draws none of it and clicks
/// go through it. The taskbar keeps its button (see `set_taskbar_button`).
pub fn cloak(hwnd: HWND, on: bool) {
    let value: i32 = on.into();
    unsafe { DwmSetWindowAttribute(hwnd, DWMWA_CLOAK as _, &value as *const i32 as *const c_void, size_of::<i32>() as u32) };
}

/// Whether DWM is keeping the window off the screen (our cloak, or another virtual desktop's).
fn is_cloaked(hwnd: HWND) -> bool {
    let mut value = 0u32;
    let ok = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as _, &mut value as *mut u32 as *mut c_void, size_of::<u32>() as u32) } == 0;
    ok && value != 0
}

/// Blocks until DWM has composed the screen twice, so a window cloaked just before is gone from
/// what a screen copy sees.
pub fn wait_for_composition() {
    use windows_sys::Win32::Graphics::Dwm::DwmFlush;
    for _ in 0..2 {
        if unsafe { DwmFlush() } != 0 {
            // Composition is off or failed; give it a frame's worth of time instead.
            std::thread::sleep(std::time::Duration::from_millis(17));
        }
    }
}

/// `ITaskbarList`'s vtable: `IUnknown`, then `HrInit`, `AddTab`, `DeleteTab`, …
#[repr(C)]
struct TaskbarListVtbl {
    query_interface: usize,
    add_ref: usize,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    hr_init: unsafe extern "system" fn(*mut c_void) -> i32,
    add_tab: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
    delete_tab: unsafe extern "system" fn(*mut c_void, HWND) -> i32,
}

thread_local! {
    /// The shell's taskbar list, made once per thread; null when it couldn't be (no Explorer).
    static TASKBAR: std::cell::Cell<*mut *const TaskbarListVtbl> = const { std::cell::Cell::new(null_mut()) };
}

/// Shows or removes the window's taskbar button without touching its styles (the shell keeps a
/// cloaked window's button). Explorer forgets a removal when it restarts; call again after
/// `TaskbarCreated`. Call on the window's own thread, which has COM up (winit's UI thread does).
pub fn set_taskbar_button(hwnd: HWND, on: bool) {
    use windows_sys::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows_sys::Win32::UI::Shell::TaskbarList;
    const IID_ITASKBARLIST: windows_sys::core::GUID = windows_sys::core::GUID::from_u128(0x56fdf342_fd6d_11d0_958a_006097c9a090);
    TASKBAR.with(|cell| unsafe {
        if cell.get().is_null() {
            let mut list: *mut *const TaskbarListVtbl = null_mut();
            if CoCreateInstance(&TaskbarList, null_mut(), CLSCTX_INPROC_SERVER, &IID_ITASKBARLIST, &mut list as *mut _ as *mut *mut c_void) != 0 || list.is_null() {
                return;
            }
            if ((**list).hr_init)(list as *mut c_void) != 0 {
                ((**list).release)(list as *mut c_void);
                return;
            }
            cell.set(list);
        }
        let list = cell.get();
        if on {
            ((**list).add_tab)(list as *mut c_void, hwnd);
        } else {
            ((**list).delete_tab)(list as *mut c_void, hwnd);
        }
    });
}

/// Lets go of this thread's taskbar list after Explorer restarted: the old one talks to the taskbar
/// that is gone, so the next `set_taskbar_button` makes a new one.
pub fn forget_taskbar_list() {
    TASKBAR.with(|cell| {
        let list = cell.replace(null_mut());
        if !list.is_null() {
            unsafe { ((**list).release)(list as *mut c_void) };
        }
    });
}

pub fn is_visible(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd) != 0 }
}

pub fn is_foreground(hwnd: HWND) -> bool {
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow() == hwnd }
}

/// Passes the foreground from `hwnd`, which is shown but cloaked, to the window below it that a
/// person can see and use, the way Windows does when the foreground window is hidden; the
/// desktop when there is none. Hiding would do this for us, but it would also stop eframe
/// repainting the window promptly.
pub fn activate_next(hwnd: HWND) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetShellWindow, GetWindow, SetForegroundWindow, GW_HWNDNEXT, GW_OWNER, WS_EX_NOACTIVATE};
    unsafe {
        let usable = |w: HWND| {
            let ex = GetWindowLongW(w, GWL_EXSTYLE) as u32;
            let mut r: RECT = zeroed();
            IsWindowVisible(w) != 0
                && IsWindowEnabled(w) != 0
                && IsIconic(w) == 0
                && !is_cloaked(w)
                && ex & (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW) == 0
                && GetWindow(w, GW_OWNER).is_null()
                && GetWindowRect(w, &mut r) != 0
                && r.right > r.left
                && r.bottom > r.top
        };
        let mut next = GetWindow(hwnd, GW_HWNDNEXT);
        while !next.is_null() && !usable(next) {
            next = GetWindow(next, GW_HWNDNEXT);
        }
        SetForegroundWindow(if next.is_null() { GetShellWindow() } else { next });
    }
}

/// A string value under `HKEY_CURRENT_USER\subkey`; `None` when it (or the key) doesn't exist.
/// `name` "" is the key's default value.
pub fn reg_get(subkey: &str, name: &str) -> Option<String> {
    let (key, value) = (wide(subkey), wide(name));
    let mut buf = [0u16; 2048];
    let mut bytes = (buf.len() * 2) as u32;
    let err = unsafe { RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, null_mut(), buf.as_mut_ptr() as *mut c_void, &mut bytes) };
    if err != 0 {
        return None;
    }
    let len = (bytes as usize / 2).min(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]).trim_end_matches('\0').to_string())
}

/// Writes a string value under `HKEY_CURRENT_USER\subkey`, creating the key if needed.
pub fn reg_set(subkey: &str, name: &str, data: &str) -> Result<(), String> {
    let (key, value, data) = (wide(subkey), wide(name), wide(data));
    let err = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), REG_SZ, data.as_ptr() as *const c_void, (data.len() * 2) as u32) };
    if err == 0 { Ok(()) } else { Err(format!("couldn't write HKCU\\{subkey}\\{name} (error {err})")) }
}

/// Removes a value under `HKEY_CURRENT_USER\subkey`; one that is already gone is fine.
pub fn reg_delete_value(subkey: &str, name: &str) -> Result<(), String> {
    let (key, value) = (wide(subkey), wide(name));
    match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr()) } {
        0 | ERROR_FILE_NOT_FOUND => Ok(()),
        err => Err(format!("couldn't remove HKCU\\{subkey}\\{name} (error {err})")),
    }
}

/// The running executable's full path.
pub fn exe_path() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|e| format!("couldn't find SnipScreen's own file: {e}"))
}

/// A message box for the one failure that has no window to show it in: a second launch that
/// couldn't reach the running copy.
pub fn error_box(msg: &str) {
    let (text, title) = (wide(msg), wide("SnipScreen"));
    unsafe { MessageBoxW(null_mut(), text.as_ptr(), title.as_ptr(), MB_ICONWARNING | MB_OK) };
}

/// Keeps a window out of screenshots and recordings (Windows 10 2004+).
pub fn exclude_from_capture(hwnd: HWND, exclude: bool) {
    unsafe {
        SetWindowDisplayAffinity(hwnd, if exclude { WDA_EXCLUDEFROMCAPTURE } else { WDA_NONE });
    }
}

/// The monitor under the mouse: x, y, w, h in screen pixels.
pub fn monitor_at_cursor() -> [i32; 4] {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    monitor_rect(p.x, p.y, false)
}

/// Work area (monitor minus taskbar) of the monitor holding the point.
pub fn work_area_at(x: i32, y: i32) -> [i32; 4] {
    monitor_rect(x, y, true)
}

fn monitor_rect(x: i32, y: i32, work: bool) -> [i32; 4] {
    unsafe {
        let mon = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = zeroed();
        mi.cbSize = size_of::<MONITORINFO>() as u32;
        GetMonitorInfoW(mon, &mut mi);
        let r = if work { mi.rcWork } else { mi.rcMonitor };
        [r.left, r.top, r.right - r.left, r.bottom - r.top]
    }
}

/// Opens a file with its default app.
pub fn open_path(path: &std::path::Path) -> Result<(), String> {
    std::process::Command::new("explorer").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// Opens Explorer with the file selected.
pub fn show_in_folder(path: &std::path::Path) -> Result<(), String> {
    let mut arg = std::ffi::OsString::from("/select,");
    arg.push(path);
    std::process::Command::new("explorer").arg(arg).spawn().map(|_| ()).map_err(|e| e.to_string())
}

/// A top-level window of ours by its exact title. Another program's window with the same title is
/// skipped: callers subclass, move and capture-exclude what this returns.
pub fn find_window(title: &str) -> Option<HWND> {
    let title = wide(title);
    let mut after: HWND = null_mut();
    loop {
        after = unsafe { FindWindowExW(null_mut(), after, null(), title.as_ptr()) };
        if after.is_null() {
            return None;
        }
        let mut pid = 0;
        unsafe { GetWindowThreadProcessId(after, &mut pid) };
        if pid == std::process::id() {
            return Some(after);
        }
    }
}

/// Our own title bar. A window it is attached to keeps its native frame (DWM shadow, resize
/// borders, Aero snap, the minimise and maximise animations) but loses the caption: its client
/// area runs up to the top edge, where egui draws the toolbar and the caption buttons. Each frame
/// the toolbar `publish`es where its controls are; `WM_NCHITTEST` then answers HTCAPTION over the
/// empty rest of the bar (drag, double-click, system menu), HTTOP along the top edge, and
/// HTMAXBUTTON over our maximise button, which is what brings up Windows 11's snap layouts. Mouse
/// input over that button goes to the frame instead of egui, so the subclass tracks its hover and
/// press itself (`max_state`) and wakes the window to repaint.
///
/// winit sizes windows with `AdjustWindowRectEx`, which still counts the caption, so the subclass
/// takes the caption back off wherever winit sizes the window: when attaching, in
/// `WM_GETMINMAXINFO` and after `WM_DPICHANGED`. A `ViewportCommand::InnerSize` for an attached
/// window goes through `inner_size_for`; `fit_client` and `size_client` measure the real client area.
#[derive(Clone, Default)]
pub struct TitleBar(Arc<Mutex<BarState>>);

#[derive(Default)]
struct BarState {
    hwnd: usize,
    /// Off while the shell's window is the picker or the recording bar: frameless, winit lays it out.
    active: bool,
    /// Client pixels, [left, top, right, bottom].
    bar: [i32; 4],
    /// Controls inside `bar` that egui handles.
    controls: Vec<[i32; 4]>,
    max_button: Option<[i32; 4]>,
    max_hot: bool,
    max_down: bool,
    /// Thickness of the sizing frame, which Windows also puts above a maximised window's client area.
    frame: i32,
    /// Client height gained by dropping the caption, at the window's current DPI.
    gained: i32,
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

const SUBCLASS_ID: usize = 0x5ee5;

impl TitleBar {
    /// Takes over the caption of `hwnd`, a window on this thread, keeping its client area the size
    /// winit made it. `wake` asks the window to repaint (the maximise button's hover changed).
    pub fn attach(&self, hwnd: HWND, wake: impl Fn() + Send + Sync + 'static) -> Result<(), String> {
        {
            let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if s.hwnd != 0 {
                return Ok(());
            }
            s.hwnd = hwnd as usize;
            s.active = true;
            s.wake = Some(Arc::new(wake));
        }
        let data = Arc::into_raw(self.0.clone()) as usize;
        if unsafe { SetWindowSubclass(hwnd, Some(bar_proc), SUBCLASS_ID, data) } == 0 {
            unsafe { drop(Arc::from_raw(data as *const Mutex<BarState>)) };
            self.0.lock().unwrap_or_else(PoisonError::into_inner).hwnd = 0;
            return Err("couldn't take over the window's title bar".into());
        }
        let before = client_size(hwnd);
        reframe(hwnd);
        if let (Some([_, h0]), Some([w, h1])) = (before, client_size(hwnd)) {
            if unsafe { IsZoomed(hwnd) } == 0 && h1 > h0 {
                size_client(hwnd, w, h0);
            }
        }
        Ok(())
    }

    /// Hands the frame back to winit (`false`, for the frameless picker and recording bar) or takes it again.
    pub fn set_active(&self, on: bool) {
        let hwnd = {
            let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if s.active == on || s.hwnd == 0 {
                return;
            }
            s.active = on;
            s.hwnd as HWND
        };
        reframe(hwnd);
    }

    /// Where the bar and its controls are this frame, in points; `max_button` is the maximise
    /// button, `None` on a window that can't be maximised. With `drag` false (a menu is open) the
    /// empty bar and the maximise button are left to egui, so a click there closes the menu
    /// instead of moving or maximising the window.
    pub fn publish(&self, ppp: f32, bar: egui::Rect, controls: &[egui::Rect], max_button: Option<egui::Rect>, drag: bool) {
        let px = |r: egui::Rect| [(r.min.x * ppp).round() as i32, (r.min.y * ppp).round() as i32, (r.max.x * ppp).round() as i32, (r.max.y * ppp).round() as i32];
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        s.bar = if drag { px(bar) } else { [0; 4] };
        s.controls.clear();
        s.controls.extend(controls.iter().map(|&r| px(r)));
        s.max_button = if drag { max_button.map(px) } else { None };
    }

    /// The size to put in `ViewportCommand::InnerSize` for a client area of `want` points: winit
    /// adds a caption this window doesn't have.
    pub fn inner_size_for(&self, want: egui::Vec2, ppp: f32) -> egui::Vec2 {
        let s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if s.hwnd == 0 {
            return want;
        }
        want - egui::vec2(0.0, s.gained as f32 / ppp)
    }

    /// The maximise button's (hovered, pressed), which only the subclass sees once attached.
    pub fn max_state(&self) -> (bool, bool) {
        let s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (s.max_hot, s.max_down)
    }
}

fn contains(r: &[i32; 4], x: i32, y: i32) -> bool {
    x >= r[0] && x < r[2] && y >= r[1] && y < r[3]
}

/// Hit-test inside the client area of an attached window, in client pixels.
fn bar_hit(s: &BarState, x: i32, y: i32, width: i32, zoomed: bool, resizable: bool) -> u32 {
    if !zoomed && resizable && y < s.frame {
        return if x < s.frame * 2 {
            HTTOPLEFT
        } else if x >= width - s.frame * 2 {
            HTTOPRIGHT
        } else {
            HTTOP
        };
    }
    if s.max_button.is_some_and(|r| contains(&r, x, y)) {
        return HTMAXBUTTON;
    }
    if contains(&s.bar, x, y) && !s.controls.iter().any(|r| contains(r, x, y)) {
        return HTCAPTION;
    }
    HTCLIENT
}

/// A maximised window with no caption covers its whole monitor, which Windows takes for full
/// screen: an auto-hide taskbar on that monitor then never slides out. Leaving it a sliver on that
/// edge keeps it working, as Windows Terminal does.
fn leave_room_for_autohide_taskbars(hwnd: HWND, client: &mut RECT) {
    use windows_sys::Win32::UI::Shell::{SHAppBarMessage, ABM_GETAUTOHIDEBAREX, ABM_GETSTATE, ABS_AUTOHIDE, APPBARDATA};
    let mut data = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..unsafe { zeroed() } };
    // The setting is shared by every taskbar; off (the usual case), nothing more to ask.
    if unsafe { SHAppBarMessage(ABM_GETSTATE, &mut data) } as u32 & ABS_AUTOHIDE == 0 {
        return;
    }
    let mut mi: MONITORINFO = unsafe { zeroed() };
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST), &mut mi) } == 0 {
        return;
    }
    autohide_inset(client, |edge| {
        let mut data = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, uEdge: edge, rc: mi.rcMonitor, ..unsafe { zeroed() } };
        unsafe { SHAppBarMessage(ABM_GETAUTOHIDEBAREX, &mut data) != 0 }
    });
}

/// Pulls `client` in by `AUTOHIDE_SLIVER` on each edge (`ABE_*`) where `has_bar` finds an auto-hide taskbar.
fn autohide_inset(client: &mut RECT, has_bar: impl Fn(u32) -> bool) {
    use windows_sys::Win32::UI::Shell::{ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP};
    const AUTOHIDE_SLIVER: i32 = 2;
    if has_bar(ABE_TOP) {
        client.top += AUTOHIDE_SLIVER;
    }
    if has_bar(ABE_BOTTOM) {
        client.bottom -= AUTOHIDE_SLIVER;
    }
    if has_bar(ABE_LEFT) {
        client.left += AUTOHIDE_SLIVER;
    }
    if has_bar(ABE_RIGHT) {
        client.right -= AUTOHIDE_SLIVER;
    }
}

/// Makes Windows ask for the frame again (`WM_NCCALCSIZE`) without moving, sizing or activating the window.
fn reframe(hwnd: HWND) {
    unsafe { SetWindowPos(hwnd, null_mut(), 0, 0, 0, 0, SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE) };
}

fn client_size(hwnd: HWND) -> Option<[i32; 2]> {
    let mut r: RECT = unsafe { zeroed() };
    (unsafe { GetClientRect(hwnd, &mut r) } != 0).then_some([r.right, r.bottom])
}

/// Sizes the window so its client area is `w`x`h` pixels, keeping its top-left corner.
pub fn size_client(hwnd: HWND, w: i32, h: i32) {
    let (Some([cw, ch]), mut outer) = (client_size(hwnd), unsafe { zeroed::<RECT>() }) else { return };
    if (cw, ch) == (w, h) || unsafe { GetWindowRect(hwnd, &mut outer) } == 0 {
        return;
    }
    let (ow, oh) = (outer.right - outer.left + w - cw, outer.bottom - outer.top + h - ch);
    unsafe { SetWindowPos(hwnd, null_mut(), 0, 0, ow, oh, SWP_NOMOVE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_NOACTIVATE) };
}

unsafe extern "system" fn bar_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, data: usize) -> LRESULT {
    let state = &*(data as *const Mutex<BarState>);
    if msg == WM_NCDESTROY {
        RemoveWindowSubclass(hwnd, Some(bar_proc), SUBCLASS_ID);
        drop(Arc::from_raw(data as *const Mutex<BarState>));
        return DefSubclassProc(hwnd, msg, wparam, lparam);
    }
    // Never held across `DefSubclassProc`: winit may run the app (and its `publish`) from inside it.
    let Ok(s) = state.try_lock() else { return DefSubclassProc(hwnd, msg, wparam, lparam) };
    if !s.active {
        drop(s);
        return DefSubclassProc(hwnd, msg, wparam, lparam);
    }
    drop(s);
    let wake = |s: &mut BarState| s.wake.clone();
    match msg {
        // With wparam 0 lparam is the window RECT, otherwise NCCALCSIZE_PARAMS, whose first field is
        // that same RECT; either way it comes back as the client rect.
        WM_NCCALCSIZE => {
            let rect = &mut *(lparam as *mut RECT);
            let (top, bottom) = (rect.top, rect.bottom);
            let r = DefSubclassProc(hwnd, msg, wparam, lparam);
            let native_top = rect.top - top;
            let frame = bottom - rect.bottom;
            // Maximised, the window hangs over the monitor edges by its frame; keep the bar on screen.
            let zoomed = IsZoomed(hwnd) != 0;
            let inset = if zoomed { frame } else { 0 };
            rect.top = top + inset;
            if zoomed {
                leave_room_for_autohide_taskbars(hwnd, rect);
            }
            if native_top > frame {
                if let Ok(mut s) = state.try_lock() {
                    s.frame = frame;
                    if inset == 0 {
                        s.gained = native_top;
                    }
                }
            }
            r
        }
        WM_NCHITTEST => {
            let r = DefSubclassProc(hwnd, msg, wparam, lparam);
            if r as u32 != HTCLIENT {
                return r;
            }
            let mut p = POINT { x: (lparam & 0xffff) as i16 as i32, y: ((lparam >> 16) & 0xffff) as i16 as i32 };
            let Some([width, _]) = client_size(hwnd) else { return r };
            if ScreenToClient(hwnd, &mut p) == 0 {
                return r;
            }
            let zoomed = IsZoomed(hwnd) != 0;
            let resizable = GetWindowLongW(hwnd, GWL_STYLE) as u32 & WS_THICKFRAME != 0;
            match state.try_lock() {
                Ok(s) => bar_hit(&s, p.x, p.y, width, zoomed, resizable) as LRESULT,
                Err(_) => r,
            }
        }
        WM_GETMINMAXINFO => {
            let r = DefSubclassProc(hwnd, msg, wparam, lparam);
            let mmi = &mut *(lparam as *mut MINMAXINFO);
            if let Ok(s) = state.try_lock() {
                mmi.ptMinTrackSize.y = (mmi.ptMinTrackSize.y - s.gained).max(s.frame * 2);
            }
            r
        }
        WM_DPICHANGED => {
            // winit keeps the client size in points, then adds a caption we no longer have.
            let r = DefSubclassProc(hwnd, msg, wparam, lparam);
            let gained = state.try_lock().map_or(0, |s| s.gained);
            if let Some([w, h]) = client_size(hwnd) {
                if IsZoomed(hwnd) == 0 && gained > 0 && h > gained {
                    size_client(hwnd, w, h - gained);
                }
            }
            r
        }
        WM_NCMOUSEMOVE | WM_NCMOUSELEAVE | WM_MOUSEMOVE => {
            let hot = msg == WM_NCMOUSEMOVE && wparam as u32 == HTMAXBUTTON;
            let changed = state.try_lock().ok().and_then(|mut s| {
                let was = (s.max_hot, s.max_down);
                s.max_hot = hot;
                s.max_down &= hot;
                (was != (s.max_hot, s.max_down)).then(|| wake(&mut s)).flatten()
            });
            if hot && changed.is_some() {
                let mut track = TRACKMOUSEEVENT { cbSize: size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE | TME_NONCLIENT, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut track);
            }
            if let Some(wake) = changed {
                wake();
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // Left to `DefWindowProc` a press on HTMAXBUTTON runs its own button loop and paints the old
        // caption button over ours, so the press and release are handled here.
        WM_NCLBUTTONDOWN | WM_NCLBUTTONDBLCLK if wparam as u32 == HTMAXBUTTON => {
            if let Some(wake) = state.try_lock().ok().and_then(|mut s| {
                s.max_down = true;
                wake(&mut s)
            }) {
                wake();
            }
            0
        }
        WM_NCLBUTTONUP if wparam as u32 == HTMAXBUTTON => {
            let pressed = state.try_lock().ok().map(|mut s| (std::mem::take(&mut s.max_down), wake(&mut s)));
            if let Some((true, wake)) = pressed {
                let cmd = if IsZoomed(hwnd) != 0 { SC_RESTORE } else { SC_MAXIMIZE };
                PostMessageW(hwnd, WM_SYSCOMMAND, cmd as WPARAM, 0);
                if let Some(wake) = wake {
                    wake();
                }
            }
            0
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zero-byte moveable block is allocated "discarded" and `GlobalLock`
    /// returns null for it: the failure must be reported, never written
    /// through, and it stops before touching the clipboard.
    #[test]
    fn unlockable_clipboard_memory_is_an_error() {
        assert_eq!(unsafe { set_clipboard(CF_DIB as u32, &[]) }, Err("couldn't lock clipboard memory".to_string()));
    }

    #[test]
    fn clipboard_bitmap_is_bottom_up_bgr_with_transparency_on_white() {
        let img = ColorImage::new([2, 2], vec![Color32::from_rgb(1, 2, 3), Color32::TRANSPARENT, Color32::from_rgba_unmultiplied(200, 100, 0, 128), Color32::from_rgb(9, 8, 7)]);
        let head = size_of::<BITMAPINFOHEADER>();
        let mut out = vec![0; head + 16];
        write_dib(&img, &mut out);
        let [r, g, b, a] = img.pixels[2].to_srgba_unmultiplied();
        let mix = |v: u8| ((v as u32 * a as u32 + 255 * (255 - a as u32)) / 255) as u8;
        assert_eq!(&out[head..], &[mix(b), mix(g), mix(r), 255, 7, 8, 9, 255, 3, 2, 1, 255, 255, 255, 255, 255], "bottom row first");
        assert_eq!(&out[..4], &(head as u32).to_le_bytes());
    }

    #[test]
    fn title_bar_hit_test_moves_on_empty_bar_and_leaves_controls_to_egui() {
        let s = BarState {
            bar: [0, 0, 600, 40],
            controls: vec![[8, 4, 80, 36], [462, 0, 508, 40], [554, 0, 600, 40]],
            max_button: Some([508, 0, 554, 40]),
            frame: 8,
            ..Default::default()
        };
        let hit = |x, y, zoomed| bar_hit(&s, x, y, 600, zoomed, true);
        assert_eq!(hit(300, 20, false), HTCAPTION, "empty bar drags");
        assert_eq!(hit(40, 20, false), HTCLIENT, "a control gets its click");
        assert_eq!(hit(530, 20, false), HTMAXBUTTON, "maximise brings snap layouts");
        assert_eq!(hit(580, 20, false), HTCLIENT, "close is ours");
        assert_eq!(hit(300, 200, false), HTCLIENT, "content below the bar");
        assert_eq!((hit(300, 2, false), hit(3, 2, false), hit(597, 2, false)), (HTTOP, HTTOPLEFT, HTTOPRIGHT), "top edge resizes");
        assert_eq!((hit(300, 2, true), hit(597, 2, true)), (HTCAPTION, HTCLIENT), "a maximised window has no top edge to drag");
        assert_eq!(bar_hit(&s, 300, 2, 600, false, false), HTCAPTION, "nor does one that can't be resized");
        let menu_open = BarState { bar: [0; 4], ..s };
        assert_eq!(bar_hit(&menu_open, 300, 20, 600, false, true), HTCLIENT, "with a menu open the bar is egui's");
    }

    #[test]
    fn the_taskbar_list_is_made_again_after_being_forgotten() {
        std::thread::spawn(|| {
            let com = unsafe { CoInitializeEx(null(), COINIT_APARTMENTTHREADED as u32) } >= 0;
            forget_taskbar_list();
            // A window that isn't there: the list is still made, and the call does nothing.
            set_taskbar_button(null_mut(), false);
            let made = TASKBAR.with(|c| !c.get().is_null());
            forget_taskbar_list();
            assert!(TASKBAR.with(|c| c.get().is_null()));
            set_taskbar_button(null_mut(), false);
            assert_eq!(TASKBAR.with(|c| !c.get().is_null()), made, "made again on the next use");
            forget_taskbar_list();
            if com {
                unsafe { CoUninitialize() };
            }
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_maximised_window_leaves_a_sliver_for_an_autohide_taskbar() {
        use windows_sys::Win32::UI::Shell::{ABE_BOTTOM, ABE_LEFT};
        let mut r = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        autohide_inset(&mut r, |_| false);
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1920, 1080), "no auto-hide taskbar: the whole monitor");
        autohide_inset(&mut r, |e| e == ABE_BOTTOM);
        assert_eq!((r.left, r.top, r.right, r.bottom), (0, 0, 1920, 1078));
        autohide_inset(&mut r, |e| e == ABE_LEFT);
        assert_eq!(r.left, 2);
    }

    #[test]
    fn with_a_menu_open_a_click_on_maximise_only_closes_the_menu() {
        let bar = TitleBar::default();
        let r = |x0: f32, x1: f32| egui::Rect::from_min_max(egui::pos2(x0, 0.0), egui::pos2(x1, 40.0));
        let hit = |bar: &TitleBar| bar_hit(&bar.0.lock().unwrap(), 530, 20, 600, false, true);
        bar.publish(1.0, r(0.0, 600.0), &[r(554.0, 600.0)], Some(r(508.0, 554.0)), true);
        assert_eq!(hit(&bar), HTMAXBUTTON);
        bar.publish(1.0, r(0.0, 600.0), &[r(554.0, 600.0)], Some(r(508.0, 554.0)), false);
        assert_eq!(hit(&bar), HTCLIENT);
    }

    #[test]
    fn numbered_names_go_before_the_extension() {
        let p = Path::new(r"C:\Shots\Snip 2026-11-05 141530.png");
        assert_eq!(numbered(p, 1), p);
        assert_eq!(numbered(p, 2), Path::new(r"C:\Shots\Snip 2026-11-05 141530 (2).png"));
        assert_eq!(numbered(Path::new("a/Recording"), 3), Path::new("a/Recording (3)"));
    }

    #[test]
    fn same_second_files_never_overwrite_each_other() {
        let dir = std::env::temp_dir().join(format!("snipscreen-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let want = dir.join("Snip 2026-11-05 141530.png");
        let img = ColorImage::new([2, 2], vec![Color32::RED; 4]);
        let png = encode_png(&img).unwrap();
        let first = save_new(&want, &png);
        let second = save_new(&want, &png);
        let third = create_unique(&want).map(|(p, _)| p);
        let sizes: Vec<_> = [&first, &second].iter().map(|r| r.as_ref().ok().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.len())).collect();
        let _ = std::fs::remove_dir_all(&dir);
        let name = |r: Result<PathBuf, String>| r.map(|p| p.file_name().unwrap().to_string_lossy().into_owned());
        assert_eq!(name(first), Ok("Snip 2026-11-05 141530.png".into()));
        assert_eq!(name(second), Ok("Snip 2026-11-05 141530 (2).png".into()));
        assert_eq!(name(third), Ok("Snip 2026-11-05 141530 (3).png".into()));
        assert!(sizes.iter().all(|s| s.is_some_and(|s| s > 0)), "both PNGs keep their bytes: {sizes:?}");
    }

    #[test]
    fn registry_values_round_trip_and_delete() {
        let key = format!("Software\\SnipScreen.Tests\\{}", std::process::id());
        let missing = reg_get(&key, "v");
        let set = reg_set(&key, "v", "\"C:\\Program Files\\x.exe\" --tray");
        let read = reg_get(&key, "v");
        let deleted = reg_delete_value(&key, "v");
        let gone = reg_get(&key, "v");
        let again = reg_delete_value(&key, "v");
        let parent = wide("Software\\SnipScreen.Tests");
        unsafe { windows_sys::Win32::System::Registry::RegDeleteTreeW(HKEY_CURRENT_USER, parent.as_ptr()) };
        assert_eq!(missing, None);
        assert_eq!(set, Ok(()));
        assert_eq!(read.as_deref(), Some("\"C:\\Program Files\\x.exe\" --tray"));
        assert_eq!((deleted, gone, again), (Ok(()), None, Ok(())), "deleting twice is fine");
    }

    #[test]
    fn workspace_corner_is_shifted_by_the_taskbar_offset() {
        assert_eq!(workspace_to_screen([100, 50], [0, 0]), [100, 50]);
        // Taskbar docked on the left and top of the primary monitor.
        assert_eq!(workspace_to_screen([100, 50], [48, 40]), [148, 90]);
        // Windows on a monitor left of the primary keep their negative coordinates.
        assert_eq!(workspace_to_screen([-1500, 20], [0, 0]), [-1500, 20]);
    }

    /// A window minimized before the snip must report its restored corner,
    /// not the -32000,-32000 that `GetWindowRect` gives for an icon.
    /// Opens a real window on the desktop; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn minimized_window_reports_its_restored_corner() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, ShowWindow, SW_MINIMIZE, SW_SHOWNORMAL, WS_OVERLAPPEDWINDOW,
        };
        let class = wide("STATIC");
        let hwnd = unsafe { CreateWindowExW(0, class.as_ptr(), wide("t").as_ptr(), WS_OVERLAPPEDWINDOW, 300, 200, 400, 300, null_mut(), null_mut(), null_mut(), null()) };
        assert!(!hwnd.is_null());
        unsafe { ShowWindow(hwnd, SW_SHOWNORMAL) };
        let shown = window_corner(hwnd);
        unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        let minimized = window_corner(hwnd);
        unsafe { DestroyWindow(hwnd) };
        assert_eq!(shown, Some([300, 200]));
        assert_eq!(minimized, shown);
    }
}
