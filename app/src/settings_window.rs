//! The small Settings window: the snip shortcut (with a recorder), what happens after a snip, where
//! snips and recordings are saved, and starting with Windows.
//!
//! Like the editors it is a deferred viewport behind a mutex, so it repaints only for its own input.
//! It never touches the OS itself: it edits its copy of the settings and raises flags that the shell
//! collects in `poll`, and the shell writes back what the agent answered (`hotkey_result`).

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui::{self, vec2, Align, Color32, Layout, RichText, Sense, StrokeKind, ViewportBuilder, ViewportCommand, ViewportId};

use crate::icons::Icon;
use crate::settings::{self, AfterCapture, Hotkey, Settings};
use crate::widgets::{self, ACCENT, BUTTON_BG, ICON, OUTLINE};
use crate::win;

const SIZE: egui::Vec2 = vec2(480.0, 495.0);
const TITLE: &str = "SnipScreen Settings";

/// The two save folders.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Folder {
    Screenshots,
    Recordings,
}

/// The folder picker's answer, from its worker.
type Picked = Result<Option<PathBuf>, String>;
const ERROR: Color32 = Color32::from_rgb(0xff, 0x99, 0xa4);

/// What the window holds between frames.
struct View {
    /// The window's copy; `changed` says the shell should take it.
    settings: Settings,
    changed: bool,
    /// The shortcut the agent has registered, if any.
    active: Option<Hotkey>,
    /// Waiting for the person to press a new shortcut.
    recording: bool,
    /// Start (`true`) or stop the agent's key recorder; taken by `poll`.
    record_request: Option<bool>,
    /// The last word on the shortcut, and whether it is a complaint.
    status: Option<(String, bool)>,
    /// The folder picker open for one of the folders, answering on this channel.
    picking: Option<(Folder, Receiver<Picked>)>,
    /// Why the last folder couldn't be chosen.
    folder_error: Option<String>,
    closed: bool,
}

/// What the window asked of the shell since the last `poll`.
#[derive(Default, Debug, PartialEq)]
pub struct Requests {
    /// The edited settings, when anything but the shortcut changed.
    pub settings: Option<Settings>,
    /// Start or stop recording a shortcut.
    pub record: Option<bool>,
    pub closed: bool,
}

pub struct SettingsWindow {
    id: ViewportId,
    builder: ViewportBuilder,
    view: Arc<Mutex<View>>,
}

impl SettingsWindow {
    pub fn new(ctx: &egui::Context, settings: &Settings, active: Option<Hotkey>, note: Option<&str>, icon: Option<Arc<egui::IconData>>) -> Self {
        let mut builder = ViewportBuilder::default().with_title(TITLE).with_inner_size(SIZE).with_min_inner_size(SIZE).with_resizable(false).with_maximize_button(false);
        if let Some(icon) = icon {
            builder = builder.with_icon(icon);
        }
        let status = note.map(|n| (n.to_string(), true));
        let view = View { settings: settings.clone(), changed: false, active, recording: false, record_request: None, status, picking: None, folder_error: None, closed: false };
        let id = ViewportId::from_hash_of("snipscreen settings");
        ctx.request_repaint_of(id);
        Self { id, builder, view: Arc::new(Mutex::new(view)) }
    }

    pub fn id(&self) -> ViewportId {
        self.id
    }

    /// Brings an open window to the front.
    pub fn focus(&self, ctx: &egui::Context) {
        for cmd in [ViewportCommand::Minimized(false), ViewportCommand::Visible(true), ViewportCommand::Focus] {
            ctx.send_viewport_cmd_to(self.id, cmd);
        }
    }

    /// Registers the window for this frame; must run on every pass of the root window.
    pub fn show(&self, ctx: &egui::Context) {
        let view = self.view.clone();
        ctx.show_viewport_deferred(self.id, self.builder.clone(), move |ui, _| {
            view.lock().unwrap_or_else(PoisonError::into_inner).ui(ui);
        });
    }

    pub fn poll(&self) -> Requests {
        // Only the UI thread runs the window and it never re-enters it, so the lock is free.
        let Ok(mut v) = self.view.try_lock() else { return Requests::default() };
        let settings = std::mem::take(&mut v.changed).then(|| v.settings.clone());
        Requests { settings, record: v.record_request.take(), closed: v.closed }
    }

    fn with(&self, ctx: &egui::Context, f: impl FnOnce(&mut View)) {
        f(&mut self.view.lock().unwrap_or_else(PoisonError::into_inner));
        ctx.request_repaint_of(self.id);
    }

    /// The recorder caught a combination (`None`: cancelled) and the shell judged it.
    pub fn recorded(&self, ctx: &egui::Context, verdict: Option<Result<Hotkey, String>>) {
        self.with(ctx, |v| {
            v.recording = false;
            v.status = match verdict {
                None => None,
                Some(Ok(h)) => Some((format!("Setting {}…", h.label()), false)),
                Some(Err(e)) => Some((e, true)),
            };
        });
    }

    /// The agent's answer to a new shortcut: in force, or refused (the old one stays).
    pub fn hotkey_result(&self, ctx: &egui::Context, result: &Result<Hotkey, String>, active: Option<Hotkey>) {
        self.with(ctx, |v| {
            v.active = active;
            if let Some(h) = active {
                v.settings.shortcut = h;
            }
            v.status = Some(match (result, active) {
                (Ok(h), _) => (format!("{} now starts a new snip from anywhere", h.label()), false),
                (Err(e), Some(old)) => (format!("{e}. Keeping {}.", old.label()), true),
                (Err(e), None) => (e.clone(), true),
            });
        });
    }
}

impl View {
    fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.closed {
            self.closed = true;
            self.stop_recording();
            ctx.request_repaint_of(ViewportId::ROOT);
        }
        // The recorder hears every key on the desktop, so it only listens while this window has focus.
        if self.recording && ctx.input(|i| i.viewport().focused) == Some(false) {
            self.stop_recording();
            ctx.request_repaint_of(ViewportId::ROOT);
        }
        let before = self.settings.clone();
        self.take_picked();
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ui.style().visuals.panel_fill).inner_margin(20)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            self.shortcut_row(ui);
            ui.add_space(10.0);
            ui.label(RichText::new("After a snip").strong());
            ui.radio_value(&mut self.settings.after_capture, AfterCapture::Notify, "Show a notification with a preview");
            ui.radio_value(&mut self.settings.after_capture, AfterCapture::Editor, "Open the editor");
            ui.checkbox(&mut self.settings.auto_copy, "Copy to clipboard automatically");
            let save = match self.settings.screenshots() {
                Some(dir) => format!("Save to {} automatically", settings::short_dir(&dir)),
                None => "Save to the screenshots folder automatically".to_string(),
            };
            ui.checkbox(&mut self.settings.auto_save, save);
            ui.add_space(10.0);
            ui.label(RichText::new("Save locations").strong());
            self.folder_row(ui, Folder::Screenshots);
            self.folder_row(ui, Folder::Recordings);
            if let Some(e) = &self.folder_error {
                ui.label(RichText::new(e).small().color(ERROR));
            }
            ui.add_space(10.0);
            ui.label(RichText::new("Startup").strong());
            ui.checkbox(&mut self.settings.start_with_windows, "Start SnipScreen with Windows (in the tray)");
        });
        if self.settings != before {
            self.changed = true;
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    fn shortcut_row(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.label(RichText::new("Shortcut for a new snip").strong());
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(240.0, 32.0), Sense::hover());
            let (text, color, stroke) = if self.recording {
                ("Press the new shortcut…".to_string(), ACCENT, ACCENT)
            } else {
                match self.active {
                    Some(h) => (h.label(), ICON, OUTLINE),
                    None => ("None".to_string(), ICON, OUTLINE),
                }
            };
            ui.painter().rect(rect, 4.0, BUTTON_BG, (1.0, stroke), StrokeKind::Inside);
            ui.painter().text(rect.left_center() + vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(14.0), color);
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if self.recording {
                    if ui.button("Cancel").clicked() {
                        self.stop_recording();
                        ctx.request_repaint_of(ViewportId::ROOT);
                    }
                } else if ui.button("Change").clicked() {
                    self.recording = true;
                    self.record_request = Some(true);
                    self.status = None;
                    ctx.request_repaint_of(ViewportId::ROOT);
                }
            });
        });
        let (line, error) = match (&self.status, self.recording) {
            (_, true) => ("Hold Ctrl, Alt, Shift or Win and press a key. F-keys and Print Screen work alone. Esc cancels.".to_string(), false),
            (Some((s, e)), false) => (s.clone(), *e),
            (None, false) => ("Works from any app, even while SnipScreen is in the tray.".to_string(), false),
        };
        let text = RichText::new(line).small();
        ui.label(if error { text.color(ERROR) } else { text.weak() });
    }

    /// One save folder: its name, the folder (the start of a long path elided), Change… and, when it
    /// isn't the default, a button back to the default.
    fn folder_row(&mut self, ui: &mut egui::Ui, which: Folder) {
        let (label, chosen, default) = match which {
            Folder::Screenshots => ("Screenshots", self.settings.screenshots_dir.clone(), settings::default_screenshots()),
            Folder::Recordings => ("Screen recordings", self.settings.recordings_dir.clone(), settings::default_recordings()),
        };
        let shown = chosen.clone().or(default.clone());
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(118.0, 30.0), Sense::hover());
            ui.painter().text(rect.left_center(), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(14.0), ICON);
            let (rect, resp) = ui.allocate_exact_size(vec2(190.0, 30.0), Sense::hover());
            ui.painter().rect(rect, 4.0, BUTTON_BG, (1.0, OUTLINE), StrokeKind::Inside);
            let text = shown.as_ref().map_or_else(|| "Not found".to_string(), |p| p.display().to_string());
            let font = egui::FontId::proportional(13.0);
            let fitted = elide_start(&text, rect.width() - 20.0, |s| ui.painter().layout_no_wrap(s.to_string(), font.clone(), ICON).size().x);
            ui.painter().text(rect.left_center() + vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, fitted, font, ICON);
            if shown.is_some() {
                resp.on_hover_text(text);
            }
            let picking = self.picking.is_some();
            if ui.add_enabled(!picking, egui::Button::new("Change…").min_size(vec2(0.0, 30.0))).clicked() {
                self.pick(ui.ctx(), which, shown.clone());
            }
            if chosen.is_some() {
                let back = default.as_ref().map_or_else(|| "Back to the default folder".to_string(), |d| format!("Back to {}", settings::short_dir(d)));
                if widgets::icon_button_sized(ui, widgets::Scale { btn: vec2(28.0, 30.0), ..widgets::FULL }, Icon::Undo, None, false, !picking, &back).clicked() {
                    self.set_folder(which, None);
                }
            }
        });
    }

    /// Opens the folder picker on a worker (it is modal to this window, and waits for the person).
    fn pick(&mut self, ctx: &egui::Context, which: Folder, start: Option<PathBuf>) {
        let owner = win::find_window(TITLE).map_or(0, |h| h as isize);
        let (tx, rx) = channel();
        let (ctx, id) = (ctx.clone(), ctx.viewport_id());
        let title = match which {
            Folder::Screenshots => "Where should snips be saved?",
            Folder::Recordings => "Where should recordings be saved?",
        };
        std::thread::spawn(move || {
            let _ = tx.send(win::pick_folder(owner as windows_sys::Win32::Foundation::HWND, start.as_deref(), title));
            ctx.request_repaint_of(id);
        });
        self.picking = Some((which, rx));
        self.folder_error = None;
    }

    /// Takes the picker's answer, once it has one.
    fn take_picked(&mut self) {
        let Some((which, rx)) = &self.picking else { return };
        let which = *which;
        let answer = match rx.try_recv() {
            Ok(a) => a,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the folder picker stopped".into()),
        };
        self.picking = None;
        match answer {
            Ok(Some(dir)) => self.set_folder(which, Some(dir)),
            Ok(None) => {}
            Err(e) => self.folder_error = Some(format!("Couldn't choose a folder: {e}")),
        }
    }

    /// Sets a folder; the default one is kept as `None`, so it follows a moved Pictures or Videos.
    fn set_folder(&mut self, which: Folder, dir: Option<PathBuf>) {
        let (slot, default) = match which {
            Folder::Screenshots => (&mut self.settings.screenshots_dir, settings::default_screenshots()),
            Folder::Recordings => (&mut self.settings.recordings_dir, settings::default_recordings()),
        };
        *slot = dir.filter(|d| Some(d) != default.as_ref());
    }

    fn stop_recording(&mut self) {
        if self.recording {
            self.recording = false;
            self.record_request = Some(false);
        }
    }
}

/// `text` cut from the front, with an ellipsis, until `width` of it fits: a path's end names the folder.
fn elide_start(text: &str, width: f32, measure: impl Fn(&str) -> f32) -> String {
    if measure(text) <= width {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    for skip in 1..chars.len() {
        let s = format!("…{}", chars[skip..].iter().collect::<String>());
        if measure(&s) <= width {
            return s;
        }
    }
    "…".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One root pass; headless, egui runs the deferred window inside it. `focused` is the window's focus.
    fn frame(ctx: &egui::Context, w: &SettingsWindow, focused: bool) {
        let mut input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, SIZE)), ..Default::default() };
        input.viewports.insert(ViewportId::ROOT, egui::ViewportInfo { focused: Some(focused), ..Default::default() });
        let mut out = ctx.run_ui(input, |ui| w.show(ui.ctx()));
        out.textures_delta.clear();
    }

    #[test]
    fn edits_reach_the_shell_once() {
        let ctx = egui::Context::default();
        let w = SettingsWindow::new(&ctx, &Settings::default(), Some(Hotkey::DEFAULT), None, None);
        frame(&ctx, &w, true);
        assert_eq!(w.poll(), Requests::default(), "drawing alone changes nothing");
        w.with(&ctx, |v| {
            v.settings.auto_save = false;
            v.changed = true;
        });
        assert_eq!(w.poll().settings.map(|s| s.auto_save), Some(false));
        assert_eq!(w.poll().settings, None, "taken once");
    }

    #[test]
    fn the_recorder_stops_when_the_window_loses_focus() {
        let ctx = egui::Context::default();
        let w = SettingsWindow::new(&ctx, &Settings::default(), Some(Hotkey::DEFAULT), None, None);
        w.with(&ctx, |v| {
            v.recording = true;
            v.record_request = Some(true);
        });
        assert_eq!(w.poll().record, Some(true));
        frame(&ctx, &w, true);
        assert_eq!(w.poll().record, None, "still recording while focused");
        frame(&ctx, &w, false);
        assert_eq!(w.poll().record, Some(false), "the desktop-wide recorder stops once the window loses focus");
        assert!(!w.view.lock().unwrap().recording);
    }

    #[test]
    fn a_long_path_keeps_its_end() {
        let measure = |s: &str| s.chars().count() as f32;
        assert_eq!(elide_start(r"D:\Snips", 20.0, measure), r"D:\Snips");
        assert_eq!(elide_start(r"C:\Users\someone\Pictures\Screenshots", 22.0, measure), r"…\Pictures\Screenshots");
    }

    #[test]
    fn a_chosen_folder_reaches_the_shell_and_the_default_is_kept_as_none() {
        let ctx = egui::Context::default();
        let w = SettingsWindow::new(&ctx, &Settings::default(), Some(Hotkey::DEFAULT), None, None);
        // What the picker would answer, as its worker sends it.
        let answer = |which: Folder, a: Picked| {
            let (tx, rx) = channel();
            tx.send(a).unwrap();
            w.with(&ctx, |v| v.picking = Some((which, rx)));
            frame(&ctx, &w, true);
        };
        answer(Folder::Recordings, Ok(Some(PathBuf::from(r"D:\Recordings"))));
        let s = w.poll().settings.expect("changed");
        assert_eq!((s.recordings_dir, s.screenshots_dir.clone()), (Some(PathBuf::from(r"D:\Recordings")), None));
        answer(Folder::Screenshots, Ok(None));
        assert_eq!(w.poll().settings, None, "a cancelled picker changes nothing");
        answer(Folder::Screenshots, Err("no shell".into()));
        assert_eq!(w.view.lock().unwrap().folder_error.as_deref(), Some("Couldn't choose a folder: no shell"));
        if let Some(default) = settings::default_screenshots() {
            answer(Folder::Screenshots, Ok(Some(default)));
            assert_eq!(w.poll().settings, None, "picking the default folder is the default");
        }
        w.with(&ctx, |v| v.set_folder(Folder::Recordings, None));
        assert!(w.view.lock().unwrap().settings.recordings_dir.is_none());
    }

    #[test]
    fn a_refused_shortcut_keeps_the_old_one_and_says_so() {
        let ctx = egui::Context::default();
        let w = SettingsWindow::new(&ctx, &Settings::default(), Some(Hotkey::DEFAULT), None, None);
        w.hotkey_result(&ctx, &Err("Alt+Shift+Q is already used by another app".into()), Some(Hotkey::DEFAULT));
        {
            let v = w.view.lock().unwrap();
            assert_eq!(v.status, Some(("Alt+Shift+Q is already used by another app. Keeping Alt+Shift+S.".into(), true)));
            assert_eq!(v.settings.shortcut, Hotkey::DEFAULT);
        }
        let new = Hotkey { mods: Hotkey::DEFAULT.mods, vk: b'Q' as u32 };
        w.hotkey_result(&ctx, &Ok(new), Some(new));
        let v = w.view.lock().unwrap();
        assert_eq!((v.settings.shortcut, v.active), (new, Some(new)));
        assert_eq!(v.status, Some(("Alt+Shift+Q now starts a new snip from anywhere".into(), false)));
    }
}
