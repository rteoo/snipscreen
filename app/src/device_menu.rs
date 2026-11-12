//! The recording bar's sound menus: which microphone, and which sound output, to record. Each is a
//! small window of its own under the bar's arrow button, because the bar is a window only as tall as
//! its buttons and a menu drawn inside it would be cut off.

use std::time::{Duration, Instant};

use eframe::egui::{self, pos2, vec2, Align2, FontId, Rect, Sense, ViewportBuilder, ViewportId};

use crate::audio::{self, Devices, Flow};
use crate::icons::{self, Icon};
use crate::widgets::{BUTTON_BG, ICON, ICON_INACTIVE, OUTLINE, TOOLBAR_BG};
use crate::win;

const TITLE: &str = "Snipscreen sound devices";
const WIDTH: f32 = 400.0;
const ROW_H: f32 = 30.0;
const PAD: f32 = 5.0;
/// Devices come and go (a headset plugged in): the list is read again this often while it shows.
// ceiling: polling; an IMMNotificationClient would update at once, if a second's wait ever shows.
const REREAD: Duration = Duration::from_secs(1);

/// What a row of the menu does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Don't record this sound.
    Off,
    /// Record the Windows default device, whichever it is at the time.
    Default,
    Device(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub choice: Choice,
    pub label: String,
    pub chosen: bool,
    /// A microphone muted in Windows: it would record silence.
    pub muted: bool,
}

/// The menu's rows for `devices`, given whether this sound is recorded (`on`) and the device chosen
/// for it (`None`: the Windows default).
pub fn rows(flow: Flow, devices: &Devices, on: bool, chosen: Option<&str>) -> Vec<Row> {
    let input = flow == Flow::Input;
    let muted = |id: &str| input && devices.list.iter().any(|d| d.id == id && d.muted);
    let off = if input { "No microphone" } else { "Don't record the computer's sound" };
    let mut rows = vec![Row { choice: Choice::Off, label: off.into(), chosen: !on, muted: false }];
    let default = devices.default.as_deref();
    let label = match default.and_then(|id| devices.name_of(id)) {
        Some(name) => format!("Windows default ({name})"),
        None => "Windows default".into(),
    };
    rows.push(Row { choice: Choice::Default, label, chosen: on && chosen.is_none(), muted: default.is_some_and(muted) });
    for d in &devices.list {
        rows.push(Row { choice: Choice::Device(d.id.clone()), label: d.name.clone(), chosen: on && chosen == Some(d.id.as_str()), muted: muted(&d.id) });
    }
    // The chosen device is unplugged: it stays chosen (it may come back), and says so.
    if let Some(id) = chosen.filter(|id| devices.name_of(id).is_none()) {
        let what = if input { "The chosen microphone" } else { "The chosen output" };
        rows.push(Row { choice: Choice::Device(id.to_string()), label: format!("{what} (not connected)"), chosen: on, muted: false });
    }
    rows
}

pub struct DeviceMenu {
    pub flow: Flow,
    devices: Devices,
    read: Instant,
    /// Top-left, in screen pixels.
    at: [i32; 2],
    had_focus: bool,
}

impl DeviceMenu {
    /// Reads the devices of `flow`; the window opens on the next `show`, at the spot `place` gives.
    pub fn open(flow: Flow) -> Self {
        Self { flow, devices: audio::devices(flow), read: Instant::now(), at: [0, 0], had_focus: false }
    }

    /// Puts the window's top-left at `at`, in screen pixels.
    pub fn place(&mut self, at: [i32; 2]) {
        self.at = at;
    }

    /// The window's size in points.
    pub fn size(&self, on: bool, chosen: Option<&str>) -> egui::Vec2 {
        vec2(WIDTH, rows(self.flow, &self.devices, on, chosen).len() as f32 * ROW_H + 2.0 * PAD)
    }

    /// Shows the menu. Returns the row clicked, and whether the menu is done (a choice, Esc, or a
    /// click elsewhere, which takes its focus).
    pub fn show(&mut self, ctx: &egui::Context, on: bool, chosen: Option<&str>) -> (Option<Choice>, bool) {
        if self.read.elapsed() >= REREAD {
            self.devices = audio::devices(self.flow);
            self.read = Instant::now();
        }
        ctx.request_repaint_after(REREAD);
        let rows = rows(self.flow, &self.devices, on, chosen);
        let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
        let size = self.size(on, chosen);
        let px = [(size.x * ppp).round() as i32, (size.y * ppp).round() as i32];
        let builder = ViewportBuilder::default()
            .with_title(TITLE)
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false)
            .with_active(true)
            .with_position(pos2(self.at[0] as f32, self.at[1] as f32) / ppp)
            .with_inner_size(size);
        let at = self.at;
        ctx.show_viewport_immediate(ViewportId::from_hash_of(TITLE), builder, |ui, _| {
            if let Some(hwnd) = win::find_window(TITLE) {
                win::exclude_from_capture(hwnd, true);
                win::fit_client(hwnd, at[0], at[1], px[0], px[1]);
            }
            let focused = ui.ctx().input(|i| i.viewport().focused);
            self.had_focus |= focused == Some(true);
            let mut done = (self.had_focus && focused == Some(false)) || ui.ctx().input(|i| i.key_pressed(egui::Key::Escape));
            let mut picked = None;
            let frame = egui::Frame::new().fill(TOOLBAR_BG).stroke((1.0, OUTLINE)).inner_margin(PAD);
            egui::CentralPanel::default().frame(frame).show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
                for row in &rows {
                    if menu_row(ui, row).clicked() {
                        picked = Some(row.choice.clone());
                        done = true;
                    }
                }
            });
            (picked, done)
        })
    }
}

/// One row: a check for the current choice, the name, and a note when the microphone is muted in Windows.
fn menu_row(ui: &mut egui::Ui, row: &Row) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, BUTTON_BG);
    }
    if row.chosen {
        icons::paint(p, Rect::from_center_size(rect.left_center() + vec2(16.0, 0.0), vec2(14.0, 14.0)), Icon::Check, ICON, None);
    }
    let note_w = if row.muted { 92.0 } else { 0.0 };
    let name = p.layout(row.label.clone(), FontId::proportional(14.0), ICON, rect.width() - 40.0 - note_w);
    let galley = if name.rows.len() > 1 { p.layout_no_wrap(elide(&row.label, 50), FontId::proportional(14.0), ICON) } else { name };
    p.galley(pos2(rect.left() + 32.0, rect.center().y - galley.size().y / 2.0), galley, ICON);
    if row.muted {
        p.text(rect.right_center() - vec2(10.0, 0.0), Align2::RIGHT_CENTER, "muted in Windows", FontId::proportional(12.0), ICON_INACTIVE);
    }
    resp.on_hover_text(&row.label)
}

/// `s` cut to `max` characters with an ellipsis.
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::Device;

    fn two_mics() -> Devices {
        let d = |id: &str, name: &str, muted| Device { id: id.into(), name: name.into(), muted };
        Devices { list: vec![d("a", "Headset Microphone", false), d("b", "Laptop Array", true)], default: Some("a".into()) }
    }

    fn labels(rows: &[Row]) -> Vec<(&str, bool, bool)> {
        rows.iter().map(|r| (r.label.as_str(), r.chosen, r.muted)).collect()
    }

    #[test]
    fn the_menu_offers_off_the_default_and_every_device_and_ticks_the_choice() {
        let mics = two_mics();
        assert_eq!(
            labels(&rows(Flow::Input, &mics, true, None)),
            [("No microphone", false, false), ("Windows default (Headset Microphone)", true, false), ("Headset Microphone", false, false), ("Laptop Array", false, true)]
        );
        let chosen = rows(Flow::Input, &mics, true, Some("b"));
        assert!(chosen[3].chosen && !chosen[1].chosen && chosen[3].choice == Choice::Device("b".into()));
        let off = rows(Flow::Input, &mics, false, Some("b"));
        assert!(off[0].chosen && off.iter().filter(|r| r.chosen).count() == 1, "off ticks only Off, the device stays remembered");
    }

    #[test]
    fn an_unplugged_choice_stays_listed_and_outputs_are_never_marked_muted() {
        let mics = two_mics();
        let gone = rows(Flow::Input, &mics, true, Some("z"));
        assert_eq!(labels(&gone).last(), Some(&("The chosen microphone (not connected)", true, false)));
        let mut outs = two_mics();
        outs.default = None;
        let r = rows(Flow::Output, &outs, true, None);
        assert_eq!(r[0].label, "Don't record the computer's sound");
        assert_eq!(r[1].label, "Windows default");
        assert!(r.iter().all(|r| !r.muted), "only microphones carry the muted note");
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(elide("short", 38), "short");
        assert_eq!(elide(&"x".repeat(50), 10).chars().count(), 10);
    }
}
