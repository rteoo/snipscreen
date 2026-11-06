//! The resident side of SnipScreen: one thread with a hidden window that owns the tray icon and
//! its menu, the global shortcuts, and takes requests from later launches.
//!
//! The window is a plain top-level one that is never shown (not a message-only window, which
//! would miss the `TaskbarCreated` broadcast). It blocks in `GetMessageW`, so it costs nothing
//! while idle. Everything it hears goes to the shell through one callback as an [`Event`].

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, ERROR_CLASS_ALREADY_EXISTS, HANDLE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VK_CONTROL, VK_ESCAPE, VK_LCONTROL, VK_LMENU,
    VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
    GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowThreadProcessId, KillTimer, LoadImageW, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SendMessageTimeoutW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, SetWindowLongPtrW, TrackPopupMenuEx, TranslateMessage,
    GWLP_USERDATA, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, SMTO_ABORTIFHUNG, SM_CXSMICON, SM_CYSMICON, TPM_BOTTOMALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, CallNextHookEx, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_NULL, WM_QUERYENDSESSION, WM_TIMER,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::settings::Hotkey;

const CLASS: &str = "SnipScreen.Agent";
/// Comes first on the command line Windows runs for a `snipscreen:` link; the link follows it.
pub const OPEN_LINK_FLAG: &str = "--open-link";
const MUTEX: &str = "Local\\SnipScreen.Instance";
/// Tags our `WM_COPYDATA` so a stray one from another program is ignored ("SNIP").
const COPYDATA_TAG: usize = 0x534e_4950;

const HOTKEY_ESCAPE: i32 = 2;
/// The snip shortcut alternates between two ids, so a new one is registered before the old one is let go.
const HOTKEY_SNIP: [i32; 2] = [1, 3];

const MSG_ARM_ESCAPE: u32 = WM_APP + 1;
const MSG_DISARM_ESCAPE: u32 = WM_APP + 2;
const MSG_SET_HOTKEY: u32 = WM_APP + 3;
const MSG_RECORD: u32 = WM_APP + 4;
/// Posted by the keyboard hook: wParam the `MOD_*` flags, lParam the key, or -1 for Esc.
const MSG_RECORDED: u32 = WM_APP + 5;
/// Posted by `Agent::drop`; the only thing that destroys the window. A `WM_CLOSE` from outside
/// (`taskkill` without /f) is a request to quit, which the shell carries out.
const MSG_DESTROY: u32 = WM_APP + 6;
/// Posted by the keyboard hook when the foreground left SnipScreen while recording.
const MSG_RECORD_LOST: u32 = WM_APP + 7;
/// How long signing out waits for the shell to finish (a recording to be finalised, settings to be written).
const END_SESSION_WAIT: Duration = Duration::from_secs(5);

/// `WM_COPYDATA` answers: taken, or refused because this copy is closing.
const COPYDATA_TAKEN: usize = 1;
const COPYDATA_CLOSING: usize = 2;
/// The tray icon's callback message.
const MSG_TRAY: u32 = WM_APP + 10;
/// Enter or Space on the focused tray icon (`NIN_SELECT | NINF_KEY`).
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;

/// Retries adding the tray icon while the taskbar isn't up yet (right after sign-in).
const TIMER_TRAY_RETRY: usize = 1;
const TRAY_RETRY_MS: u32 = 2000;
const TRAY_RETRIES: u8 = 15;

const MENU_NEW: usize = 1;
const MENU_OPEN: usize = 2;
const MENU_SETTINGS: usize = 3;
const MENU_QUIT: usize = 4;

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16) -> HANDLE;
    fn GetModuleHandleW(name: *const u16) -> HMODULE;
}

#[link(name = "user32")]
extern "system" {
    fn GetDpiForWindow(hwnd: HWND) -> u32;
    fn GetSystemMetricsForDpi(index: i32, dpi: u32) -> i32;
}

/// The exe's icon resource; `winresource` embeds `assets/snipscreen.ico` under id 1.
const ICON_RESOURCE: usize = 1;

/// What a launch of the program asks for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Intent {
    /// Opened from the Start menu, a shortcut or Explorer: show the home window.
    Show,
    /// Started with Windows (`--tray`): stay out of sight.
    Tray,
    /// `--quit`: the running copy should exit (the installer uses this).
    Quit,
    /// A `snipscreen:` link, from a click on a notification (`--open-link <link>`, see `main::parse_args`).
    Open(String),
}

impl Intent {
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Intent {
        for a in args.iter().map(AsRef::as_ref) {
            match a {
                "--tray" => return Intent::Tray,
                "--quit" => return Intent::Quit,

                _ => {}
            }
        }
        Intent::Show
    }

    fn encode(&self) -> String {
        match self {
            Intent::Show => "show".into(),
            Intent::Tray => "tray".into(),
            Intent::Quit => "quit".into(),
            Intent::Open(uri) => format!("open {uri}"),
        }
    }

    fn decode(s: &str) -> Option<Intent> {
        match s {
            "show" => Some(Intent::Show),
            "tray" => Some(Intent::Tray),
            "quit" => Some(Intent::Quit),
            _ => s.strip_prefix("open ").map(|uri| Intent::Open(uri.to_string())),
        }
    }
}

/// Everything the agent thread reports to the shell.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// The snip shortcut was pressed.
    Snip,
    /// Esc while it is armed (during a countdown).
    Escape,
    /// A later launch forwarded what it was started for.
    Intent(Intent),
    /// Result of [`Agent::set_hotkey`]: the shortcut now in force, or why the new one was refused
    /// (the old one stays registered).
    HotkeySet(Result<Hotkey, String>),
    /// The combination caught by [`Agent::record`]; `None` when Esc cancelled it.
    Recorded(Option<Hotkey>),
    /// Settings… in the tray menu.
    Settings,
    /// The shortcut was set aside while recording and couldn't be taken back (another app has it
    /// now): no snip shortcut is registered any more, for the reason given.
    HotkeyLost(String),
    /// Explorer (re)started and rebuilt the taskbar.
    TaskbarCreated,
}

/// True when this is the only running copy. The mutex lives as long as the process.
pub fn claim_single_instance() -> bool {
    let name = wide(MUTEX);
    unsafe {
        let handle = CreateMutexW(null(), 0, name.as_ptr());
        // Without a mutex there is no way to tell; running is better than refusing to start.
        if handle.is_null() || GetLastError() != ERROR_ALREADY_EXISTS {
            return true;
        }
        // Ours would keep the mutex alive after the running copy has gone.
        CloseHandle(handle);
        false
    }
}

/// What the running copy said to a forwarded launch.
#[derive(Debug, PartialEq)]
pub enum Forwarded {
    Taken,
    /// It is quitting; this launch should wait for it to go and then run itself.
    Closing,
}

/// Hands `intent` to the copy that is already running, waiting up to ~3 s for its window (it may
/// still be starting) and letting it take the foreground, which this launch was given.
pub fn forward(intent: &Intent) -> Result<Forwarded, String> {
    let class = wide(CLASS);
    let text: Vec<u16> = intent.encode().encode_utf16().collect();
    for _ in 0..150 {
        let hwnd = unsafe { FindWindowW(class.as_ptr(), null()) };
        if hwnd.is_null() {
            std::thread::sleep(std::time::Duration::from_millis(20));
            continue;
        }
        unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            AllowSetForegroundWindow(pid);
            let data = COPYDATASTRUCT { dwData: COPYDATA_TAG, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut c_void };
            let mut answer = 0usize;
            let sent = SendMessageTimeoutW(hwnd, WM_COPYDATA, 0, &data as *const _ as LPARAM, SMTO_ABORTIFHUNG, 3000, &mut answer);
            return match (sent != 0, answer) {
                (true, COPYDATA_TAKEN) => Ok(Forwarded::Taken),
                (true, COPYDATA_CLOSING) => Ok(Forwarded::Closing),
                _ => Err("SnipScreen is already running but didn't answer".into()),
            };
        }
    }
    Err("SnipScreen is already starting but its window never appeared".into())
}

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "SnipScreen";

/// The command Windows runs at sign-in: this exe, quoted, starting in the tray.
pub fn startup_command(exe: &std::path::Path) -> String {
    format!("\"{}\" --tray", exe.display())
}

/// Makes the `Run` entry match `enabled`: written when missing or pointing at another copy (the exe
/// moved, or another install wrote it), removed when the setting is off.
pub fn sync_run_at_startup(enabled: bool) -> Result<(), String> {
    if !enabled {
        return crate::win::reg_delete_value(RUN_KEY, RUN_VALUE);
    }
    let want = startup_command(&crate::win::exe_path()?);
    if crate::win::reg_get(RUN_KEY, RUN_VALUE).as_deref() == Some(want.as_str()) {
        return Ok(());
    }
    crate::win::reg_set(RUN_KEY, RUN_VALUE, &want)
}

/// Handle to the agent thread. Dropping it unregisters the shortcuts and ends the thread.
pub struct Agent {
    /// The hidden window; `HWND` itself is not `Send`.
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
    exit: Arc<ExitGate>,
}

/// How the shell's exit reaches the agent thread: `closing` once it has begun (later launches are
/// turned away), `done` once it has finished (signing out may go on).
#[derive(Default)]
struct ExitGate {
    closing: AtomicBool,
    done: Mutex<bool>,
    finished: Condvar,
}

/// The agent and how its first registration went.
pub struct Started {
    pub agent: Agent,
    /// The shortcut in force, or why `hotkey` couldn't be registered.
    pub hotkey: Result<Hotkey, String>,
}

/// What the window procedure needs; lives on the agent thread, reached through `GWLP_USERDATA`.
struct State {
    on_event: Box<dyn Fn(Event)>,
    hotkey: Option<Hotkey>,
    /// Index into `HOTKEY_SNIP` of the id the current shortcut is registered under.
    slot: usize,
    /// The small icon from the exe, kept for re-adding the tray icon.
    icon: HICON,
    /// Whether the tray icon is in the notification area right now.
    tray_added: bool,
    tray_retries: u8,
    /// Explorer broadcasts this after it (re)starts; every tray icon must be added again.
    taskbar_created: u32,
    /// The keyboard hook while a shortcut is being recorded.
    hook: HHOOK,
    exit: Arc<ExitGate>,
}

/// The agent window, for the keyboard hook to post to (a hook procedure gets no context of its own).
static RECORD_TO: AtomicIsize = AtomicIsize::new(0);
/// Win held down: the hook swallows Win, so `GetAsyncKeyState` never sees it.
static RECORD_WIN: AtomicBool = AtomicBool::new(false);
/// A combination was posted; later keys pass untouched until the hook is removed.
static RECORD_DONE: AtomicBool = AtomicBool::new(false);

// Tests call the hook directly with made-up keys (`tests::press`) and say whether SnipScreen is
// in front. The real hook, live meanwhile on the agent thread, ignores the person's own typing
// then, or a key pressed in another app during `cargo test` would break the test.
#[cfg(test)]
thread_local! {
    static TEST_KEY: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the window in front belongs to this process (the Settings window, while recording).
unsafe fn ours_in_front() -> bool {
    #[cfg(test)]
    if let Some(front) = TEST_KEY.with(|k| k.get()) {
        return front;
    }
    let mut pid = 0;
    GetWindowThreadProcessId(GetForegroundWindow(), &mut pid);
    pid == std::process::id()
}

fn is_modifier(vk: u16) -> bool {
    matches!(vk, VK_SHIFT | VK_LSHIFT | VK_RSHIFT | VK_CONTROL | VK_LCONTROL | VK_RCONTROL | VK_MENU | VK_LMENU | VK_RMENU)
}

/// Low-level keyboard hook, live only while recording: notes the first non-modifier key pressed
/// with the modifiers held at the time. Ctrl, Alt and Shift still reach the focused window (the
/// Settings window, which ignores them); Win doesn't reach the system, and the recorded key
/// reaches nothing, so Win+Shift+S doesn't open Windows' snipping bar and Alt+F4 or Alt+Space
/// don't act on the Settings window. Once another app is in front it lets every key through and
/// has itself taken off: it must never eat keys meant for something else.
unsafe extern "system" fn record_hook(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    #[cfg(test)]
    if TEST_KEY.with(|k| k.get()).is_none() {
        return CallNextHookEx(null_mut(), code, w, l);
    }
    if code < 0 {
        return CallNextHookEx(null_mut(), code, w, l);
    }
    if !ours_in_front() {
        RECORD_WIN.store(false, Ordering::Relaxed);
        if !RECORD_DONE.swap(true, Ordering::Relaxed) {
            PostMessageW(RECORD_TO.load(Ordering::Relaxed) as HWND, MSG_RECORD_LOST, 0, 0);
        }
        return CallNextHookEx(null_mut(), code, w, l);
    }
    let key = &*(l as *const KBDLLHOOKSTRUCT);
    let vk = key.vkCode as u16;
    let down = matches!(w as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
    // Up and down alike, until the hook is off: a lone Win release would open Start.
    if vk == VK_LWIN || vk == VK_RWIN {
        RECORD_WIN.store(down, Ordering::Relaxed);
        return 1;
    }
    if !down || is_modifier(vk) || RECORD_DONE.load(Ordering::Relaxed) {
        return CallNextHookEx(null_mut(), code, w, l);
    }
    let held = |k: u16| GetAsyncKeyState(k as i32) < 0;
    let mut mods = 0;
    for (k, m) in [(VK_CONTROL, MOD_CONTROL), (VK_MENU, MOD_ALT), (VK_SHIFT, MOD_SHIFT)] {
        if held(k) {
            mods |= m;
        }
    }
    if RECORD_WIN.load(Ordering::Relaxed) {
        mods |= MOD_WIN;
    }
    let result = if vk == VK_ESCAPE && mods == 0 { -1 } else { vk as isize };
    RECORD_DONE.store(true, Ordering::Relaxed);
    PostMessageW(RECORD_TO.load(Ordering::Relaxed) as HWND, MSG_RECORDED, mods as WPARAM, result);
    1
}

/// Takes the hook off and gives the snip shortcut back, telling the shell if it can't be.
unsafe fn stop_recording(hwnd: HWND, st: &mut State) {
    if st.hook.is_null() {
        return;
    }
    UnhookWindowsHookEx(st.hook);
    st.hook = null_mut();
    RECORD_WIN.store(false, Ordering::Relaxed);
    if let Some(h) = st.hotkey {
        if let Err(e) = register(hwnd, HOTKEY_SNIP[st.slot], h) {
            st.hotkey = None;
            (st.on_event)(Event::HotkeyLost(format!("{e}: it was taken while you were choosing a new shortcut")));
        }
    }
}

impl Agent {
    /// Starts the thread and its window, registers `hotkey` and, with `tray`, adds the tray icon.
    /// Fails only if the window can't be made.
    pub fn start(hotkey: Hotkey, tray: bool, on_event: impl Fn(Event) + Send + 'static) -> Result<Started, String> {
        let (ready_tx, ready_rx) = channel();
        let exit = Arc::new(ExitGate::default());
        let exit_for_thread = exit.clone();
        let thread = std::thread::spawn(move || unsafe {
            let hwnd = match create_window() {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let first = register(hwnd, HOTKEY_SNIP[0], hotkey);
            let icon = load_tray_icon(GetDpiForWindow(hwnd));
            let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            let state = Box::new(State {
                on_event: Box::new(on_event),
                hotkey: first.as_ref().ok().copied(),
                slot: 0,
                icon,
                tray_added: false,
                tray_retries: 0,
                taskbar_created,
                hook: null_mut(),
                exit: exit_for_thread,
            });
            let state = Box::into_raw(state);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            if tray {
                add_tray_icon(hwnd, &mut *state);
            }
            let _ = ready_tx.send(Ok((hwnd as isize, first)));
            let mut msg: MSG = zeroed();
            while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        });
        match ready_rx.recv() {
            Ok(Ok((hwnd, hotkey))) => Ok(Started { agent: Agent { hwnd, thread: Some(thread), exit }, hotkey }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("the shortcut thread stopped while starting".into()),
        }
    }

    fn post(&self, msg: u32, w: WPARAM, l: LPARAM) {
        unsafe { PostMessageW(self.hwnd as HWND, msg, w, l) };
    }

    /// While on, Esc is taken from whatever app has focus and reported as [`Event::Escape`]. If another
    /// app already owns Esc the registration quietly fails and only in-app Esc works.
    pub fn set_escape(&self, on: bool) {
        self.post(if on { MSG_ARM_ESCAPE } else { MSG_DISARM_ESCAPE }, 0, 0);
    }

    /// Swaps the snip shortcut; the answer comes back as [`Event::HotkeySet`].
    pub fn set_hotkey(&self, hotkey: Hotkey) {
        self.post(MSG_SET_HOTKEY, hotkey.mods as WPARAM, hotkey.vk as LPARAM);
    }

    /// While on, the next key combination pressed anywhere comes back as [`Event::Recorded`] instead
    /// of doing anything, and the snip shortcut is set aside so pressing it is recorded too. Win and
    /// Print Screen are kept from the system meanwhile, so recording Win+Shift+S doesn't open
    /// Windows' own snipping bar. Turning it off (or a recorded key) puts the shortcut back.
    pub fn record(&self, on: bool) {
        self.post(MSG_RECORD, on as WPARAM, 0);
    }

    /// The shell has begun to exit: later launches are refused (`Forwarded::Closing`), so they
    /// start once this copy is gone instead of being lost.
    pub fn closing(&self) {
        self.exit.closing.store(true, Ordering::Relaxed);
    }

    /// The shell has finished exiting (recording finalised, settings written): a sign-out held
    /// back in `WM_ENDSESSION` may go on.
    pub fn exited(&self) {
        *self.exit.done.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.exit.finished.notify_all();
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.exited();
        self.post(MSG_DESTROY, 0, 0);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn create_window() -> Result<HWND, String> {
    let class = wide(CLASS);
    let instance = GetModuleHandleW(null());
    let wc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(agent_proc), hInstance: instance, lpszClassName: class.as_ptr(), ..zeroed() };
    // Already registered when an agent ran before in this process (tests start several).
    if RegisterClassExW(&wc) == 0 && GetLastError() != ERROR_CLASS_ALREADY_EXISTS {
        return Err(format!("couldn't register the agent window class (error {})", GetLastError()));
    }
    let title = wide("SnipScreen");
    let hwnd = CreateWindowExW(WS_EX_TOOLWINDOW, class.as_ptr(), title.as_ptr(), WS_POPUP, 0, 0, 0, 0, null_mut(), null_mut(), instance, null());
    if hwnd.is_null() {
        return Err(format!("couldn't create the agent window (error {})", GetLastError()));
    }
    Ok(hwnd)
}

fn tray_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW { cbSize: size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: 1, ..Default::default() }
}

/// The exe's icon at the small-icon size for `dpi`.
unsafe fn load_tray_icon(dpi: u32) -> HICON {
    let dpi = if dpi == 0 { 96 } else { dpi };
    let (w, h) = (GetSystemMetricsForDpi(SM_CXSMICON, dpi), GetSystemMetricsForDpi(SM_CYSMICON, dpi));
    LoadImageW(GetModuleHandleW(null()), ICON_RESOURCE as *const u16, IMAGE_ICON, w, h, LR_DEFAULTCOLOR) as HICON
}

/// Swaps in the icon for the current scale (the display or its scaling changed).
unsafe fn reload_tray_icon(hwnd: HWND, st: &mut State, dpi: u32) {
    let icon = load_tray_icon(dpi);
    if icon.is_null() {
        return;
    }
    if st.tray_added {
        let mut nid = tray_data(hwnd);
        nid.uFlags = NIF_ICON;
        nid.hIcon = icon;
        Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
    if !st.icon.is_null() {
        DestroyIcon(st.icon);
    }
    st.icon = icon;
}

/// Puts the icon in the notification area. Right after sign-in the taskbar may not exist yet, so a
/// failure is retried on a timer for a while; Explorer's `TaskbarCreated` also re-adds it. An icon
/// still there (the broadcast also comes when only the taskbar's settings changed) is taken off
/// first, or the add would fail and leave a ghost.
unsafe fn add_tray_icon(hwnd: HWND, st: &mut State) {
    Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
    let mut nid = tray_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = MSG_TRAY;
    nid.hIcon = st.icon;
    for (dst, src) in nid.szTip.iter_mut().zip("SnipScreen".encode_utf16()) {
        *dst = src;
    }
    st.tray_added = Shell_NotifyIconW(NIM_ADD, &nid) != 0;
    if st.tray_added {
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        Shell_NotifyIconW(NIM_SETVERSION, &nid);
        KillTimer(hwnd, TIMER_TRAY_RETRY);
    } else if st.tray_retries < TRAY_RETRIES {
        st.tray_retries += 1;
        SetTimer(hwnd, TIMER_TRAY_RETRY, TRAY_RETRY_MS, None);
    } else {
        KillTimer(hwnd, TIMER_TRAY_RETRY);
    }
}

/// The right-click menu at screen point `x`, `y`, with the snip shortcut shown beside New snip.
/// Returns the chosen item, 0 for none. Runs a modal loop, so the window procedure re-enters.
unsafe fn tray_menu(hwnd: HWND, shortcut: Option<String>, x: i32, y: i32) -> usize {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return 0;
    }
    let new = match shortcut {
        Some(key) => format!("New snip\t{key}"),
        None => "New snip".into(),
    };
    AppendMenuW(menu, MF_STRING, MENU_NEW, wide(&new).as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_OPEN, wide("Open SnipScreen").as_ptr());
    AppendMenuW(menu, MF_STRING, MENU_SETTINGS, wide("Settings…").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, MENU_QUIT, wide("Quit").as_ptr());
    SetMenuDefaultItem(menu, MENU_OPEN as u32, 0);
    // Without being the foreground window the menu wouldn't close when the person clicks elsewhere;
    // the WM_NULL afterwards is the documented fix for the menu closing on the second try.
    SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenuEx(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, x, y, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    chosen as usize
}

unsafe fn register(hwnd: HWND, id: i32, hotkey: Hotkey) -> Result<Hotkey, String> {
    if RegisterHotKey(hwnd, id, hotkey.mods | MOD_NOREPEAT, hotkey.vk) != 0 {
        Ok(hotkey)
    } else {
        Err(format!("{} is already used by another app", hotkey.label()))
    }
}

unsafe extern "system" fn agent_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
    if state.is_null() {
        return DefWindowProcW(hwnd, msg, w, l);
    }
    let st = &mut *state;
    match msg {
        WM_HOTKEY if w as i32 == HOTKEY_ESCAPE => (st.on_event)(Event::Escape),
        WM_HOTKEY if w as i32 == HOTKEY_SNIP[st.slot] => (st.on_event)(Event::Snip),
        MSG_ARM_ESCAPE => {
            RegisterHotKey(hwnd, HOTKEY_ESCAPE, MOD_NOREPEAT, VK_ESCAPE as u32);
        }
        MSG_DISARM_ESCAPE => {
            UnregisterHotKey(hwnd, HOTKEY_ESCAPE);
        }
        MSG_SET_HOTKEY => {
            let want = Hotkey { mods: w as u32, vk: l as u32 };
            let result = if st.hotkey == Some(want) {
                Ok(want)
            } else {
                let next = 1 - st.slot;
                let r = register(hwnd, HOTKEY_SNIP[next], want);
                if r.is_ok() {
                    if st.hotkey.is_some() {
                        UnregisterHotKey(hwnd, HOTKEY_SNIP[st.slot]);
                    }
                    (st.slot, st.hotkey) = (next, Some(want));
                }
                r
            };
            (st.on_event)(Event::HotkeySet(result));
        }
        MSG_RECORD if w != 0 => {
            if st.hook.is_null() {
                if st.hotkey.is_some() {
                    UnregisterHotKey(hwnd, HOTKEY_SNIP[st.slot]);
                }
                RECORD_TO.store(hwnd as isize, Ordering::Relaxed);
                RECORD_DONE.store(false, Ordering::Relaxed);
                st.hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(record_hook), GetModuleHandleW(null()), 0);
                if st.hook.is_null() {
                    stop_recording(hwnd, st);
                    (st.on_event)(Event::Recorded(None));
                }
            }
        }
        MSG_RECORD => stop_recording(hwnd, st),
        MSG_RECORDED => {
            if !st.hook.is_null() {
                stop_recording(hwnd, st);
                let caught = (l != -1).then_some(Hotkey { mods: w as u32, vk: l as u32 });
                (st.on_event)(Event::Recorded(caught));
            }
        }
        // The person went to another app while the recorder listened: cancelled.
        MSG_RECORD_LOST => {
            if !st.hook.is_null() {
                stop_recording(hwnd, st);
                (st.on_event)(Event::Recorded(None));
            }
        }
        WM_COPYDATA => {
            let data = &*(l as *const COPYDATASTRUCT);
            if data.dwData != COPYDATA_TAG || data.lpData.is_null() {
                return 0;
            }
            if st.exit.closing.load(Ordering::Relaxed) {
                return COPYDATA_CLOSING as LRESULT;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2));
            return match Intent::decode(&text) {
                Some(intent) => {
                    (st.on_event)(Event::Intent(intent));
                    COPYDATA_TAKEN as LRESULT
                }
                None => 0,
            };
        }
        MSG_TRAY => {
            // NOTIFYICON_VERSION_4: the event is in the low word of lParam, the anchor point in wParam.
            let (x, y) = ((w & 0xffff) as i16 as i32, ((w >> 16) & 0xffff) as i16 as i32);
            match (l & 0xffff) as u32 {
                NIN_SELECT | NIN_KEYSELECT | WM_LBUTTONDBLCLK => (st.on_event)(Event::Intent(Intent::Show)),
                WM_CONTEXTMENU => {
                    let chosen = tray_menu(hwnd, st.hotkey.map(Hotkey::label), x, y);
                    // The menu's loop ran this procedure again, and may have destroyed the window
                    // (and freed the state) on the way: look the state up afresh.
                    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
                    if state.is_null() {
                        return 0;
                    }
                    let st = &mut *state;
                    match chosen {
                        MENU_NEW => (st.on_event)(Event::Snip),
                        MENU_OPEN => (st.on_event)(Event::Intent(Intent::Show)),
                        MENU_SETTINGS => (st.on_event)(Event::Settings),
                        MENU_QUIT => (st.on_event)(Event::Intent(Intent::Quit)),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        WM_TIMER if w == TIMER_TRAY_RETRY => {
            if !st.tray_added {
                add_tray_icon(hwnd, st);
            }
        }
        WM_DPICHANGED => reload_tray_icon(hwnd, st, ((w >> 16) & 0xffff) as u32),
        WM_DISPLAYCHANGE => reload_tray_icon(hwnd, st, GetDpiForWindow(hwnd)),
        _ if msg == st.taskbar_created && st.taskbar_created != 0 => {
            // Only when an icon was asked for: one that was added, or one still being retried.
            if st.tray_added || st.tray_retries > 0 {
                st.tray_retries = 0;
                add_tray_icon(hwnd, st);
            }
            (st.on_event)(Event::TaskbarCreated);
        }
        WM_QUERYENDSESSION => return 1,
        // Windows is signing out, or the installer's Restart Manager wants the file: leave cleanly.
        // The session may end as soon as this returns, so it waits (a while) for the shell to
        // stop a recording, finalise its file and write the settings.
        WM_ENDSESSION if w != 0 => {
            (st.on_event)(Event::Intent(Intent::Quit));
            let exit = st.exit.clone();
            let done = exit.done.lock().unwrap_or_else(PoisonError::into_inner);
            drop(exit.finished.wait_timeout_while(done, END_SESSION_WAIT, |done| !*done));
        }
        // From outside: `taskkill` without /f, or a tool closing windows. The shell quits as from
        // the tray menu; the window stays until then.
        WM_CLOSE => (st.on_event)(Event::Intent(Intent::Quit)),
        MSG_DESTROY => {
            DestroyWindow(hwnd);
        }
        WM_DESTROY => {
            if !st.hook.is_null() {
                UnhookWindowsHookEx(st.hook);
            }
            for id in [HOTKEY_SNIP[0], HOTKEY_SNIP[1], HOTKEY_ESCAPE] {
                UnregisterHotKey(hwnd, id);
            }
            if st.tray_added {
                Shell_NotifyIconW(NIM_DELETE, &tray_data(hwnd));
            }
            if !st.icon.is_null() {
                DestroyIcon(st.icon);
            }
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            drop(Box::from_raw(state));
            PostQuitMessage(0);
        }
        _ => return DefWindowProcW(hwnd, msg, w, l),
    }
    0
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Receiver;
    use std::time::Duration;

    #[test]
    fn launch_arguments_name_the_intent() {
        assert_eq!(Intent::from_args::<&str>(&[]), Intent::Show);
        assert_eq!(Intent::from_args(&["--tray"]), Intent::Tray);
        assert_eq!(Intent::from_args(&["--quit"]), Intent::Quit);
        assert_eq!(Intent::from_args(&["snipscreen:open?id=4&path=x"]), Intent::Show, "a link counts only after --open-link");
        assert_eq!(Intent::from_args(&["--unknown", "--tray"]), Intent::Tray);
    }

    #[test]
    fn startup_command_quotes_the_path_and_starts_in_the_tray() {
        let cmd = startup_command(std::path::Path::new(r"C:\Users\A B\AppData\Local\Programs\SnipScreen\snipscreen.exe"));
        assert_eq!(cmd, r#""C:\Users\A B\AppData\Local\Programs\SnipScreen\snipscreen.exe" --tray"#);
        assert_eq!(Intent::from_args(&cmd.rsplit(' ').take(1).collect::<Vec<_>>()), Intent::Tray);
    }

    #[test]
    fn intents_survive_the_trip_to_the_running_copy() {
        for i in [Intent::Show, Intent::Tray, Intent::Quit, Intent::Open("snipscreen:open?id=1&path=C%3A%5Cx.png".into())] {
            assert_eq!(Intent::decode(&i.encode()), Some(i));
        }
        assert_eq!(Intent::decode("format c:"), None);
    }

    fn next(rx: &Receiver<Event>) -> Event {
        rx.recv_timeout(Duration::from_secs(3)).expect("the agent answers")
    }

    /// A real agent window: a forwarded launch arrives as an event, a refused shortcut keeps the old
    /// one, and dropping the agent ends its thread. Uses obscure shortcuts nobody else holds.
    #[test]
    fn agent_takes_forwarded_launches_and_swaps_shortcuts() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_F1};
        let (tx, rx) = channel();
        let first = Hotkey { mods: MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, vk: VK_F1 as u32 + 21 };
        // No tray icon: a test must not put one in the person's notification area.
        let Started { agent, hotkey } = Agent::start(first, false, move |e| tx.send(e).unwrap()).expect("agent window");
        assert_eq!(hotkey, Ok(first));

        // `forward` looks the window up by class, so a copy run by another test or the real app could
        // answer instead; send straight to ours.
        let text: Vec<u16> = Intent::Open("snipscreen:open?id=9".into()).encode().encode_utf16().collect();
        let data = COPYDATASTRUCT { dwData: COPYDATA_TAG, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut c_void };
        let mut answer = 0usize;
        unsafe { SendMessageTimeoutW(agent.hwnd as HWND, WM_COPYDATA, 0, &data as *const _ as LPARAM, SMTO_ABORTIFHUNG, 3000, &mut answer) };
        assert_eq!(answer, 1);
        assert_eq!(next(&rx), Event::Intent(Intent::Open("snipscreen:open?id=9".into())));

        // A shortcut held elsewhere (here: by this thread) is refused and the old one is kept.
        let taken = Hotkey { mods: MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, vk: VK_F1 as u32 + 22 };
        assert_ne!(unsafe { RegisterHotKey(null_mut(), 77, taken.mods, taken.vk) }, 0);
        agent.set_hotkey(taken);
        assert!(matches!(next(&rx), Event::HotkeySet(Err(e)) if e.contains("already used by another app")));
        unsafe { UnregisterHotKey(null_mut(), 77) };
        agent.set_hotkey(taken);
        assert_eq!(next(&rx), Event::HotkeySet(Ok(taken)));
        // The first shortcut was let go once the second one took over.
        assert_ne!(unsafe { RegisterHotKey(null_mut(), 78, first.mods, first.vk) }, 0, "the old shortcut is free again");
        unsafe { UnregisterHotKey(null_mut(), 78) };
        drop(agent);
        assert!(rx.recv_timeout(Duration::from_secs(1)).is_err(), "the thread is gone and sent nothing more");
    }

    /// Recording sets the shortcut aside and puts it back once a key is caught. The hook procedure
    /// is called directly with a made-up key, so no input is sent to the desktop; the real hook is live
    /// for the few milliseconds in between.
    #[test]
    fn recording_sets_the_shortcut_aside_and_reports_the_key() {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_F1};
        let _front = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let (tx, rx) = channel();
        let first = Hotkey { mods: MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, vk: VK_F1 as u32 + 20 };
        let Started { agent, .. } = Agent::start(first, false, move |e| tx.send(e).unwrap()).expect("agent window");
        let free = || unsafe {
            let ok = RegisterHotKey(null_mut(), 79, first.mods, first.vk) != 0;
            if ok {
                UnregisterHotKey(null_mut(), 79);
            }
            ok
        };
        assert!(!free(), "registered by the agent");
        agent.record(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !free() {
            assert!(std::time::Instant::now() < deadline, "recording should set the shortcut aside");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(press(VK_F1 as u32 + 6, true), 1, "the recorded key reaches nothing (Alt+F4 must not close Settings)");
        match next(&rx) {
            Event::Recorded(Some(h)) => assert_eq!(h.vk, VK_F1 as u32 + 6),
            other => panic!("expected the F7 press, got {other:?}"),
        }
        assert!(!free(), "the shortcut is back once the key was caught");
        agent.record(true);
        while !free() {
            std::thread::sleep(Duration::from_millis(5));
        }
        agent.record(false);
        // Probing in a loop could grab the shortcut just as the agent takes it back, so give the agent
        // ample time (it needs microseconds) and look once.
        std::thread::sleep(Duration::from_millis(300));
        assert!(!free(), "turning recording off puts the shortcut back");
        assert!(rx.try_recv().is_err(), "a cancelled recording reports nothing");
    }

    /// The hook reports to the last agent that started recording (`RECORD_TO`); one test at a time.
    static HOOK_TESTS: Mutex<()> = Mutex::new(());

    /// Calls the hook as Windows would for a key going down, with SnipScreen in front or not.
    fn press(vk: u32, ours_in_front: bool) -> LRESULT {
        let key = KBDLLHOOKSTRUCT { vkCode: vk, ..unsafe { zeroed() } };
        TEST_KEY.with(|k| k.set(Some(ours_in_front)));
        let r = unsafe { record_hook(0, WM_KEYDOWN as WPARAM, &key as *const _ as LPARAM) };
        TEST_KEY.with(|k| k.set(None));
        r
    }

    fn obscure(vk_offset: u32) -> Hotkey {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_F1;
        Hotkey { mods: MOD_CONTROL | MOD_ALT | MOD_SHIFT | MOD_WIN, vk: VK_F1 as u32 + vk_offset }
    }

    fn send(hwnd: isize, msg: u32, w: WPARAM, l: LPARAM) -> usize {
        let mut answer = 0usize;
        unsafe { SendMessageTimeoutW(hwnd as HWND, msg, w, l, SMTO_ABORTIFHUNG, 10_000, &mut answer) };
        answer
    }

    fn send_intent(hwnd: isize, intent: &Intent) -> usize {
        let text: Vec<u16> = intent.encode().encode_utf16().collect();
        let data = COPYDATASTRUCT { dwData: COPYDATA_TAG, cbData: (text.len() * 2) as u32, lpData: text.as_ptr() as *mut c_void };
        send(hwnd, WM_COPYDATA, 0, &data as *const _ as LPARAM)
    }

    /// `taskkill` without /f sends WM_CLOSE: the shell is asked to quit and the window lives on
    /// until it does; once the shell is closing, later launches are told so and not lost.
    #[test]
    fn an_outside_close_is_a_quit_request() {
        use windows_sys::Win32::UI::WindowsAndMessaging::IsWindow;
        let (tx, rx) = channel();
        let Started { agent, .. } = Agent::start(obscure(19), false, move |e| tx.send(e).unwrap()).expect("agent window");
        let hwnd = agent.hwnd;
        send(hwnd, WM_CLOSE, 0, 0);
        assert_eq!(next(&rx), Event::Intent(Intent::Quit));
        assert_ne!(unsafe { IsWindow(hwnd as HWND) }, 0, "the window, the tray icon and the shortcuts are still there");
        assert_eq!(send_intent(hwnd, &Intent::Show), COPYDATA_TAKEN);
        assert_eq!(next(&rx), Event::Intent(Intent::Show));
        agent.closing();
        assert_eq!(send_intent(hwnd, &Intent::Show), COPYDATA_CLOSING, "a launch while closing is turned away");
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "and not passed on");
        drop(agent);
        assert_eq!(unsafe { IsWindow(hwnd as HWND) }, 0, "dropping the agent destroys it");
    }

    /// Signing out holds `WM_ENDSESSION` until the shell has finished exiting, so a recording can
    /// be finalised first.
    #[test]
    fn signing_out_waits_for_the_shell() {
        let (tx, rx) = channel();
        let Started { agent, .. } = Agent::start(obscure(18), false, move |e| tx.send(e).unwrap()).expect("agent window");
        let hwnd = agent.hwnd;
        let t0 = std::time::Instant::now();
        let session = std::thread::spawn(move || {
            send(hwnd, WM_ENDSESSION, 1, 0);
            t0.elapsed()
        });
        assert_eq!(next(&rx), Event::Intent(Intent::Quit));
        std::thread::sleep(Duration::from_millis(300));
        assert!(!session.is_finished(), "still waiting for the shell");
        agent.exited();
        let took = session.join().unwrap();
        assert!(took >= Duration::from_millis(300) && took < END_SESSION_WAIT, "{took:?}");
    }

    /// The recorder lets go as soon as another app is in front, passing the key on.
    #[test]
    fn the_recorder_gives_up_when_another_app_is_in_front() {
        let _front = HOOK_TESTS.lock().unwrap_or_else(PoisonError::into_inner);
        let (tx, rx) = channel();
        let first = obscure(17);
        let Started { agent, .. } = Agent::start(first, false, move |e| tx.send(e).unwrap()).expect("agent window");
        let free = || unsafe {
            let ok = RegisterHotKey(null_mut(), 80, first.mods, first.vk) != 0;
            if ok {
                UnregisterHotKey(null_mut(), 80);
            }
            ok
        };
        agent.record(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !free() {
            assert!(std::time::Instant::now() < deadline, "recording should set the shortcut aside");
            std::thread::sleep(Duration::from_millis(5));
        }
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_LWIN;
        assert_eq!(press(VK_LWIN as u32, false), 0, "Win reaches the app in front");
        assert_eq!(next(&rx), Event::Recorded(None), "reported as cancelled");
        assert!(!free(), "the shortcut is back");
    }
}
