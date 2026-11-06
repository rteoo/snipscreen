//! The small Settings window: the snip shortcut (with a recorder), what happens after a snip, and
//! starting with Windows.
//!
//! Like the editors it is a deferred viewport behind a mutex, so it repaints only for its own input.
//! It never touches the OS itself: it edits its copy of the settings and raises flags that the shell
//! collects in `poll`, and the shell writes back what the agent answered (`hotkey_result`).

use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui::{self, vec2, Align, Color32, Layout, RichText, Sense, StrokeKind, ViewportBuilder, ViewportCommand, ViewportId};

use crate::settings::{AfterCapture, Hotkey, Settings};
use crate::widgets::{ACCENT, BUTTON_BG, ICON, OUTLINE};

const SIZE: egui::Vec2 = vec2(440.0, 390.0);
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
        let mut builder = ViewportBuilder::default().with_title("SnipScreen Settings").with_inner_size(SIZE).with_min_inner_size(SIZE).with_resizable(false).with_maximize_button(false);
        if let Some(icon) = icon {
            builder = builder.with_icon(icon);
        }
        let status = note.map(|n| (n.to_string(), true));
        let view = View { settings: settings.clone(), changed: false, active, recording: false, record_request: None, status, closed: false };
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
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ui.style().visuals.panel_fill).inner_margin(20)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            self.shortcut_row(ui);
            ui.add_space(10.0);
            ui.label(RichText::new("After a snip").strong());
            ui.radio_value(&mut self.settings.after_capture, AfterCapture::Notify, "Show a notification with a preview");
            ui.radio_value(&mut self.settings.after_capture, AfterCapture::Editor, "Open the editor");
            ui.checkbox(&mut self.settings.auto_copy, "Copy to clipboard automatically");
            ui.checkbox(&mut self.settings.auto_save, "Save to Pictures\\Screenshots automatically");
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

    fn stop_recording(&mut self) {
        if self.recording {
            self.recording = false;
            self.record_request = Some(false);
        }
    }
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
