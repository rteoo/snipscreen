//! The resident side of SnipScreen: one thread with a hidden window that owns the tray icon and
//! its menu, the global shortcuts, and takes requests from later launches.
//!
//! The window is a plain top-level one that is never shown (not a message-only window, which
//! would miss the `TaskbarCreated` broadcast). It blocks in `GetMessageW`, so it costs nothing
//! while idle. Everything it hears goes to the shell through one callback as an [`Event`].

use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::mpsc::channel;
use std::thread::JoinHandle;

use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::COPYDATASTRUCT;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, MOD_NOREPEAT, VK_ESCAPE};
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW,
    GetMessageW, GetSystemMetrics, GetWindowLongPtrW, GetWindowThreadProcessId, KillTimer, LoadImageW, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SendMessageTimeoutW, SetForegroundWindow, SetMenuDefaultItem, SetTimer, SetWindowLongPtrW, TrackPopupMenuEx, TranslateMessage,
    GWLP_USERDATA, HICON, IMAGE_ICON, LR_DEFAULTCOLOR, MF_SEPARATOR, MF_STRING, MSG, SMTO_ABORTIFHUNG, SM_CXSMICON, SM_CYSMICON, TPM_BOTTOMALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_COPYDATA, WM_DESTROY, WM_ENDSESSION, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_NULL, WM_QUERYENDSESSION, WM_TIMER,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::settings::Hotkey;

const CLASS: &str = "SnipScreen.Agent";
const MUTEX: &str = "Local\\SnipScreen.Instance";
/// Tags our `WM_COPYDATA` so a stray one from another program is ignored ("SNIP").
const COPYDATA_TAG: usize = 0x534e_4950;

const HOTKEY_ESCAPE: i32 = 2;
/// The snip shortcut alternates between two ids, so a new one is registered before the old one is let go.
const HOTKEY_SNIP: [i32; 2] = [1, 3];

const MSG_ARM_ESCAPE: u32 = WM_APP + 1;
const MSG_DISARM_ESCAPE: u32 = WM_APP + 2;
const MSG_SET_HOTKEY: u32 = WM_APP + 3;
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
const MENU_QUIT: usize = 4;

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attributes: *const c_void, initial_owner: i32, name: *const u16) -> HANDLE;
    fn GetModuleHandleW(name: *const u16) -> HMODULE;
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
    /// A `snipscreen:` link, from a click on a notification.
    Open(String),
}

impl Intent {
    pub fn from_args<S: AsRef<str>>(args: &[S]) -> Intent {
        for a in args.iter().map(AsRef::as_ref) {
            match a {
                "--tray" => return Intent::Tray,
                "--quit" => return Intent::Quit,
                _ if a.len() > 11 && a[..11].eq_ignore_ascii_case("snipscreen:") => return Intent::Open(a.to_string()),
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
}

/// True when this is the only running copy. The mutex lives as long as the process.
pub fn claim_single_instance() -> bool {
    let name = wide(MUTEX);
    unsafe {
        let handle = CreateMutexW(null(), 0, name.as_ptr());
        // Without a mutex there is no way to tell; running is better than refusing to start.
        handle.is_null() || GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Hands `intent` to the copy that is already running, waiting up to ~3 s for its window (it may
/// still be starting) and letting it take the foreground, which this launch was given.
pub fn forward(intent: &Intent) -> Result<(), String> {
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
            return if sent != 0 && answer == 1 { Ok(()) } else { Err("SnipScreen is already running but didn't answer".into()) };
        }
    }
    Err("SnipScreen is already starting but its window never appeared".into())
}

/// Handle to the agent thread. Dropping it unregisters the shortcuts and ends the thread.
pub struct Agent {
    /// The hidden window; `HWND` itself is not `Send`.
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
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
}

impl Agent {
    /// Starts the thread and its window, registers `hotkey` and, with `tray`, adds the tray icon.
    /// Fails only if the window can't be made.
    pub fn start(hotkey: Hotkey, tray: bool, on_event: impl Fn(Event) + Send + 'static) -> Result<Started, String> {
        let (ready_tx, ready_rx) = channel();
        let thread = std::thread::spawn(move || unsafe {
            let hwnd = match create_window() {
                Ok(h) => h,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let first = register(hwnd, HOTKEY_SNIP[0], hotkey);
            let instance = GetModuleHandleW(null());
            let icon = LoadImageW(instance, ICON_RESOURCE as *const u16, IMAGE_ICON, GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON), LR_DEFAULTCOLOR) as HICON;
            let taskbar_created = RegisterWindowMessageW(wide("TaskbarCreated").as_ptr());
            let state = Box::new(State {
                on_event: Box::new(on_event),
                hotkey: first.as_ref().ok().copied(),
                slot: 0,
                icon,
                tray_added: false,
                tray_retries: 0,
                taskbar_created,
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
            Ok(Ok((hwnd, hotkey))) => Ok(Started { agent: Agent { hwnd, thread: Some(thread) }, hotkey }),
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
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set_hotkey(&self, hotkey: Hotkey) {
        self.post(MSG_SET_HOTKEY, hotkey.mods as WPARAM, hotkey.vk as LPARAM);
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        self.post(WM_CLOSE, 0, 0);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe fn create_window() -> Result<HWND, String> {
    let class = wide(CLASS);
    let instance = GetModuleHandleW(null());
    let wc = WNDCLASSEXW { cbSize: size_of::<WNDCLASSEXW>() as u32, lpfnWndProc: Some(agent_proc), hInstance: instance, lpszClassName: class.as_ptr(), ..zeroed() };
    if RegisterClassExW(&wc) == 0 {
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

/// Puts the icon in the notification area. Right after sign-in the taskbar may not exist yet, so a
/// failure is retried on a timer for a while; Explorer's `TaskbarCreated` also re-adds it.
unsafe fn add_tray_icon(hwnd: HWND, st: &mut State) {
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
        WM_COPYDATA => {
            let data = &*(l as *const COPYDATASTRUCT);
            if data.dwData != COPYDATA_TAG || data.lpData.is_null() {
                return 0;
            }
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2));
            return match Intent::decode(&text) {
                Some(intent) => {
                    (st.on_event)(Event::Intent(intent));
                    1
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
                    // The menu's loop ran this procedure again; borrow the state afresh.
                    let st = &mut *state;
                    match chosen {
                        MENU_NEW => (st.on_event)(Event::Snip),
                        MENU_OPEN => (st.on_event)(Event::Intent(Intent::Show)),
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
        // Only when an icon was asked for: one that was added, or one still being retried.
        _ if msg == st.taskbar_created && st.taskbar_created != 0 && (st.tray_added || st.tray_retries > 0) => {
            st.tray_retries = 0;
            add_tray_icon(hwnd, st);
        }
        WM_QUERYENDSESSION => return 1,
        // Windows is signing out, or the installer's Restart Manager wants the file: leave cleanly.
        WM_ENDSESSION if w != 0 => (st.on_event)(Event::Intent(Intent::Quit)),
        WM_CLOSE => {
            DestroyWindow(hwnd);
        }
        WM_DESTROY => {
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
        assert_eq!(Intent::from_args(&["SnipScreen:open?id=4"]), Intent::Open("SnipScreen:open?id=4".into()));
        assert_eq!(Intent::from_args(&["snipscreen:"]), Intent::Show, "a bare scheme opens nothing");
        assert_eq!(Intent::from_args(&["--unknown", "--tray"]), Intent::Tray);
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
}
