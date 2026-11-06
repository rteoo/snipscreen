//! What the person chose, kept in `%APPDATA%\SnipScreen\settings.ini` as plain `key=value` lines.
//!
//! Reading never fails: a missing file is a first run, and a value that can't be read falls back to
//! its default and is named in `Loaded::problems` so the shell can say so. Writes go to a temporary
//! file that then replaces the old one, so a crash mid-write never leaves a half-written file.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::thread::JoinHandle;

use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_F24, VK_HOME, VK_INSERT, VK_LEFT, VK_NEXT, VK_NUMPAD0,
    VK_NUMPAD9, VK_OEM_1, VK_OEM_2, VK_OEM_3, VK_OEM_4, VK_OEM_5, VK_OEM_6, VK_OEM_7, VK_OEM_COMMA, VK_OEM_MINUS, VK_OEM_PERIOD, VK_OEM_PLUS, VK_PAUSE, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SNAPSHOT, VK_SPACE, VK_TAB, VK_UP,
};

/// A system-wide shortcut as `RegisterHotKey` takes it: `MOD_*` flags and a virtual-key code.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hotkey {
    pub mods: u32,
    pub vk: u32,
}

impl Hotkey {
    pub const DEFAULT: Hotkey = Hotkey { mods: MOD_ALT | MOD_SHIFT, vk: b'S' as u32 };

    /// "Alt+Shift+S", "Win+Ctrl+F12", "Print Screen".
    pub fn label(self) -> String {
        let mut parts: Vec<String> = MODS.iter().filter(|(m, _)| self.mods & m != 0).map(|(_, n)| n.to_string()).collect();
        parts.push(key_name(self.vk).unwrap_or_else(|| format!("Key {:#04x}", self.vk)));
        parts.join("+")
    }

    pub fn parse(s: &str) -> Option<Hotkey> {
        let mut parts: Vec<&str> = s.split('+').map(str::trim).collect();
        let key = parts.pop()?;
        let vk = key_code(key)?;
        let mut mods = 0;
        for p in parts {
            let (m, _) = MODS.iter().find(|(_, n)| n.eq_ignore_ascii_case(p))?;
            if mods & m != 0 {
                return None;
            }
            mods |= m;
        }
        Some(Hotkey { mods, vk })
    }

    /// Whether a shortcut is fit to be global: F-keys and Print Screen may stand alone, anything else
    /// needs Ctrl, Alt or Win, or it would fire while typing (Shift+S is just a capital S).
    pub fn check(self) -> Result<Hotkey, String> {
        if key_name(self.vk).is_none() {
            return Err("That key can't be used in a shortcut".into());
        }
        let alone_ok = (VK_F1 as u32..=VK_F24 as u32).contains(&self.vk) || self.vk == VK_SNAPSHOT as u32;
        if alone_ok || self.mods & (MOD_CONTROL | MOD_ALT | MOD_WIN) != 0 {
            Ok(self)
        } else {
            Err(format!("{} would fire while you type. Add Ctrl, Alt or Win (F-keys and Print Screen work alone).", self.label()))
        }
    }
}

/// Modifier flags in the order they are written.
const MODS: [(u32, &str); 4] = [(MOD_WIN, "Win"), (MOD_CONTROL, "Ctrl"), (MOD_ALT, "Alt"), (MOD_SHIFT, "Shift")];

/// Keys with names of their own; letters, digits, F-keys and the number pad are worked out in `key_name`.
const NAMED_KEYS: [(u16, &str); 28] = [
    (VK_SNAPSHOT, "Print Screen"),
    (VK_SPACE, "Space"),
    (VK_TAB, "Tab"),
    (VK_RETURN, "Enter"),
    (VK_BACK, "Backspace"),
    (VK_ESCAPE, "Esc"),
    (VK_INSERT, "Insert"),
    (VK_DELETE, "Delete"),
    (VK_HOME, "Home"),
    (VK_END, "End"),
    (VK_PRIOR, "Page Up"),
    (VK_NEXT, "Page Down"),
    (VK_LEFT, "Left"),
    (VK_UP, "Up"),
    (VK_RIGHT, "Right"),
    (VK_DOWN, "Down"),
    (VK_PAUSE, "Pause"),
    (VK_OEM_1, ";"),
    (VK_OEM_PLUS, "="),
    (VK_OEM_COMMA, ","),
    (VK_OEM_MINUS, "-"),
    (VK_OEM_PERIOD, "."),
    (VK_OEM_2, "/"),
    (VK_OEM_3, "`"),
    (VK_OEM_4, "["),
    (VK_OEM_5, "\\"),
    (VK_OEM_6, "]"),
    (VK_OEM_7, "'"),
];

pub fn key_name(vk: u32) -> Option<String> {
    let c = char::from_u32(vk)?;
    if c.is_ascii_uppercase() || c.is_ascii_digit() {
        return Some(c.to_string());
    }
    if (VK_F1 as u32..=VK_F24 as u32).contains(&vk) {
        return Some(format!("F{}", vk - VK_F1 as u32 + 1));
    }
    if (VK_NUMPAD0 as u32..=VK_NUMPAD9 as u32).contains(&vk) {
        return Some(format!("Num {}", vk - VK_NUMPAD0 as u32));
    }
    NAMED_KEYS.iter().find(|(k, _)| *k as u32 == vk).map(|(_, n)| n.to_string())
}

fn key_code(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    if up.len() == 1 {
        let c = up.as_bytes()[0];
        if c.is_ascii_uppercase() || c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(n) = up.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        return (1..=24).contains(&n).then(|| VK_F1 as u32 + n - 1);
    }
    if let Some(n) = up.strip_prefix("NUM ").and_then(|n| n.parse::<u32>().ok()) {
        return (n <= 9).then(|| VK_NUMPAD0 as u32 + n);
    }
    NAMED_KEYS.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)).map(|(k, _)| *k as u32)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AfterCapture {
    /// A Windows notification with a preview; clicking it opens the editor.
    Notify,
    /// Straight into an editor window.
    Editor,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Photo,
    Video,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shape {
    Rectangle,
    Window,
    FullScreen,
    Freeform,
}

pub const DELAYS: [u64; 4] = [0, 3, 5, 10];

#[derive(Clone, PartialEq, Debug)]
pub struct Settings {
    pub shortcut: Hotkey,
    pub after_capture: AfterCapture,
    pub auto_copy: bool,
    pub auto_save: bool,
    pub start_with_windows: bool,
    pub mode: Mode,
    pub shape: Shape,
    pub delay: u64,
    pub record_system: bool,
    pub record_mic: bool,
    /// `None` is the Windows default microphone.
    pub mic: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            shortcut: Hotkey::DEFAULT,
            after_capture: AfterCapture::Notify,
            auto_copy: true,
            auto_save: true,
            start_with_windows: true,
            mode: Mode::Photo,
            shape: Shape::Rectangle,
            delay: 0,
            record_system: true,
            record_mic: false,
            mic: None,
        }
    }
}

fn word<T: Copy + PartialEq>(table: &[(T, &'static str)], v: T) -> &'static str {
    table.iter().find(|(t, _)| *t == v).map_or("", |(_, w)| w)
}

fn from_word<T: Copy>(table: &[(T, &str)], w: &str) -> Option<T> {
    table.iter().find(|(_, s)| s.eq_ignore_ascii_case(w)).map(|(t, _)| *t)
}

const AFTER: [(AfterCapture, &str); 2] = [(AfterCapture::Notify, "notify"), (AfterCapture::Editor, "editor")];
const MODES: [(Mode, &str); 2] = [(Mode::Photo, "photo"), (Mode::Video, "video")];
const SHAPES: [(Shape, &str); 4] = [(Shape::Rectangle, "rectangle"), (Shape::Window, "window"), (Shape::FullScreen, "fullscreen"), (Shape::Freeform, "freeform")];

fn bool_word(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

fn parse_bool(s: &str) -> Option<bool> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl Settings {
    pub fn serialize(&self) -> String {
        let lines = [
            "# SnipScreen settings. Edit while SnipScreen is closed; unreadable values reset to their defaults.".to_string(),
            format!("shortcut={}", self.shortcut.label()),
            format!("after_capture={}", word(&AFTER, self.after_capture)),
            format!("auto_copy={}", bool_word(self.auto_copy)),
            format!("auto_save={}", bool_word(self.auto_save)),
            format!("start_with_windows={}", bool_word(self.start_with_windows)),
            format!("mode={}", word(&MODES, self.mode)),
            format!("shape={}", word(&SHAPES, self.shape)),
            format!("delay={}", self.delay),
            format!("record_system={}", bool_word(self.record_system)),
            format!("record_mic={}", bool_word(self.record_mic)),
            format!("mic={}", self.mic.as_deref().unwrap_or("")),
        ];
        lines.join("\r\n") + "\r\n"
    }

    /// Reads `text`, keeping the default for every value that is missing or unreadable. Returns the
    /// names of the unreadable ones. Unknown keys are ignored, so a newer version's file still loads.
    pub fn parse(text: &str) -> (Settings, Vec<String>) {
        let mut s = Settings::default();
        let mut bad = Vec::new();
        // Notepad may save the file with a byte-order mark in front of the first line.
        for line in text.trim_start_matches('\u{feff}').lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                bad.push(line.chars().take(40).collect());
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            let ok = match key {
                "shortcut" => Hotkey::parse(value).and_then(|h| h.check().ok()).map(|h| s.shortcut = h).is_some(),
                "after_capture" => from_word(&AFTER, value).map(|v| s.after_capture = v).is_some(),
                "auto_copy" => parse_bool(value).map(|v| s.auto_copy = v).is_some(),
                "auto_save" => parse_bool(value).map(|v| s.auto_save = v).is_some(),
                "start_with_windows" => parse_bool(value).map(|v| s.start_with_windows = v).is_some(),
                "mode" => from_word(&MODES, value).map(|v| s.mode = v).is_some(),
                "shape" => from_word(&SHAPES, value).map(|v| s.shape = v).is_some(),
                "delay" => value.parse().ok().filter(|d| DELAYS.contains(d)).map(|v| s.delay = v).is_some(),
                "record_system" => parse_bool(value).map(|v| s.record_system = v).is_some(),
                "record_mic" => parse_bool(value).map(|v| s.record_mic = v).is_some(),
                "mic" => {
                    s.mic = (!value.is_empty()).then(|| value.to_string());
                    true
                }
                _ => true,
            };
            if !ok {
                bad.push(key.to_string());
            }
        }
        // A video can't be freeform; the shell would fix it silently, so the file is fixed here too.
        if s.mode == Mode::Video && s.shape == Shape::Freeform {
            s.shape = Shape::Rectangle;
        }
        (s, bad)
    }
}

/// The settings file as found at startup.
pub struct Loaded {
    pub settings: Settings,
    /// Values that couldn't be read and were reset, or why the file couldn't be read at all.
    pub problems: Vec<String>,
    /// False on a first run: nothing was saved yet.
    pub existed: bool,
    /// Where changes are saved; `None` when they must not be (no AppData folder, or a file that is
    /// there but couldn't be read, which a save would overwrite with defaults).
    pub path: Option<PathBuf>,
}

pub fn path() -> Option<PathBuf> {
    crate::win::roaming_app_data_dir().map(|d| d.join("SnipScreen").join("settings.ini"))
}

pub fn load() -> Loaded {
    match path() {
        Some(path) => load_from(path),
        None => Loaded { settings: Settings::default(), problems: vec!["couldn't find your AppData folder, so settings won't be kept".into()], existed: false, path: None },
    }
}

fn load_from(path: PathBuf) -> Loaded {
    match std::fs::read(&path) {
        // A stray non-UTF-8 byte (an editor saving in the ANSI code page) costs only the value it is in.
        Ok(bytes) => {
            let (settings, problems) = Settings::parse(&String::from_utf8_lossy(&bytes));
            Loaded { settings, problems, existed: true, path: Some(path) }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Loaded { settings: Settings::default(), problems: Vec::new(), existed: false, path: Some(path) },
        // Locked by another program, or no permission: the file is still there and must survive.
        Err(e) => Loaded {
            settings: Settings::default(),
            problems: vec![format!("couldn't read {} ({e}), so changes won't be saved until SnipScreen starts again", path.display())],
            existed: true,
            path: None,
        },
    }
}

/// Writes `settings` to `path` through a temporary file in the same folder, renamed over the old one.
pub fn save(path: &Path, settings: &Settings) -> Result<(), String> {
    use std::io::Write;
    let fail = |e: std::io::Error| format!("couldn't save settings to {}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(fail)?;
    }
    let tmp = path.with_extension("ini.tmp");
    let mut file = std::fs::File::create(&tmp).map_err(fail)?;
    file.write_all(settings.serialize().as_bytes()).and_then(|_| file.sync_all()).map_err(fail)?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(fail)
}

/// Saves on its own thread, so a slow disk never stalls a frame. Snapshots that queue up while a write
/// runs collapse into the newest one. Dropping it writes whatever is still queued, then stops.
pub struct Saver {
    tx: Option<Sender<Settings>>,
    thread: Option<JoinHandle<()>>,
}

impl Saver {
    pub fn new(path: Option<PathBuf>, on_error: impl Fn(String) + Send + 'static) -> Self {
        let Some(path) = path else { return Self { tx: None, thread: None } };
        let (tx, rx) = channel::<Settings>();
        let thread = std::thread::spawn(move || {
            while let Ok(mut s) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    s = newer;
                }
                if let Err(e) = save(&path, &s) {
                    on_error(e);
                }
            }
        });
        Self { tx: Some(tx), thread: Some(thread) }
    }

    pub fn save(&self, s: &Settings) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(s.clone());
        }
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let s = Settings::default();
        assert_eq!(Settings::parse(&s.serialize()), (s, vec![]));
    }

    #[test]
    fn every_choice_round_trips() {
        let s = Settings {
            shortcut: Hotkey { mods: MOD_WIN | MOD_CONTROL, vk: VK_F1 as u32 + 11 },
            after_capture: AfterCapture::Editor,
            auto_copy: false,
            auto_save: false,
            start_with_windows: false,
            mode: Mode::Video,
            shape: Shape::FullScreen,
            delay: 10,
            record_system: false,
            record_mic: true,
            mic: Some("{0.0.1.00000000}.{a=b c}".into()),
        };
        let text = s.serialize();
        assert!(text.contains("shortcut=Win+Ctrl+F12\r\n"), "{text}");
        assert_eq!(Settings::parse(&text), (s, vec![]));
    }

    #[test]
    fn bad_values_fall_back_one_by_one() {
        let text = "shortcut=Hyper+Q\nafter_capture=popup\nauto_copy=maybe\nauto_save=false\ndelay=7\nshape=circle\njunk line\nfuture_key=1\n";
        let (s, bad) = Settings::parse(text);
        assert_eq!(bad, ["shortcut", "after_capture", "auto_copy", "delay", "shape", "junk line"]);
        assert_eq!(s, Settings { auto_save: false, ..Settings::default() }, "the readable value is kept, the rest are defaults");
    }

    #[test]
    fn garbage_and_empty_files_load_defaults_without_panicking() {
        assert_eq!(Settings::parse(""), (Settings::default(), vec![]));
        let (s, bad) = Settings::parse("\u{0}=\n==\n=x\nshortcut=\nshortcut=+\nshortcut=Ctrl+Ctrl+S\n");
        assert_eq!(s, Settings::default());
        assert_eq!(bad, ["shortcut", "shortcut", "shortcut"], "empty and unknown keys are skipped");
        let edited = Settings { auto_save: false, ..Settings::default() };
        let notepad = format!("\u{feff}{}", edited.serialize());
        assert_eq!(Settings::parse(&notepad), (edited, vec![]), "a byte-order mark is not a problem");
    }

    #[test]
    fn a_shortcut_that_fires_while_typing_is_refused_in_the_file() {
        let (s, bad) = Settings::parse("shortcut=Shift+S\n");
        assert_eq!((s.shortcut, bad), (Hotkey::DEFAULT, vec!["shortcut".to_string()]));
    }

    #[test]
    fn video_with_freeform_is_corrected() {
        assert_eq!(Settings::parse("mode=video\nshape=freeform\n").0.shape, Shape::Rectangle);
    }

    #[test]
    fn hotkey_labels_round_trip() {
        for (label, hk) in [
            ("Alt+Shift+S", Hotkey::DEFAULT),
            ("Print Screen", Hotkey { mods: 0, vk: VK_SNAPSHOT as u32 }),
            ("Win+Ctrl+Alt+Shift+F24", Hotkey { mods: MOD_WIN | MOD_CONTROL | MOD_ALT | MOD_SHIFT, vk: VK_F24 as u32 }),
            ("Ctrl+Num 7", Hotkey { mods: MOD_CONTROL, vk: VK_NUMPAD0 as u32 + 7 }),
            ("Ctrl+Alt+Page Down", Hotkey { mods: MOD_CONTROL | MOD_ALT, vk: VK_NEXT as u32 }),
            ("Ctrl+]", Hotkey { mods: MOD_CONTROL, vk: VK_OEM_6 as u32 }),
            ("Alt+5", Hotkey { mods: MOD_ALT, vk: b'5' as u32 }),
        ] {
            assert_eq!(hk.label(), label);
            assert_eq!(Hotkey::parse(label), Some(hk), "{label}");
        }
        assert_eq!(Hotkey::parse("ctrl + alt + p"), Some(Hotkey { mods: MOD_CONTROL | MOD_ALT, vk: b'P' as u32 }));
        assert_eq!(Hotkey::parse("F25"), None);
        assert_eq!(Hotkey::parse("Ctrl+"), None);
    }

    #[test]
    fn only_f_keys_and_print_screen_may_stand_alone() {
        let alone = |vk: u16| Hotkey { mods: 0, vk: vk as u32 }.check();
        assert!(alone(VK_SNAPSHOT).is_ok());
        assert!(alone(VK_F1 + 8).is_ok());
        assert!(alone(b'S' as u16).is_err());
        assert!(Hotkey { mods: MOD_SHIFT, vk: b'S' as u32 }.check().unwrap_err().contains("Shift+S would fire while you type"));
        assert!(Hotkey { mods: MOD_SHIFT, vk: VK_SNAPSHOT as u32 }.check().is_ok());
        assert!(Hotkey { mods: MOD_WIN, vk: b'Q' as u32 }.check().is_ok());
        assert!(Hotkey { mods: MOD_CONTROL, vk: 0xE5 }.check().is_err(), "keys without a name are refused");
    }

    #[test]
    fn a_file_with_stray_bytes_keeps_its_readable_values() {
        let dir = std::env::temp_dir().join(format!("snipscreen-lossy-{}", std::process::id()));
        let path = dir.join("settings.ini");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"auto_save=false\r\nmic=Microfone (Realtek\xae)\r\ndelay=5\r\n").unwrap();
        let loaded = load_from(path.clone());
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((loaded.settings.auto_save, loaded.settings.delay), (false, 5));
        assert_eq!(loaded.path, Some(path), "still saved to");
    }

    #[test]
    fn a_file_that_cant_be_read_is_never_overwritten() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("snipscreen-locked-{}", std::process::id()));
        let path = dir.join("settings.ini");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "auto_save=false\r\n").unwrap();
        // Another program holding the file without sharing it: a sharing violation on read.
        let lock = std::fs::OpenOptions::new().read(true).share_mode(0).open(&path).unwrap();
        let loaded = load_from(path.clone());
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.path, None, "saving is off for the session");
        assert!(loaded.problems[0].contains("won't be saved"), "{:?}", loaded.problems);
    }

    #[test]
    fn save_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("snipscreen-settings-{}", std::process::id()));
        let path = dir.join("settings.ini");
        let s = Settings { auto_copy: false, ..Settings::default() };
        let first = save(&path, &Settings::default());
        let second = save(&path, &s);
        let read = std::fs::read_to_string(&path);
        let leftovers = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!((first, second), (Ok(()), Ok(())));
        assert_eq!(Settings::parse(&read.unwrap()).0, s);
        assert_eq!(leftovers, 1, "the temporary file is gone");
    }

    #[test]
    fn saver_writes_the_newest_snapshot_before_it_stops() {
        let dir = std::env::temp_dir().join(format!("snipscreen-saver-{}", std::process::id()));
        let path = dir.join("settings.ini");
        let saver = Saver::new(Some(path.clone()), |e| panic!("{e}"));
        for delay in DELAYS {
            saver.save(&Settings { delay, ..Settings::default() });
        }
        drop(saver);
        let read = std::fs::read_to_string(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(Settings::parse(&read.unwrap()).0.delay, 10);
    }
}
