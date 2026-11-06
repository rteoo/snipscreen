//! Thin Win32 layer: screen grab, window list, clipboard, save dialog, hotkey.

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use eframe::egui::{Color32, ColorImage};
use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, POINT, RECT, SYSTEMTIME};
use windows_sys::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows_sys::Win32::Graphics::Gdi::{
    BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    GetDC, GetDIBits, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    SRCCOPY,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::System::Ole::CF_DIB;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_NOREPEAT, MOD_SHIFT, VK_ESCAPE,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_Pictures, FOLDERID_RoamingAppData, FOLDERID_Videos, SHGetKnownFolderPath};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetClientRect, GetCursorPos, GetWindowPlacement, WINDOWPLACEMENT, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, GetMessageW, GetSystemMetrics, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, IsIconic, ShowWindow, SW_SHOWNOACTIVATE, IsWindowVisible, SetWindowPos, GWL_EXSTYLE, MSG,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE,
    SWP_NOSIZE, SWP_NOZORDER, WM_HOTKEY, WM_APP, PeekMessageW, PostThreadMessageW, PM_NOREMOVE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
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
    let [w, h] = img.size;
    let mut dib = Vec::with_capacity(size_of::<BITMAPINFOHEADER>() + w * h * 4);
    let header = bitmap_header(w as i32, h as i32);
    dib.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            &header as *const _ as *const u8,
            size_of::<BITMAPINFOHEADER>(),
        )
    });
    // Bottom-up rows; transparent areas become white so they don't paste as black.
    for row in img.pixels.chunks_exact(w).rev() {
        for c in row {
            let [r, g, b, a] = c.to_srgba_unmultiplied();
            let mix = |v: u8| ((v as u32 * a as u32 + 255 * (255 - a as u32)) / 255) as u8;
            dib.extend_from_slice(&[mix(b), mix(g), mix(r), 255]);
        }
    }
    let png = encode_png(img)?;

    unsafe {
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(owner) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        if !opened {
            return Err("the clipboard is busy".into());
        }
        EmptyClipboard();
        let png_format = RegisterClipboardFormatW(wide("PNG").as_ptr());
        let result = set_clipboard(CF_DIB as u32, &dib).and_then(|_| set_clipboard(png_format, &png));
        CloseClipboard();
        result
    }
}

unsafe fn set_clipboard(format: u32, bytes: &[u8]) -> Result<(), String> {
    let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
    if mem.is_null() {
        return Err("out of memory for the clipboard".into());
    }
    let dst = GlobalLock(mem) as *mut u8;
    if dst.is_null() {
        GlobalFree(mem);
        return Err("couldn't lock clipboard memory".into());
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
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

/// Saves into a new file at `path`, or at the first free "name (2).png",
/// "name (3).png"… beside it, and returns where it went. Never overwrites.
pub fn save_png_new(path: &Path, img: &ColorImage) -> Result<PathBuf, String> {
    let png = encode_png(img)?;
    let (path, mut file) = create_unique(path)?;
    if let Err(e) = file.write_all(&png) {
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

/// Native "Save as" dialog. `None` when the person cancels.
/// `kind` is (description, extension), e.g. ("PNG image", "png").
pub fn ask_save_path(
    owner: HWND,
    start_dir: Option<&std::path::Path>,
    name: &str,
    (description, extension): (&str, &str),
) -> Option<PathBuf> {
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
    if unsafe { GetSaveFileNameW(&mut ofn) } == 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

const HOTKEY_SNIP: i32 = 1;
const HOTKEY_ESCAPE: i32 = 2;
const MSG_ARM_ESCAPE: u32 = WM_APP + 1;
const MSG_DISARM_ESCAPE: u32 = WM_APP + 2;

#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentThreadId() -> u32;
}

/// Handle to the hotkey thread that turns the global Esc hotkey on and off.
/// Dropping it releases Esc.
pub struct EscapeHotkey {
    thread: u32,
}

impl EscapeHotkey {
    /// While on, Esc is taken from whatever app has focus and reported through
    /// the `on_escape` callback instead. Hotkeys belong to the thread that
    /// registered them, so this posts a request to that thread. If another app
    /// already owns Esc the registration quietly fails and only in-app Esc works.
    pub fn set(&self, on: bool) {
        let msg = if on { MSG_ARM_ESCAPE } else { MSG_DISARM_ESCAPE };
        unsafe { PostThreadMessageW(self.thread, msg, 0, 0) };
    }
}

impl Drop for EscapeHotkey {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// Listens for Alt+Shift+S anywhere in Windows on its own thread and calls
/// `on_press` each time. The same thread owns the on-demand Esc hotkey (see
/// [`EscapeHotkey`]) and calls `on_escape` when it fires. Returns an error if
/// another app already owns Alt+Shift+S.
pub fn listen_hotkey(on_press: impl Fn() + Send + 'static, on_escape: impl Fn() + Send + 'static) -> Result<EscapeHotkey, String> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || unsafe {
        let ok = RegisterHotKey(null_mut(), HOTKEY_SNIP, MOD_ALT | MOD_SHIFT | MOD_NOREPEAT, 'S' as u32) != 0;
        let mut msg: MSG = zeroed();
        // Creates this thread's message queue, so a request posted right after
        // the handshake is not lost.
        PeekMessageW(&mut msg, null_mut(), 0, 0, PM_NOREMOVE);
        let _ = ready_tx.send(ok.then(|| GetCurrentThreadId()));
        if !ok {
            return;
        }
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            match msg.message {
                WM_HOTKEY if msg.wParam == HOTKEY_SNIP as usize => on_press(),
                WM_HOTKEY if msg.wParam == HOTKEY_ESCAPE as usize => on_escape(),
                MSG_ARM_ESCAPE => {
                    RegisterHotKey(null_mut(), HOTKEY_ESCAPE, MOD_NOREPEAT, VK_ESCAPE as u32);
                }
                MSG_DISARM_ESCAPE => {
                    UnregisterHotKey(null_mut(), HOTKEY_ESCAPE);
                }
                _ => {}
            }
        }
    });
    match ready_rx.recv() {
        Ok(Some(thread)) => Ok(EscapeHotkey { thread }),
        _ => Err("Alt+Shift+S is already used by another app".into()),
    }
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

/// A top-level window of ours by its exact title.
pub fn find_window(title: &str) -> Option<HWND> {
    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide(title).as_ptr()) };
    (!hwnd.is_null()).then_some(hwnd)
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
        let first = save_png_new(&want, &img);
        let second = save_png_new(&want, &img);
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
