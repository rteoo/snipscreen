//! The look shared by the home window, the editor windows and the picker: colours sampled from
//! the original, toolbar sizes, egui style, small painted widgets, toasts and the Esc / popup rules.

use std::time::Duration;

use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Key, Modifiers, Rect, RichText, Sense, StrokeKind, Vec2};

use crate::icons::{self, Icon};

pub const ACCENT: Color32 = Color32::from_rgb(0xff, 0xb6, 0x34);
pub const TOOLBAR_BG: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
pub const CANVAS_BG: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2c);
pub const BUTTON_BG: Color32 = Color32::from_rgb(0x2d, 0x2d, 0x2d);
pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);
pub const OUTLINE: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);
pub const DIVIDER: Color32 = Color32::from_rgb(0x31, 0x31, 0x31);
pub const ICON: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
pub const ICON_OFF: Color32 = Color32::from_rgb(0x6b, 0x6b, 0x6b);
pub const DIM: Color32 = Color32::from_black_alpha(120);
pub const BTN: Vec2 = vec2(36.0, 34.0);
pub const GAP: f32 = 4.0;
pub const DIVIDER_W: f32 = 13.0;
/// Clear space kept between the toolbar groups.
pub const GROUP_GAP: f32 = 16.0;
/// Smallest window: the home toolbar, or an editor showing a video.
pub const MIN_WINDOW_W: f32 = 520.0;
pub const MIN_WINDOW_H: f32 = 240.0;
/// How long a message stays up once something is drawing it.
pub const NOTE_SECS: f64 = 4.0;

pub fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    // Ctrl+Plus/Minus/0 zoom the snip (see `editor::read_keys`), not egui's whole UI.
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
    ctx.style_mut_of(egui::Theme::Dark, |s| {
        s.spacing.button_padding = vec2(10.0, 5.0);
        s.spacing.interact_size.y = 30.0;
        s.visuals.selection.bg_fill = Color32::from_rgb(0x3d, 0x3d, 0x3d);
        s.visuals.selection.stroke.color = Color32::WHITE;
        s.visuals.panel_fill = TOOLBAR_BG;
        s.visuals.window_fill = TOOLBAR_BG;
        s.visuals.extreme_bg_color = CANVAS_BG;
        s.visuals.widgets.noninteractive.bg_stroke.color = DIVIDER;
        s.visuals.slider_trailing_fill = true;
        s.visuals.widgets.inactive.bg_fill = BUTTON_BG;
        s.visuals.widgets.inactive.weak_bg_fill = BUTTON_BG;
        s.visuals.widgets.hovered.weak_bg_fill = BUTTON_HOVER;
        for w in [
            &mut s.visuals.widgets.inactive,
            &mut s.visuals.widgets.hovered,
            &mut s.visuals.widgets.active,
            &mut s.visuals.widgets.open,
            &mut s.visuals.widgets.noninteractive,
        ] {
            w.corner_radius = CornerRadius::same(4);
        }
        s.text_styles.insert(egui::TextStyle::Body, FontId::proportional(14.0));
        s.text_styles.insert(egui::TextStyle::Button, FontId::proportional(14.0));
    });
}

/// A message for the user. Its clock starts the first time a view draws it, so
/// one raised while the picker or the recording bar owns the window (nothing
/// draws it there) is still on screen when the normal window comes back.
pub struct Note {
    pub msg: String,
    pub until: Option<f64>,
}

impl Note {
    pub fn new(msg: String) -> Self {
        Self { msg, until: None }
    }

    /// Starts the clock on the first call; false once it has run out.
    pub fn visible(&mut self, now: f64) -> bool {
        now <= *self.until.get_or_insert(now + NOTE_SECS)
    }
}

/// The message in `note` to draw right now, if any; the first draw starts its clock and books the
/// one repaint that takes it down again.
pub fn live_note(note: &mut Option<Note>, ctx: &egui::Context) -> Option<String> {
    let now = ctx.input(|i| i.time);
    let n = note.as_mut()?;
    let first_draw = n.until.is_none();
    if !n.visible(now) {
        *note = None;
        return None;
    }
    if first_draw {
        ctx.request_repaint_after(Duration::from_secs_f64(NOTE_SECS + 0.1));
    }
    Some(n.msg.clone())
}

/// Draws `msg` as a toast; `video_top` is the top of the content area while a video view is showing.
pub fn toast(ctx: &egui::Context, msg: &str, video_top: Option<f32>) {
    let (align, offset) = toast_anchor(video_top);
    egui::Area::new(egui::Id::unique("toast"))
        .anchor(align, offset)
        .order(egui::Order::Foreground)
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(BUTTON_BG)
                .stroke((1.0, OUTLINE))
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(14, 8))
                .show(ui, |ui| ui.label(RichText::new(msg).color(ICON)));
        });
}

/// Where the toast sits: bottom centre, except over the video view, whose Play
/// and "Show in folder" buttons live down there, so it drops from the top of the content instead.
pub fn toast_anchor(video_top: Option<f32>) -> (Align2, Vec2) {
    match video_top {
        Some(top) => (Align2::CENTER_TOP, vec2(0.0, top + 8.0)),
        None => (Align2::CENTER_BOTTOM, vec2(0.0, -20.0)),
    }
}

/// A popup that closes on a click outside lets that click through to whatever lies beneath it.
/// This notices when a canvas click only dismissed a popup, so it doesn't also draw.
#[derive(Default)]
pub struct PopupClickGuard {
    open_at_press: bool,
}

impl PopupClickGuard {
    /// Call once per frame before any popup is shown: the press that starts a click sees the popups as they were.
    pub fn begin_frame(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.open_at_press = egui::Popup::is_any_open(ctx);
        }
    }

    /// True for a click that was open at the press and is closed now. Drags leave the popup open, so they still draw.
    pub fn swallows(&self, ctx: &egui::Context, clicked: bool) -> bool {
        clicked && self.open_at_press && !egui::Popup::is_any_open(ctx)
    }
}

/// Esc cancels the innermost thing: an open popup or menu closes itself on Esc, so while one is open
/// the key is left alone, and otherwise it is consumed so only one action reacts to it.
pub fn take_escape(ctx: &egui::Context) -> bool {
    let popup_open = egui::Popup::is_any_open(ctx);
    ctx.input_mut(|i| escape_for_app(i, popup_open))
}

pub fn escape_for_app(i: &mut egui::InputState, popup_open: bool) -> bool {
    !popup_open && i.consume_key(Modifiers::NONE, Key::Escape)
}

/// The wide "+ New" button that starts a snip.
pub fn new_button(ui: &mut egui::Ui, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(80.0, BTN.y), Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, if resp.hovered() { BUTTON_HOVER } else { BUTTON_BG });
    let icon = Rect::from_center_size(rect.left_center() + vec2(20.0, 0.0), vec2(18.0, 18.0));
    icons::paint(p, icon, Icon::Plus, ACCENT, None);
    p.text(rect.left_center() + vec2(36.0, 0.0), Align2::LEFT_CENTER, "New", FontId::proportional(14.0), ICON);
    resp.on_hover_text(tip)
}

pub fn icon_button(ui: &mut egui::Ui, icon: Icon, ink: Option<Color32>, selected: bool, enabled: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(BTN, if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    if selected {
        p.rect(rect, 4.0, BUTTON_BG, (1.0, OUTLINE), StrokeKind::Inside);
    } else if enabled && resp.hovered() {
        p.rect_filled(rect, 4.0, BUTTON_BG);
    }
    let color = if enabled { ICON } else { ICON_OFF };
    icons::paint(p, Rect::from_center_size(rect.center(), vec2(20.0, 20.0)), icon, color, ink);
    resp.on_hover_text(tip)
}

/// Icon plus a small chevron; `badge` writes a number by the icon (timer seconds).
pub fn dropdown(ui: &mut egui::Ui, icon: Icon, badge: Option<u64>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(52.0, BTN.y), Sense::click());
    let p = ui.painter();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, BUTTON_BG);
    }
    let icon_rect = Rect::from_center_size(rect.left_center() + vec2(18.0, 0.0), vec2(20.0, 20.0));
    icons::paint(p, icon_rect, icon, ICON, None);
    icons::paint(p, Rect::from_center_size(rect.right_center() - vec2(13.0, 0.0), vec2(12.0, 12.0)), Icon::Chevron, ICON, None);
    if let Some(n) = badge {
        p.text(icon_rect.right_bottom() + vec2(1.0, 2.0), Align2::CENTER_BOTTOM, n.to_string(), FontId::proportional(10.0), ACCENT);
    }
    resp
}

pub fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(DIVIDER_W, BTN.y), Sense::hover());
    ui.painter().line_segment([rect.center_top() + vec2(0.0, 7.0), rect.center_bottom() - vec2(0.0, 7.0)], (1.0, DIVIDER));
}

pub fn dim_outside(painter: &egui::Painter, all: Rect, keep: Rect) {
    let keep = keep.intersect(all);
    for r in [
        Rect::from_min_max(all.min, pos2(all.max.x, keep.min.y)),
        Rect::from_min_max(pos2(all.min.x, keep.max.y), all.max),
        Rect::from_min_max(pos2(all.min.x, keep.min.y), pos2(keep.min.x, keep.max.y)),
        Rect::from_min_max(pos2(keep.max.x, keep.min.y), pos2(all.max.x, keep.max.y)),
    ] {
        if r.is_positive() {
            painter.rect_filled(r, 0.0, DIM);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Event, Pos2};

    #[test]
    fn note_clock_starts_when_first_drawn() {
        let mut n = Note::new("x".into());
        // Raised at t=0 but only drawn from t=100: it still gets its full time.
        assert!(n.visible(100.0));
        assert!(n.visible(100.0 + NOTE_SECS));
        assert!(!n.visible(100.0 + NOTE_SECS + 0.01));
    }

    #[test]
    fn toast_leaves_the_video_buttons_alone() {
        assert_eq!(toast_anchor(None), (Align2::CENTER_BOTTOM, vec2(0.0, -20.0)));
        let (align, offset) = toast_anchor(Some(48.0));
        assert_eq!(align, Align2::CENTER_TOP);
        assert!(offset.y > 48.0, "below the toolbar");
    }

    #[test]
    fn esc_is_consumed_so_a_second_reader_does_not_also_act() {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }],
            ..Default::default()
        };
        let (mut first, mut second) = (false, false);
        let mut out = ctx.run_ui(input, |ui| {
            first = take_escape(ui.ctx());
            second = take_escape(ui.ctx());
        });
        out.textures_delta.clear();
        assert!(first && !second);
    }

    /// One frame of a toolbar with a click-outside popup over a canvas; returns whether the canvas would draw.
    fn popup_frame(ctx: &egui::Context, guard: &mut PopupClickGuard, events: Vec<Event>, open: bool) -> bool {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut draws = false;
        let mut out = ctx.run_ui(input, |ui| {
            guard.begin_frame(ui.ctx());
            egui::Panel::top("t").show(ui, |ui| {
                let pen = ui.button("pen");
                egui::Popup::from_response(&pen)
                    .open_memory(open.then_some(egui::SetOpenCommand::Bool(true)))
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| ui.label("options"));
            });
            egui::CentralPanel::default().show(ui, |ui| {
                let resp = ui.allocate_response(ui.available_size(), Sense::click_and_drag());
                draws = (resp.clicked() || resp.drag_started()) && !guard.swallows(ui.ctx(), resp.clicked());
            });
        });
        out.textures_delta.clear();
        draws
    }

    fn click(ctx: &egui::Context, guard: &mut PopupClickGuard, at: Pos2) -> bool {
        let btn = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        popup_frame(ctx, guard, vec![Event::PointerMoved(at)], false);
        popup_frame(ctx, guard, vec![btn(true)], false);
        popup_frame(ctx, guard, vec![btn(false)], false)
    }

    #[test]
    fn click_that_closes_a_popup_does_not_draw() {
        let (ctx, mut guard) = (egui::Context::default(), PopupClickGuard::default());
        for _ in 0..3 {
            popup_frame(&ctx, &mut guard, vec![], false);
        }
        popup_frame(&ctx, &mut guard, vec![], true);
        popup_frame(&ctx, &mut guard, vec![], false);
        assert!(egui::Popup::is_any_open(&ctx), "popup should be open before the click");
        assert!(!click(&ctx, &mut guard, pos2(400.0, 400.0)), "the dismissing click must not reach the canvas");
        assert!(!egui::Popup::is_any_open(&ctx), "click outside should have closed the popup");
        assert!(click(&ctx, &mut guard, pos2(400.0, 400.0)), "the next click draws as usual");
    }

    #[test]
    fn esc_closes_an_open_popup_first_and_cancels_only_on_the_next_press() {
        let ctx = egui::Context::default();
        let esc = || Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        // Same order as the windows' `ui`: the shortcut reader runs before the toolbar shows the popup.
        let frame = |events: Vec<Event>, open: bool| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))), events, ..Default::default() };
            let mut cancels = false;
            let mut out = ctx.run_ui(input, |ui| {
                cancels = take_escape(ui.ctx());
                egui::Panel::top("t").show(ui, |ui| {
                    let b = ui.button("menu");
                    egui::Popup::from_response(&b)
                        .open_memory(open.then_some(egui::SetOpenCommand::Bool(true)))
                        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                        .show(|ui| ui.label("options"));
                });
            });
            out.textures_delta.clear();
            cancels
        };
        for _ in 0..3 {
            frame(vec![], false);
        }
        frame(vec![], true);
        frame(vec![], false);
        assert!(egui::Popup::is_any_open(&ctx), "popup should be open");
        assert!(!frame(vec![esc()], false), "first Esc belongs to the popup");
        assert!(!egui::Popup::is_any_open(&ctx), "and it closed the popup");
        assert!(frame(vec![esc()], false), "second Esc cancels");
    }
}
