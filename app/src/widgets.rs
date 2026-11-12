//! The look shared by the home window, the editor windows and the picker: colours sampled from
//! the original, toolbar sizes, egui style, small painted widgets, toasts and the Esc / popup rules.

use std::sync::mpsc::Sender;
use std::time::Duration;

use eframe::egui::{self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Key, Modifiers, Rect, RichText, Sense, StrokeKind, Vec2, ViewportId};

use crate::icons::{self, Icon};
use crate::win;

pub const ACCENT: Color32 = Color32::from_rgb(0xff, 0xb6, 0x34);
pub const TOOLBAR_BG: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
pub const CANVAS_BG: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2c);
pub const BUTTON_BG: Color32 = Color32::from_rgb(0x2d, 0x2d, 0x2d);
pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);
/// A toolbar button held down.
pub const BUTTON_PRESSED: Color32 = Color32::from_rgb(0x29, 0x29, 0x29);
pub const OUTLINE: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);
pub const DIVIDER: Color32 = Color32::from_rgb(0x31, 0x31, 0x31);
pub const ICON: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
pub const ICON_OFF: Color32 = Color32::from_rgb(0x6b, 0x6b, 0x6b);
/// Caption button glyphs while the window is in the background.
pub const ICON_INACTIVE: Color32 = Color32::from_rgb(0x8a, 0x8a, 0x8a);
/// Windows 11's close button under the mouse, and held down.
pub const CLOSE_HOVER: Color32 = Color32::from_rgb(0xc4, 0x2b, 0x1c);
pub const CLOSE_PRESSED: Color32 = Color32::from_rgb(0xa7, 0x26, 0x19);
pub const DIM: Color32 = Color32::from_black_alpha(120);
/// The title bar, which holds the toolbar.
pub const BAR_H: f32 = 40.0;
/// Space before the first control and after the last one ahead of the caption buttons.
pub const BAR_PAD: f32 = 8.0;
/// Windows 11 caption buttons: as wide as the system's, as tall as our bar.
pub const CAPTION_W: f32 = 46.0;
pub const BTN: Vec2 = vec2(32.0, 32.0);
/// Toolbar icons are drawn this many points square.
pub const GLYPH: f32 = 18.0;
/// Control sizes: `FULL` for the toolbars, `COMPACT` for the picker's helper bar.
#[derive(Clone, Copy)]
pub struct Scale {
    pub btn: Vec2,
    pub glyph: f32,
    pub dropdown_w: f32,
}
pub const FULL: Scale = Scale { btn: BTN, glyph: GLYPH, dropdown_w: 46.0 };
pub const COMPACT: Scale = Scale { btn: vec2(26.0, 26.0), glyph: 16.0, dropdown_w: 40.0 };
/// The Size slider in the tool options: a thin rail and a small round handle.
pub const SLIDER_RAIL: f32 = 4.0;
pub const SLIDER_HANDLE: f32 = 14.0;
pub const RADIUS: u8 = 4;
pub const GAP: f32 = 4.0;
pub const DIVIDER_W: f32 = 9.0;
/// Clear space kept between the toolbar groups.
pub const GROUP_GAP: f32 = 16.0;
/// Smallest window: the home toolbar, or an editor showing a video.
pub const MIN_WINDOW_W: f32 = 520.0;
pub const MIN_WINDOW_H: f32 = 240.0;
/// Dashes around an area being picked or recorded: (on, off) in points.
pub const DASH: (f32, f32) = (5.0, 4.0);
pub const DASH_STROKE: f32 = 1.0;
pub const DASH_COLOR: Color32 = Color32::from_rgba_premultiplied(230, 230, 230, 230);
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
            w.corner_radius = CornerRadius::same(RADIUS);
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

/// Hands background work's messages to one window's toast and wakes that window to show them.
#[derive(Clone)]
pub struct Notifier {
    tx: Sender<String>,
    ctx: egui::Context,
    viewport: ViewportId,
}

impl Notifier {
    pub fn new(tx: Sender<String>, ctx: egui::Context, viewport: ViewportId) -> Self {
        Self { tx, ctx, viewport }
    }

    pub fn send(&self, msg: String) {
        let _ = self.tx.send(msg);
        self.ctx.request_repaint_of(self.viewport);
    }
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

/// Background for a toolbar button: quiet fills for hover and press, a slightly stronger one for
/// the selected tool. Nothing at rest.
fn button_fill(p: &egui::Painter, rect: Rect, resp: &egui::Response, selected: bool, enabled: bool) {
    let fill = if enabled && resp.is_pointer_button_down_on() {
        BUTTON_PRESSED
    } else if selected {
        BUTTON_HOVER
    } else if enabled && resp.hovered() {
        BUTTON_BG
    } else {
        return;
    };
    p.rect_filled(rect, RADIUS, fill);
}

/// The wide "+ New" button that starts a snip.
pub fn new_button(ui: &mut egui::Ui, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(72.0, BTN.y), Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, RADIUS, if resp.is_pointer_button_down_on() { BUTTON_PRESSED } else if resp.hovered() { BUTTON_HOVER } else { BUTTON_BG });
    let icon = Rect::from_center_size(rect.left_center() + vec2(18.0, 0.0), vec2(16.0, 16.0));
    icons::paint(p, icon, Icon::Plus, ACCENT, None);
    p.text(rect.left_center() + vec2(32.0, 0.0), Align2::LEFT_CENTER, "New", FontId::proportional(14.0), ICON);
    resp.on_hover_text(tip)
}

pub fn icon_button(ui: &mut egui::Ui, icon: Icon, ink: Option<Color32>, selected: bool, enabled: bool, tip: &str) -> egui::Response {
    icon_button_sized(ui, FULL, icon, ink, selected, enabled, tip)
}

pub fn icon_button_sized(ui: &mut egui::Ui, sc: Scale, icon: Icon, ink: Option<Color32>, selected: bool, enabled: bool, tip: &str) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(sc.btn, if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    button_fill(p, rect, &resp, selected, enabled);
    let color = if enabled { ICON } else { ICON_OFF };
    icons::paint(p, Rect::from_center_size(rect.center(), Vec2::splat(sc.glyph)), icon, color, ink);
    resp.on_hover_text(tip)
}

/// Icon plus a small chevron; `badge` writes a number by the icon (timer seconds).
pub fn dropdown(ui: &mut egui::Ui, icon: Icon, badge: Option<u64>) -> egui::Response {
    dropdown_sized(ui, FULL, icon, badge)
}

pub fn dropdown_sized(ui: &mut egui::Ui, sc: Scale, icon: Icon, badge: Option<u64>) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(sc.dropdown_w, sc.btn.y), Sense::click());
    let p = ui.painter();
    button_fill(p, rect, &resp, false, true);
    let icon_rect = Rect::from_center_size(rect.left_center() + vec2(sc.dropdown_w * 0.37, 0.0), Vec2::splat(sc.glyph));
    icons::paint(p, icon_rect, icon, ICON, None);
    icons::paint(p, Rect::from_center_size(rect.right_center() - vec2(sc.dropdown_w * 0.24, 0.0), Vec2::splat(sc.glyph * 0.55)), Icon::Chevron, ICON, None);
    if let Some(n) = badge {
        p.text(icon_rect.right_bottom() + vec2(1.0, 2.0), Align2::CENTER_BOTTOM, n.to_string(), FontId::proportional(10.0), ACCENT);
    }
    resp
}

pub fn divider(ui: &mut egui::Ui) {
    divider_sized(ui, FULL);
}

pub fn divider_sized(ui: &mut egui::Ui, sc: Scale) {
    let (rect, _) = ui.allocate_exact_size(vec2(DIVIDER_W, sc.btn.y), Sense::hover());
    let x = ui.painter().round_to_pixel_center(rect.center().x);
    let half = sc.btn.y / 4.0;
    ui.painter().line_segment([pos2(x, rect.center().y - half), pos2(x, rect.center().y + half)], (1.0, DIVIDER));
}

/// The tool options' Size slider: a thin rail, accent up to the handle, and a small light round handle.
pub fn slim_slider(ui: &mut egui::Ui, value: &mut f32, range: std::ops::RangeInclusive<f32>) -> egui::Response {
    let (lo, hi) = (*range.start(), *range.end());
    let r = SLIDER_HANDLE / 2.0;
    let (rect, mut resp) = ui.allocate_exact_size(vec2(ui.spacing().slider_width, SLIDER_HANDLE + 6.0), Sense::click_and_drag());
    let (x0, x1) = (rect.left() + r, rect.right() - r);
    if let Some(pos) = resp.interact_pointer_pos() {
        let v = lo + ((pos.x - x0) / (x1 - x0)).clamp(0.0, 1.0) * (hi - lo);
        if v != *value {
            *value = v;
            resp.mark_changed();
        }
    }
    let t = ((*value - lo) / (hi - lo)).clamp(0.0, 1.0);
    let hx = x0 + t * (x1 - x0);
    let p = ui.painter();
    let rail = Rect::from_center_size(rect.center(), vec2(rect.width(), SLIDER_RAIL));
    p.rect_filled(rail, SLIDER_RAIL / 2.0, OUTLINE);
    p.rect_filled(Rect::from_min_max(rail.min, pos2(hx, rail.max.y)), SLIDER_RAIL / 2.0, ACCENT);
    let grow = if resp.dragged() || resp.hovered() { 1.0 } else { 0.0 };
    p.circle_filled(pos2(hx, rect.center().y), r + grow, ICON);
    resp
}

/// The window's top row: the toolbar merged into the title bar, with the caption buttons at the
/// right. `fill` lays the controls out in the row it is given and returns the rects of its control
/// groups; the empty rest of the bar moves the window.
pub fn title_bar(ui: &mut egui::Ui, id: &str, bar: &win::TitleBar, fill: impl FnOnce(&mut egui::Ui, Rect) -> Vec<Rect>) {
    egui::Panel::top(id)
        .exact_size(BAR_H)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(TOOLBAR_BG))
        .show(ui, |ui| {
            let full = ui.max_rect();
            ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
            let caption_left = full.max.x - 3.0 * CAPTION_W;
            let row = Rect::from_min_max(pos2(full.min.x + BAR_PAD, full.center().y - BTN.y / 2.0), pos2(caption_left - BAR_PAD, full.center().y + BTN.y / 2.0));
            // The toolbar first, so Tab reaches its controls before the caption buttons.
            let mut controls = fill(ui, row);
            let [min, max, close] = caption_buttons(ui, full, bar);
            controls.extend([min, close]);
            let ctx = ui.ctx();
            bar.publish(ctx.pixels_per_point(), full, &controls, Some(max), !egui::Popup::is_any_open(ctx));
        });
}

/// Minimise, maximise/restore and close at the right end of `bar`, drawn like Windows 11's.
/// Over the maximise button the mouse belongs to Windows (for snap layouts), so its hover and
/// press come from `bar` and its click from the window frame.
fn caption_buttons(ui: &mut egui::Ui, bar: Rect, title_bar: &win::TitleBar) -> [Rect; 3] {
    let ctx = ui.ctx().clone();
    let (focused, maximized) = ctx.input(|i| (i.viewport().focused.unwrap_or(true), i.viewport().maximized.unwrap_or(false)));
    let close = Rect::from_min_max(pos2(bar.max.x - CAPTION_W, bar.min.y), bar.max);
    let max = close.translate(vec2(-CAPTION_W, 0.0));
    let min = max.translate(vec2(-CAPTION_W, 0.0));
    let id = ui.scope_id().with("caption");
    let ink = if focused { ICON } else { ICON_INACTIVE };
    let p = ui.painter().clone();
    // Glyph lines sit on pixel centres so they stay one crisp pixel wide.
    let centre = |r: Rect| pos2(p.round_to_pixel_center(r.center().x), p.round_to_pixel_center(r.center().y));
    let stroke = |c: Color32| egui::Stroke::new(1.0, c);

    let resp = ui.interact(min, id.with("min"), Sense::click()).on_hover_text("Minimise");
    p.rect_filled(min, 0.0, if resp.is_pointer_button_down_on() { BUTTON_PRESSED } else if resp.hovered() { BUTTON_BG } else { Color32::TRANSPARENT });
    let c = centre(min);
    p.line_segment([c - vec2(5.0, 0.0), c + vec2(5.0, 0.0)], stroke(ink));
    if resp.clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
    }

    let resp = ui.interact(max, id.with("max"), Sense::click());
    let (hot, down) = title_bar.max_state();
    let (hot, down) = (hot || resp.hovered(), down || resp.is_pointer_button_down_on());
    p.rect_filled(max, 0.0, if down { BUTTON_PRESSED } else if hot { BUTTON_BG } else { Color32::TRANSPARENT });
    let c = centre(max);
    if maximized {
        p.rect_stroke(Rect::from_min_size(c + vec2(-5.0, -3.0), vec2(8.0, 8.0)), 1.0, stroke(ink), StrokeKind::Middle);
        p.add(egui::Shape::line(vec![c + vec2(-3.0, -3.0), c + vec2(-3.0, -5.0), c + vec2(5.0, -5.0), c + vec2(5.0, 3.0), c + vec2(3.0, 3.0)], stroke(ink)));
    } else {
        p.rect_stroke(Rect::from_center_size(c, vec2(10.0, 10.0)), 1.0, stroke(ink), StrokeKind::Middle);
    }
    // Only reached before the frame is taken over: afterwards Windows has this button's clicks.
    if resp.clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
    }

    let resp = ui.interact(close, id.with("close"), Sense::click()).on_hover_text("Close");
    let (fill, glyph) = if resp.is_pointer_button_down_on() {
        (CLOSE_PRESSED, Color32::WHITE)
    } else if resp.hovered() {
        (CLOSE_HOVER, Color32::WHITE)
    } else {
        (Color32::TRANSPARENT, ink)
    };
    p.rect_filled(close, 0.0, fill);
    let c = centre(close);
    p.line_segment([c - vec2(5.0, 5.0), c + vec2(5.0, 5.0)], stroke(glyph));
    p.line_segment([c + vec2(-5.0, 5.0), c + vec2(5.0, -5.0)], stroke(glyph));
    if resp.clicked() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    [min, max, close]
}

/// A dashed outline just outside `rect`, as around an area being picked or recorded. Each side
/// starts on a dash, so the corners always read.
pub fn dashed_outline(p: &egui::Painter, rect: Rect) {
    let r = rect.expand(DASH_STROKE / 2.0);
    for side in [[r.left_top(), r.right_top()], [r.right_top(), r.right_bottom()], [r.right_bottom(), r.left_bottom()], [r.left_bottom(), r.left_top()]] {
        p.add(egui::Shape::dashed_line(&side, (DASH_STROKE, DASH_COLOR), DASH.0, DASH.1));
    }
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
