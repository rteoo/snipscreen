//! One result in its own window: a snip being marked up, or a finished screen recording.
//!
//! Every `Editor` is shown as a deferred egui viewport (`Editors::show`), so a window repaints only
//! for its own input and timers: drawing in one editor never re-runs the home window or the other
//! editors, and idle editors cost nothing. The shell owns the `Editors` list; an editor reports
//! back through two flags (`new_requested`, `closed`) that the shell collects in `Editors::poll`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CursorIcon, Event, FontId, Key, KeyboardShortcut, Layout, Modifiers, Pos2, Rect, Sense,
    StrokeKind, TextureHandle, TextureOptions, Vec2, ViewportBuilder, ViewportCommand, ViewportId,
};
use windows_sys::Win32::Foundation::HWND;

use crate::icons::{self, Icon};
use crate::markup::{self, Doc, Ink, Mark, PxRect, Shape, ShapeKind, Stroke, Text, TextRaster, Tiles};
use crate::widgets::{
    self, dim_outside, divider, escape_for_app, icon_button, Note, Notifier, PopupClickGuard, ACCENT, BTN, BUTTON_HOVER, DIVIDER_W, GAP, GROUP_GAP, ICON,
    ICON_OFF, MIN_WINDOW_H, MIN_WINDOW_W, TOOLBAR_BG,
};
use crate::win;

/// Measured: bar padding 8 + New button 72 + markup with crop buttons 382 + output group 117 + 2 group gaps
/// + padding 8 + caption buttons 138 = 757.
pub const MIN_EDIT_W: f32 = 760.0;
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 8.0;
/// Snip zoom change per Ctrl+Plus / Ctrl+Minus press.
const ZOOM_STEP: f32 = 1.25;
/// Empty space kept around the snip inside the scrolling canvas, in points.
const CANVAS_MARGIN: Vec2 = vec2(32.0, 32.0);
const SCROLL_ID: &str = "snip_scroll";
/// Frames spent keeping a new window on its monitor while Windows finishes any rescaling.
const SETTLE_FRAMES: u8 = 8;
/// Frames a new window waits to be shown before it gives up asking for the foreground.
const RAISE_FRAMES: u8 = 60;
/// Frames a Save waits for its window's handle (found by title once Windows has made the window).
const SAVE_WAIT_FRAMES: u8 = 30;
/// Each further open editor starts this many pixels down and right of the last, so none hides another.
const CASCADE: i32 = 28;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Pen,
    Highlighter,
    Shape,
    Text,
    Eraser,
}

/// The part of the crop frame a drag holds: a side, a corner, or the whole frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grip {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Grip {
    fn cursor(self) -> CursorIcon {
        match self {
            Grip::Move => CursorIcon::Move,
            Grip::N | Grip::S => CursorIcon::ResizeVertical,
            Grip::E | Grip::W => CursorIcon::ResizeHorizontal,
            Grip::NW | Grip::SE => CursorIcon::ResizeNwSe,
            Grip::NE | Grip::SW => CursorIcon::ResizeNeSw,
        }
    }
}

/// How close to an edge, in points, the pointer takes hold of it.
const GRIP_REACH: f32 = 10.0;
/// Smallest crop frame, in points on screen.
const MIN_CROP_PT: f32 = 16.0;

/// The grip under `p` for a frame shown at `frame` (both in points): a corner or side within
/// `reach`, else the inside to move it, else nothing.
fn grip_at(frame: Rect, p: Pos2, reach: f32) -> Option<Grip> {
    if !frame.expand(reach).contains(p) {
        return None;
    }
    // On a frame narrower than two reaches both sides are in range; the nearer one wins.
    let side = |lo: f32, hi: f32, v: f32| {
        let (dl, dh) = ((v - lo).abs(), (v - hi).abs());
        if dl.min(dh) > reach {
            0
        } else if dl <= dh {
            -1
        } else {
            1
        }
    };
    match (side(frame.left(), frame.right(), p.x), side(frame.top(), frame.bottom(), p.y)) {
        (-1, -1) => Some(Grip::NW),
        (1, -1) => Some(Grip::NE),
        (-1, 1) => Some(Grip::SW),
        (1, 1) => Some(Grip::SE),
        (-1, 0) => Some(Grip::W),
        (1, 0) => Some(Grip::E),
        (0, -1) => Some(Grip::N),
        (0, 1) => Some(Grip::S),
        _ => frame.contains(p).then_some(Grip::Move),
    }
}

/// The frame after dragging `grip` by `delta` from `start` (image pixels): it stays inside
/// `bounds`, and sides never come closer than `min`.
fn drag_frame(start: Rect, grip: Grip, delta: Vec2, bounds: Rect, min: f32) -> Rect {
    if grip == Grip::Move {
        let dx = delta.x.min(bounds.max.x - start.max.x).max(bounds.min.x - start.min.x);
        let dy = delta.y.min(bounds.max.y - start.max.y).max(bounds.min.y - start.min.y);
        return start.translate(vec2(dx, dy));
    }
    let min = vec2(min.min(bounds.width()), min.min(bounds.height()));
    let mut r = start;
    if matches!(grip, Grip::W | Grip::NW | Grip::SW) {
        r.min.x = (start.min.x + delta.x).min(r.max.x - min.x).max(bounds.min.x);
    }
    if matches!(grip, Grip::E | Grip::NE | Grip::SE) {
        r.max.x = (start.max.x + delta.x).max(r.min.x + min.x).min(bounds.max.x);
    }
    if matches!(grip, Grip::N | Grip::NW | Grip::NE) {
        r.min.y = (start.min.y + delta.y).min(r.max.y - min.y).max(bounds.min.y);
    }
    if matches!(grip, Grip::S | Grip::SW | Grip::SE) {
        r.max.y = (start.max.y + delta.y).max(r.min.y + min.y).min(bounds.max.y);
    }
    r
}

/// The crop frame: dimmed outside, a thin border, and bars on the corners and sides to grab.
fn paint_crop_frame(painter: &egui::Painter, img_rect: Rect, frame: Rect, px: Vec2) {
    dim_outside(painter, img_rect, frame);
    painter.rect_stroke(frame, 0.0, (1.0, Color32::from_white_alpha(220)), StrokeKind::Middle);
    let (len, thick) = (20.0_f32.min(frame.width() / 2.0).min(frame.height() / 2.0), 4.0);
    let mut bars = Vec::new();
    for (corner, dx, dy) in [(frame.left_top(), 1.0, 1.0), (frame.right_top(), -1.0, 1.0), (frame.left_bottom(), 1.0, -1.0), (frame.right_bottom(), -1.0, -1.0)] {
        // Each arm starts half a bar outside the corner so the two meet in a square corner, not a notch.
        bars.push([corner - vec2(dx * thick / 2.0, 0.0), corner + vec2(dx * len, 0.0)]);
        bars.push([corner - vec2(0.0, dy * thick / 2.0), corner + vec2(0.0, dy * len)]);
    }
    let half = len * 0.6;
    for (mid, along) in [(frame.center_top(), vec2(1.0, 0.0)), (frame.center_bottom(), vec2(1.0, 0.0)), (frame.left_center(), vec2(0.0, 1.0)), (frame.right_center(), vec2(0.0, 1.0))] {
        bars.push([mid - along * half, mid + along * half]);
    }
    // A dark halo under each white bar keeps the handles visible on light and dark pictures alike.
    for [a, b] in &bars {
        painter.line_segment([*a, *b], (thick + 2.0, Color32::from_black_alpha(90)));
    }
    for [a, b] in &bars {
        painter.line_segment([*a, *b], (thick, Color32::WHITE));
    }
    let galley = painter.layout_no_wrap(format!("{:.0} × {:.0}", px.x, px.y), FontId::proportional(12.0), Color32::WHITE);
    let at = frame.left_top() + vec2(8.0, 10.0);
    painter.rect_filled(Rect::from_min_size(at, galley.size() + vec2(10.0, 4.0)), 4.0, Color32::from_black_alpha(170));
    painter.galley(at + vec2(5.0, 2.0), galley, Color32::WHITE);
}

const SHAPES: [(ShapeKind, Icon, &str); 4] = [
    (ShapeKind::Arrow, Icon::Arrow, "Arrow"),
    (ShapeKind::Line, Icon::Line, "Line"),
    (ShapeKind::Rectangle, Icon::RectOutline, "Rectangle"),
    (ShapeKind::Ellipse, Icon::Ellipse, "Ellipse"),
];

fn shape_icon(kind: ShapeKind) -> Icon {
    SHAPES.iter().find(|s| s.0 == kind).map_or(Icon::Arrow, |s| s.1)
}

const PEN_COLORS: [Color32; 6] = [
    Color32::from_rgb(0x1f, 0x1f, 0x1f),
    Color32::WHITE,
    Color32::from_rgb(0xe8, 0x1c, 0x24),
    Color32::from_rgb(0x00, 0x78, 0xd4),
    Color32::from_rgb(0x10, 0x9c, 0x3c),
    Color32::from_rgb(0xff, 0x8c, 0x00),
];
const HIGHLIGHT_COLORS: [Color32; 4] = [
    Color32::from_rgb(0xff, 0xff, 0x00),
    Color32::from_rgb(0x5c, 0xf0, 0x5c),
    Color32::from_rgb(0xff, 0x6e, 0xc7),
    Color32::from_rgb(0x5c, 0xd6, 0xff),
];

/// The chosen tool and its ink, carried from the newest snip editor into the next one so they
/// stay put from snip to snip.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Tools {
    pub tool: Tool,
    /// Colour and width in screen points.
    pub pen: (Color32, f32),
    pub highlighter: (Color32, f32),
    pub shape_kind: ShapeKind,
    pub shape: (Color32, f32),
    /// Colour and font size in screen points.
    pub text: (Color32, f32),
    pub text_outline: bool,
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            tool: Tool::Pen,
            pen: (PEN_COLORS[2], 4.0),
            highlighter: (HIGHLIGHT_COLORS[0], 18.0),
            shape_kind: ShapeKind::Arrow,
            shape: (PEN_COLORS[2], 4.0),
            text: (PEN_COLORS[2], 24.0),
            text_outline: true,
        }
    }
}

/// A finished screen recording.
pub struct VideoResult {
    pub path: PathBuf,
    pub seconds: f64,
    pub size: [u32; 2],
    pub thumb: Option<TextureHandle>,
}

/// A snip being marked up.
pub struct Snip {
    pub doc: Doc,
    pub tools: Tools,
    texture: Option<CanvasTex>,
    /// The mark being drawn, in image pixels.
    drawing: Option<Mark>,
    erase_marked: bool,
    /// The crop frame while cropping, in image pixels.
    crop: Option<Rect>,
    /// The frame's grip being dragged, the frame as it was when the drag began, and where it began (points).
    crop_drag: Option<(Grip, Rect, Pos2)>,
    /// `None` fits the snip to the window; otherwise 1.0 is one screen pixel per pixel.
    zoom: Option<f32>,
    /// Zoom factor requested from the keyboard this frame; `canvas` applies it.
    key_zoom: f32,
    /// The rest of the text being typed, while `drawing` is text.
    draft: Draft,
    /// Text was placed with an input method's composition in it; the editor cancels the composition
    /// the input method still holds, so it isn't committed a second time.
    ime_taken: bool,
}

pub enum Content {
    Snip(Box<Snip>),
    Video(VideoResult),
}

pub struct Editor {
    pub content: Content,
    title: String,
    viewport: ViewportId,
    /// The shell's window: clipboard owner, and owner of dialogs until this window's own handle is known.
    root_hwnd: isize,
    /// This editor's own window, looked up by its (unique) title.
    hwnd: Option<isize>,
    /// The toolbar doubles as the window's title bar once it is attached to `hwnd`.
    bar: win::TitleBar,
    bar_tried: bool,
    /// Screen corner to move the new window to on its first frame.
    place: Option<[i32; 2]>,
    /// Frames left in which to keep the new window on screen.
    settle: u8,
    /// Frames left in which the new window still asks for the foreground (see `raise`).
    raise: u8,
    /// Minimum window width last sent to the OS, so the command goes out only on change.
    min_width: f32,
    popup_guard: PopupClickGuard,
    note: Option<Note>,
    note_tx: Sender<String>,
    note_rx: Receiver<String>,
    /// The New button's tooltip, copied each frame from `shared_tip`, which the shell keeps current (it
    /// knows the capture mode and hotkey).
    new_tip: String,
    shared_tip: Arc<Mutex<String>>,
    /// New button or Ctrl+N pressed; the shell starts the capture.
    new_requested: bool,
    /// The window's close button was pressed; the shell drops this editor.
    closed: bool,
    /// A Save is under way (its dialog is open or its file is being written).
    saving: Arc<AtomicBool>,
    /// Frames left to wait for this window's handle before a Save asked for too early gives up.
    save_waiting: u8,
}

/// Clears its flag when dropped, however the worker holding it ends.
struct Busy(Arc<AtomicBool>);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Snip {
    pub fn new(image: ColorImage, tools: Tools) -> Self {
        Self { doc: Doc::new(image), tools, texture: None, drawing: None, erase_marked: false, crop: None, crop_drag: None, zoom: None, key_zoom: 1.0, draft: Draft::default(), ime_taken: false }
    }

    /// The picture with its ink, as it would be copied or saved. The canvas texture already holds
    /// exactly that whenever it is up to date, which saves re-burning every stroke.
    fn flattened(&self) -> Arc<ColorImage> {
        match &self.texture {
            Some(c) if c.rev == (self.doc.image_rev, self.doc.ink_rev) => c.shown.clone(),
            _ => Arc::new(self.doc.flatten()),
        }
    }

    fn apply_crop(&mut self) {
        if let Some(r) = self.crop.take() {
            self.doc.crop(r);
            self.zoom = None;
        }
        self.crop_drag = None;
    }

    /// Starts cropping with the frame around the whole snip, or cancels the crop under way.
    fn toggle_crop(&mut self) {
        let [w, h] = self.doc.size();
        self.crop = match self.crop {
            Some(_) => None,
            None => Some(Rect::from_min_size(Pos2::ZERO, vec2(w as f32, h as f32))),
        };
        self.crop_drag = None;
        self.finish_text();
    }

    /// True while text is being typed.
    fn typing(&self) -> bool {
        matches!(self.drawing, Some(Mark::Text(_)))
    }

    /// Places the text being typed, if it has anything in it.
    fn finish_text(&mut self) {
        if let (Some(m), Some(canvas)) = (take_typed(&mut self.drawing, &mut self.draft, &mut self.ime_taken), self.texture.as_mut()) {
            canvas.commit(&mut self.doc, m);
        }
    }

    fn cancel_crop(&mut self) {
        self.crop = None;
        self.crop_drag = None;
    }

    /// Undoes or redoes a step: of the text being typed while there is some (undoing past its start
    /// drops it; redo never does), of the snip otherwise.
    fn step(&mut self, redo: bool) {
        if let Some(Mark::Text(t)) = self.drawing.as_mut() {
            if redo {
                self.draft.redo(&mut t.text);
            } else if !self.draft.undo(&mut t.text) {
                self.drawing = None;
            }
            return;
        }
        step_history(&mut self.doc, &mut self.crop, &mut self.drawing, redo);
        self.crop_drag = None;
    }

    /// Whether Undo has something to take back: the text being typed (undoing past its start drops
    /// it), or the snip's own history.
    fn can_undo(&self) -> bool {
        self.typing() || self.doc.can_undo()
    }

    /// Whether Redo has something to bring back, from the same history `step` would use.
    fn can_redo(&self) -> bool {
        if self.typing() {
            !self.draft.redo.is_empty()
        } else {
            self.doc.can_redo()
        }
    }

    fn markup_width(&self) -> f32 {
        let buttons = if self.crop.is_some() { 10.0 } else { 8.0 };
        let dividers = 2.0;
        buttons * BTN.x + dividers * DIVIDER_W + (buttons + dividers - 1.0) * GAP
    }

    fn markup_controls(&mut self, ui: &mut egui::Ui) {
        for (tool, icon, tip) in [
            (Tool::Pen, Icon::Pen, "Pen (click again for colour and size)"),
            (Tool::Highlighter, Icon::Highlighter, "Highlighter (click again for colour and size)"),
            (Tool::Shape, shape_icon(self.tools.shape_kind), "Shapes (click again for shape, colour and size; Shift keeps lines at 45° and boxes square)"),
            (Tool::Text, Icon::Text, "Text: click and type, Enter places it, Shift+Enter starts a new line (click again for colour, size and outline)"),
            (Tool::Eraser, Icon::Eraser, "Eraser: rubs out whole marks"),
        ] {
            let ink = match tool {
                Tool::Pen => Some(self.tools.pen.0),
                Tool::Highlighter => Some(self.tools.highlighter.0),
                Tool::Shape => Some(self.tools.shape.0),
                Tool::Text => Some(self.tools.text.0),
                _ => None,
            };
            let resp = icon_button(ui, icon, ink, self.tools.tool == tool, true, tip);
            let again = resp.clicked() && self.tools.tool == tool;
            // A click on the tool already chosen opens its options and leaves the text being typed alone.
            if resp.clicked() && !again {
                self.tools.tool = tool;
                self.cancel_crop();
                self.finish_text();
            }
            if ink.is_some() {
                egui::Popup::from_response(&resp)
                    .open_memory(again.then_some(egui::SetOpenCommand::Toggle))
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| self.ink_options(ui, tool));
            }
        }
        divider(ui);
        if icon_button(ui, Icon::Crop, None, self.crop.is_some(), true, "Crop: drag the frame's corners and sides, then Enter").clicked() {
            self.toggle_crop();
        }
        if self.crop.is_some() {
            if icon_button(ui, Icon::Check, None, false, true, "Apply crop (Enter)").clicked() {
                self.apply_crop();
            }
            if icon_button(ui, Icon::Close, None, false, true, "Cancel crop (Esc)").clicked() {
                self.cancel_crop();
            }
        }
        divider(ui);
        if icon_button(ui, Icon::Undo, None, false, self.can_undo(), "Undo (Ctrl+Z)").clicked() {
            self.step(false);
        }
        if icon_button(ui, Icon::Redo, None, false, self.can_redo(), "Redo (Ctrl+Y)").clicked() {
            self.step(true);
        }
    }

    fn ink_options(&mut self, ui: &mut egui::Ui, tool: Tool) {
        ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
        if tool == Tool::Shape {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = GAP;
                for (kind, icon, tip) in SHAPES {
                    if icon_button(ui, icon, None, self.tools.shape_kind == kind, true, tip).clicked() {
                        self.tools.shape_kind = kind;
                    }
                }
            });
        }
        let ((color, width), palette, range) = match tool {
            Tool::Highlighter => (&mut self.tools.highlighter, &HIGHLIGHT_COLORS[..], 6.0..=48.0),
            Tool::Shape => (&mut self.tools.shape, &PEN_COLORS[..], 1.0..=24.0),
            Tool::Text => (&mut self.tools.text, &PEN_COLORS[..], 10.0..=96.0),
            _ => (&mut self.tools.pen, &PEN_COLORS[..], 1.0..=24.0),
        };
        ui.horizontal(|ui| {
            for &c in palette {
                let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
                let p = ui.painter();
                p.circle_filled(rect.center(), 9.0, c);
                p.circle_stroke(rect.center(), 9.0, (1.0, Color32::from_gray(90)));
                if *color == c {
                    p.circle_stroke(rect.center(), 11.5, (2.0, ACCENT));
                }
                if resp.clicked() {
                    *color = c;
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Size");
            ui.add(egui::Slider::new(width, range).show_value(false));
        });
        if tool == Tool::Text {
            ui.checkbox(&mut self.tools.text_outline, "Outline");
        }
    }

    fn canvas(&mut self, ui: &mut egui::Ui, guard: &PopupClickGuard) {
        let ctx = ui.ctx().clone();
        let Snip { doc, tools, texture, drawing, erase_marked, crop, crop_drag, zoom: zoom_choice, key_zoom, draft, ime_taken } = self;
        let canvas = texture.get_or_insert_with(|| CanvasTex::new(&ctx, doc));
        canvas.sync(doc);

        let ppp = ctx.pixels_per_point();
        let px_size = vec2(doc.size()[0] as f32, doc.size()[1] as f32);
        let natural = px_size / ppp;
        let view = ui.available_size();
        let fit = ((view - CANVAS_MARGIN) / natural).min_elem().clamp(0.02, 1.0);
        let wheel_zoom = if ui.rect_contains_pointer(ui.max_rect()) { ui.input(|i| i.zoom_delta()) } else { 1.0 };
        let zoom_delta = wheel_zoom * std::mem::replace(key_zoom, 1.0);
        let old_zoom = zoom_choice.unwrap_or(fit);
        if zoom_delta != 1.0 {
            *zoom_choice = Some(clamp_zoom(old_zoom * zoom_delta, fit));
        }
        let zoom = zoom_choice.unwrap_or(fit);
        let shown = natural * zoom;

        let view_rect = ui.available_rect_before_wrap();
        let mut scroll = egui::ScrollArea::both().id_salt(SCROLL_ID).auto_shrink(false);
        if zoom != old_zoom {
            // Wheel zoom stays on the pointer, keyboard zoom on the middle of the view.
            let pointer = if wheel_zoom != 1.0 { ui.input(|i| i.pointer.hover_pos()) } else { None };
            let anchor = pointer.map_or(view / 2.0, |p| p - view_rect.min);
            scroll = scroll.scroll_offset(anchored_offset(scroll_offset(ui), anchor, natural * old_zoom, shown, view));
        }

        scroll.show(ui, |ui| {
            let area = shown + CANVAS_MARGIN;
            let (outer, resp) = ui.allocate_exact_size(area.max(view), Sense::click_and_drag());
            let img_rect = Rect::from_center_size(outer.center(), shown);
            let k = px_size.x / img_rect.width(); // image pixels per point
            let to_px = |p: Pos2| ((p - img_rect.min) * k).to_pos2();
            let to_pt = |p: Pos2| img_rect.min + p.to_vec2() / k;

            let painter = ui.painter();
            // Committed ink is already burned into the texture, exactly as it will be saved.
            canvas.tex.paint(painter, img_rect, doc.size());

            let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
            // Drags are noticed a few pixels after the button goes down; start where it went down.
            let origin = ctx.input(|i| i.pointer.press_origin()).or(pointer);
            let swallowed = guard.swallows(&ctx, resp.clicked());
            match tools.tool {
                _ if swallowed => {}
                // The crop frame takes the pointer from whichever tool is chosen until it is applied or cancelled.
                _ if crop.is_some() => {
                    let frame = crop.unwrap_or(Rect::NOTHING);
                    let frame_pt = Rect::from_min_max(to_pt(frame.min), to_pt(frame.max));
                    if resp.drag_started() {
                        *crop_drag = origin.and_then(|o| grip_at(frame_pt, o, GRIP_REACH).map(|g| (g, frame, o)));
                    }
                    if let (Some((grip, start, o)), Some(p)) = (*crop_drag, resp.interact_pointer_pos()) {
                        let bounds = Rect::from_min_size(Pos2::ZERO, px_size);
                        *crop = Some(drag_frame(start, grip, (p - o) * k, bounds, MIN_CROP_PT * k));
                    }
                    if resp.drag_stopped() {
                        *crop_drag = None;
                    }
                    let held = crop_drag.map(|(g, _, _)| g).or_else(|| resp.hover_pos().and_then(|p| grip_at(frame_pt, p, GRIP_REACH)));
                    if let Some(g) = held {
                        ctx.set_cursor_icon(g.cursor());
                    }
                    let frame = crop.unwrap_or(frame);
                    paint_crop_frame(painter, img_rect, Rect::from_min_max(to_pt(frame.min), to_pt(frame.max)), frame.size());
                }
                Tool::Pen | Tool::Highlighter => {
                    let (ink, (color, width)) = if tools.tool == Tool::Pen { (Ink::Pen, tools.pen) } else { (Ink::Highlighter, tools.highlighter) };
                    if resp.hovered() {
                        ctx.set_cursor_icon(CursorIcon::Crosshair);
                    }
                    // Thickness is chosen in screen points, so ink looks the same at any zoom.
                    let width_px = width * k;
                    if resp.drag_started() || resp.clicked() {
                        if let Some(p) = origin {
                            *drawing = Some(Mark::Stroke(Stroke { ink, color, width: width_px, points: vec![to_px(p)] }));
                        }
                    }
                    if let (Some(Mark::Stroke(s)), Some(p)) = (drawing.as_mut(), pointer) {
                        let p = to_px(p);
                        if s.points.last().is_none_or(|l| l.distance(p) > 0.75 * k) {
                            s.points.push(p);
                        }
                    }
                    if resp.drag_stopped() || resp.clicked() {
                        if let Some(m) = drawing.take() {
                            canvas.commit(doc, m);
                        }
                    }
                }
                Tool::Shape => {
                    if resp.hovered() {
                        ctx.set_cursor_icon(CursorIcon::Crosshair);
                    }
                    if resp.drag_started() {
                        if let Some(p) = origin {
                            let (color, width) = tools.shape;
                            let p = to_px(p);
                            *drawing = Some(Mark::Shape(Shape { kind: tools.shape_kind, color, width: width * k, from: p, to: p }));
                        }
                    }
                    if let (Some(Mark::Shape(s)), Some(p)) = (drawing.as_mut(), resp.interact_pointer_pos()) {
                        let p = to_px(p);
                        s.to = if ctx.input(|i| i.modifiers.shift) { markup::constrain(s.kind, s.from, p) } else { p };
                    }
                    if resp.drag_stopped() {
                        match drawing.take() {
                            // Shorter than a few points on screen is a slip, not a shape.
                            Some(Mark::Shape(s)) if s.from.distance(s.to) < 3.0 * k => {}
                            Some(m) => canvas.commit(doc, m),
                            None => {}
                        }
                    }
                }
                Tool::Text => {
                    if resp.hovered() {
                        ctx.set_cursor_icon(CursorIcon::Text);
                    }
                    if resp.clicked() {
                        if let Some(p) = pointer {
                            // A click places the text being typed and starts new text there, its first line centred on the click.
                            if let Some(m) = take_typed(drawing, draft, ime_taken) {
                                canvas.commit(doc, m);
                            }
                            let mut t = Text { text: String::new(), color: tools.text.0, size: tools.text.1 * k, outline: tools.text_outline, pos: to_px(p) };
                            t.pos.y -= t.line_height() / 2.0;
                            *drawing = Some(Mark::Text(t));
                            *draft = Draft::new(tools.text.1);
                        }
                    }
                    if let Some(Mark::Text(t)) = drawing.as_mut() {
                        // Colour and outline follow the options while typing. The size, chosen in screen points like the
                        // pen's, is turned into pixels at the click and again only when the option changes, so zooming
                        // or resizing the window leaves the text as it is.
                        (t.color, t.outline) = (tools.text.0, tools.text_outline);
                        if tools.text.1 != draft.size_pt {
                            (t.size, draft.size_pt) = (tools.text.1 * k, tools.text.1);
                        }
                        if type_into(&ctx, &mut t.text, draft) {
                            if let Some(m) = take_typed(drawing, draft, ime_taken) {
                                canvas.commit(doc, m);
                            }
                        }
                    }
                }
                Tool::Eraser => {
                    let radius_pt = 10.0;
                    if let Some(p) = resp.hover_pos().or(pointer) {
                        painter.circle_stroke(p, radius_pt, (1.5, Color32::WHITE));
                        painter.circle_stroke(p, radius_pt + 1.5, (1.0, Color32::BLACK));
                    }
                    if resp.drag_started() || resp.clicked() {
                        *erase_marked = false;
                    }
                    if resp.dragged() || resp.clicked() {
                        if let Some(p) = pointer {
                            let (p, r) = (to_px(p), radius_pt * k);
                            if doc.hits(p, r) {
                                if !*erase_marked {
                                    doc.checkpoint();
                                    *erase_marked = true;
                                }
                                doc.erase_at(p, r);
                            }
                        }
                    }
                }
            }
            // An erase above changed the doc after the first sync: show it in this same frame.
            canvas.sync(doc);
            // A pen or highlighter press always ends in a mark, a click too, so the copy starts then.
            let pressing_ink = resp.is_pointer_button_down_on() && matches!(tools.tool, Tool::Pen | Tool::Highlighter) && crop.is_none();
            if drawing.is_some() || pressing_ink {
                canvas.own_picture(drawing.is_some());
            } else if !resp.is_pointer_button_down_on() {
                // A copy started for a press that drew nothing (it only closed a popup) isn't kept.
                canvas.copying = None;
            }
            // The tiles of the snip inside the scroll area's view this frame.
            let mut visible = Tiles::new(doc.size());
            visible.add(Rect::from_min_max(to_px(ui.clip_rect().min), to_px(ui.clip_rect().max)).expand(1.0));
            match drawing {
                Some(Mark::Text(t)) if !draft.preedit.is_empty() => canvas.show_live(&Mark::Text(with_preedit(t, &draft.preedit)), &visible),
                Some(m) => canvas.show_live(m, &visible),
                None => canvas.clear_live(),
            }
            if let Some(Mark::Text(t)) = drawing.as_ref() {
                let line = t.line_height() / k;
                // From the text's raster, laid out already, rather than laying out a long paste again.
                let live = canvas.live.text.as_ref();
                let typed = to_pt(live.map_or_else(|| t.caret(), |r| r.caret_at(t.text.len())));
                // An input method's composition shows after the text, underlined, until it is committed.
                let end = if draft.preedit.is_empty() { typed } else { to_pt(live.map_or_else(|| with_preedit(t, &draft.preedit).caret(), |r| r.caret())) };
                if end != typed {
                    let under = vec2(0.0, line * 0.92);
                    painter.line_segment([typed + under, end + under], (1.5, t.color));
                }
                let caret = [end, end + vec2(0.0, line)];
                painter.line_segment(caret, (3.0, t.halo_color()));
                painter.line_segment(caret, (1.5, t.color));
                // Turns the window's input method on and tells it where the text is.
                let rect = Rect::from_min_max(to_pt(t.pos), end + vec2(1.0, line));
                let cursor_rect = Rect::from_min_size(end, vec2(1.0, line));
                ctx.output_mut(|o| o.ime = Some(egui::output::IMEOutput { purpose: egui::IMEPurpose::Normal, rect, cursor_rect, should_interrupt_composition: false }));
            }
        });
        zoom_badge(ui.painter(), view_rect, zoom);
    }
}

impl Editor {
    fn new(content: Content, title: String, viewport: ViewportId, root_hwnd: isize, place: Option<[i32; 2]>, shared_tip: Arc<Mutex<String>>, min_width: f32) -> Self {
        let (note_tx, note_rx) = channel();
        Self {
            content,
            title,
            viewport,
            root_hwnd,
            hwnd: None,
            bar: win::TitleBar::default(),
            bar_tried: false,
            place,
            settle: SETTLE_FRAMES,
            raise: RAISE_FRAMES,
            min_width,
            popup_guard: PopupClickGuard::default(),
            note: None,
            note_tx,
            note_rx,
            new_tip: String::new(),
            shared_tip,
            new_requested: false,
            closed: false,
            saving: Arc::default(),
            save_waiting: 0,
        }
    }

    pub fn snip(&self) -> Option<&Snip> {
        match &self.content {
            Content::Snip(s) => Some(s),
            Content::Video(_) => None,
        }
    }

    /// Shows `msg` as this window's toast. Call only from this window's own frame; other threads and
    /// windows use `notifier`, which also wakes this window.
    pub fn say(&mut self, msg: String) {
        self.note = Some(Note::new(msg));
    }

    pub fn notifier(&self, ctx: &egui::Context) -> Notifier {
        Notifier::new(self.note_tx.clone(), ctx.clone(), self.viewport)
    }

    /// One frame of this editor's window.
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.closed {
            self.closed = true;
            ctx.request_repaint_of(ViewportId::ROOT);
        }
        while let Ok(msg) = self.note_rx.try_recv() {
            self.say(msg);
        }
        // The shell holds this lock only to replace the string, so a busy one just waits for the next frame.
        if let Ok(tip) = self.shared_tip.try_lock() {
            if *tip != self.new_tip {
                self.new_tip.clone_from(&tip);
            }
        }
        self.attach_title_bar(&ctx);
        self.retry_save(&ctx);
        self.settle_window(&ctx);
        self.raise(&ctx);
        self.popup_guard.begin_frame(&ctx);
        self.shortcuts(&ctx);
        self.toolbar(ui);
        let mut video_top = None;
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ui.style().visuals.extreme_bg_color)).show(ui, |ui| {
            if let Content::Snip(snip) = &mut self.content {
                snip.canvas(ui, &self.popup_guard);
            } else {
                video_top = Some(ui.max_rect().top());
                self.video_view(ui);
            }
        });
        if let Some(msg) = widgets::live_note(&mut self.note, &ctx) {
            widgets::toast(&ctx, &msg, video_top);
        }
        let ime_taken = match &mut self.content {
            Content::Snip(s) => std::mem::take(&mut s.ime_taken),
            Content::Video(_) => false,
        };
        if ime_taken {
            if let Some(hwnd) = self.own_hwnd() {
                win::cancel_ime_composition(hwnd);
            }
        }
    }

    /// The window's own handle, once Windows has created it.
    fn own_hwnd(&mut self) -> Option<HWND> {
        if self.hwnd.is_none() {
            self.hwnd = win::find_window(&self.title).map(|h| h as isize);
        }
        self.hwnd.map(|h| h as HWND)
    }

    /// Takes over the window's caption on the first frame its handle can be found.
    fn attach_title_bar(&mut self, ctx: &egui::Context) {
        if self.bar_tried {
            return;
        }
        let Some(hwnd) = self.own_hwnd() else { return };
        self.bar_tried = true;
        let (wake, id) = (ctx.clone(), self.viewport);
        if let Err(e) = self.bar.attach(hwnd, move || wake.request_repaint_of(id)) {
            self.say(e);
        }
        ctx.request_repaint();
    }

    /// Brings the new window to the front once it is on screen. eframe shows it with a plain
    /// `ShowWindow`, which Windows' foreground lock leaves behind the active app unless this
    /// process may take the foreground at that moment; a notification click hands that right to the
    /// `--open-link` launch, and it is often gone by the time the window appears. winit's focus
    /// gets past the lock.
    fn raise(&mut self, ctx: &egui::Context) {
        if self.raise == 0 {
            return;
        }
        self.raise -= 1;
        match self.own_hwnd() {
            Some(hwnd) if win::is_visible(hwnd) => {
                self.raise = 0;
                if !win::is_foreground(hwnd) {
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                }
            }
            _ => ctx.request_repaint(),
        }
    }

    /// A Save asked for before this window's handle was known runs once it is.
    fn retry_save(&mut self, ctx: &egui::Context) {
        if self.save_waiting == 0 {
            return;
        }
        self.save_waiting -= 1;
        if self.own_hwnd().is_some() {
            self.save_waiting = 0;
            self.save_as(ctx);
        } else if self.save_waiting == 0 {
            self.say("Couldn't open Save: this window isn't ready. Try again in a moment".into());
        } else {
            ctx.request_repaint();
        }
    }

    /// First frames only: move to the chosen corner, then keep the window inside its monitor once
    /// Windows has finished any rescaling for that monitor.
    fn settle_window(&mut self, ctx: &egui::Context) {
        if self.settle == 0 {
            return;
        }
        self.settle -= 1;
        if let Some(hwnd) = self.own_hwnd() {
            win::place_on_screen(hwnd, self.place.take());
        }
        ctx.request_repaint();
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        // Popups close themselves on Esc and a focused button activates itself on Enter, so those keys are left to them.
        let (popup_open, focused) = (egui::Popup::is_any_open(ctx), ctx.memory(|m| m.focused().is_some()));
        // While text is being typed Enter belongs to it, as it would to a focused widget.
        let typing = matches!(&self.content, Content::Snip(s) if s.typing());
        let Keys { new, save, undo, redo, copy, enter, esc, zoom_in, zoom_out, zoom_fit } = ctx.input_mut(|i| read_keys(i, popup_open, focused || typing));
        if new {
            self.request_new(ctx);
        }
        if save {
            self.save_as(ctx);
        }
        if copy {
            self.copy(ctx);
        }
        let Content::Snip(snip) = &mut self.content else { return };
        if esc && snip.typing() {
            // Esc throws away the text being typed, and nothing else.
            snip.drawing = None;
        } else {
            if undo {
                snip.step(false);
            }
            if redo {
                snip.step(true);
            }
        }
        if enter {
            snip.apply_crop();
        }
        if esc && !typing {
            snip.cancel_crop();
        }
        if zoom_fit {
            snip.zoom = None;
        } else if zoom_in != zoom_out {
            snip.key_zoom = if zoom_in { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        }
    }

    fn request_new(&mut self, ctx: &egui::Context) {
        self.new_requested = true;
        ctx.request_repaint_of(ViewportId::ROOT);
    }

    /// With a snip open the toolbar needs room for all three groups; a video's toolbar is much narrower.
    fn sync_min_width(&mut self, ctx: &egui::Context) {
        let monitor_w = ctx.input(|i| i.viewport().monitor_size).map_or(f32::INFINITY, |m| m.x);
        let want = min_width_for(&self.content, monitor_w);
        if want != self.min_width {
            self.min_width = want;
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(vec2(want, MIN_WINDOW_H)));
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        self.sync_min_width(&ui.ctx().clone());
        let bar = self.bar.clone();
        widgets::title_bar(ui, "editor_toolbar", &bar, |ui, row| {
            let layout = |l| egui::UiBuilder::new().max_rect(row).layout(l);

            let mut left = ui.new_child(layout(Layout::left_to_right(Align::Center)));
            if widgets::new_button(&mut left, &self.new_tip).clicked() {
                self.request_new(&left.ctx().clone());
            }
            let left_end = left.min_rect().max.x;

            let mut right = ui.new_child(layout(Layout::right_to_left(Align::Center)));
            self.output_controls(&mut right);
            let right_start = right.min_rect().min.x;
            let mut groups = vec![left.min_rect(), right.min_rect()];

            if let Content::Snip(snip) = &mut self.content {
                let width = snip.markup_width();
                // Centred on the window, as the original centres its tools, unless the groups beside push it.
                let x = markup_x(ui.max_rect().center().x, width, left_end, right_start);
                let mid_rect = Rect::from_min_size(pos2(x, row.min.y), vec2(width + 1.0, BTN.y));
                let mut mid = ui.new_child(egui::UiBuilder::new().max_rect(mid_rect).layout(Layout::left_to_right(Align::Center)));
                snip.markup_controls(&mut mid);
                groups.push(mid.min_rect());
            }
            groups
        });
    }

    fn output_controls(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let more = icon_button(ui, Icon::More, None, false, true, "More");
        egui::Popup::menu(&more).show(|ui| {
            if ui.button("Open the Screenshots folder").clicked() {
                if let Some(dir) = win::pictures_dir().map(|d| d.join("Screenshots")) {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            }
            if let Content::Snip(snip) = &mut self.content {
                ui.separator();
                if ui.button("Zoom to fit").clicked() {
                    snip.zoom = None;
                }
                if ui.button("Actual size (100%)").clicked() {
                    snip.zoom = Some(1.0);
                }
            }
        });
        divider(ui);
        match self.content {
            Content::Video(_) => {
                if icon_button(ui, Icon::Save, None, false, true, "Save a copy (Ctrl+S)").clicked() {
                    self.save_as(&ctx);
                }
            }
            Content::Snip(_) => {
                if icon_button(ui, Icon::Copy, None, false, true, "Copy with markup (Ctrl+C)").clicked() {
                    self.copy(&ctx);
                }
                if icon_button(ui, Icon::Save, None, false, true, "Save as (Ctrl+S)").clicked() {
                    self.save_as(&ctx);
                }
            }
        }
    }

    fn copy(&mut self, ctx: &egui::Context) {
        let Content::Snip(snip) = &mut self.content else { return };
        // What is copied is what is on screen, text still being typed included.
        snip.finish_text();
        let (image, hwnd, report) = (snip.flattened(), self.root_hwnd, self.notifier(ctx));
        std::thread::spawn(move || {
            report.send(match win::copy_image(hwnd as HWND, &image) {
                Ok(()) => "Copied to clipboard".to_string(),
                Err(e) => format!("Couldn't copy: {e}"),
            });
        });
    }

    /// Asks where to save, then writes the file, all on a worker thread: the dialog is modal to this
    /// window alone, so the other windows (the recording bar among them) keep running, and a big PNG
    /// takes a moment to encode, a long recording a moment to copy. A second Save while one is under
    /// way is ignored.
    fn save_as(&mut self, ctx: &egui::Context) {
        // The dialog is modal to its owner, which must be this window: owned by the shell's, on
        // another thread's terms, it would block the home window or the recording bar instead.
        let Some(owner) = self.own_hwnd() else {
            self.save_waiting = SAVE_WAIT_FRAMES;
            ctx.request_repaint();
            return;
        };
        if self.saving.swap(true, Ordering::AcqRel) {
            return;
        }
        let busy = Busy(self.saving.clone());
        if let Content::Snip(snip) = &mut self.content {
            snip.finish_text();
        }
        // Window handles are plain numbers to Windows; this one is only used to own the dialog.
        let (owner, report) = (owner as isize, self.notifier(ctx));
        let job: Box<dyn FnOnce() + Send> = match &self.content {
            Content::Snip(snip) => {
                let (start, name, image) = (win::pictures_dir().map(|d| d.join("Screenshots")), format!("Snip {}", win::timestamp()), snip.flattened());
                Box::new(move || {
                    let _busy = busy;
                    let path = match win::ask_save_path(owner as HWND, start.as_deref(), &name, ("PNG image", "png")) {
                        Ok(Some(path)) => path,
                        Ok(None) => return,
                        Err(e) => return report.send(format!("Couldn't save: {e}")),
                    };
                    let png = win::encode_png(&image);
                    // Let go of the picture once it is encoded, so the editor's next mark needn't copy it.
                    drop(image);
                    report.send(saved_note(&path, png.and_then(|bytes| std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display())))));
                })
            }
            Content::Video(v) => {
                let start = win::videos_dir().map(|d| d.join("Screen Recordings"));
                let name = v.path.file_stem().map_or_else(|| "Screen Recording".into(), |n| n.to_string_lossy().into_owned());
                let from = v.path.clone();
                Box::new(move || {
                    let _busy = busy;
                    let path = match win::ask_save_path(owner as HWND, start.as_deref(), &name, ("MP4 video", "mp4")) {
                        Ok(Some(path)) => path,
                        Ok(None) => return,
                        Err(e) => return report.send(format!("Couldn't save: {e}")),
                    };
                    report.send(saved_note(&path, std::fs::copy(&from, &path).map(|_| ()).map_err(|e| e.to_string())));
                })
            }
        };
        std::thread::spawn(job);
    }

    fn video_view(&mut self, ui: &mut egui::Ui) {
        let Content::Video(v) = &self.content else { return };
        let ppp = ui.ctx().pixels_per_point();
        let avail = ui.available_rect_before_wrap();
        let room = Rect::from_min_max(avail.min + vec2(24.0, 24.0), avail.max - vec2(24.0, 110.0));
        let natural = vec2(v.size[0] as f32, v.size[1] as f32) / ppp;
        let scale = (room.size() / natural).min_elem().clamp(0.02, 1.0);
        let shown = Rect::from_center_size(room.center(), natural * scale);
        let resp = ui.interact(shown, ui.scope_id().with("video"), Sense::click()).on_hover_text("Play");
        let p = ui.painter();
        match &v.thumb {
            Some(t) => {
                p.image(t.id(), shown, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                p.rect_filled(shown, 0.0, Color32::BLACK);
            }
        }
        p.circle_filled(shown.center(), 30.0, Color32::from_black_alpha(if resp.hovered() { 190 } else { 150 }));
        icons::paint(p, Rect::from_center_size(shown.center() + vec2(2.0, 0.0), vec2(28.0, 28.0)), Icon::Play, Color32::WHITE, None);
        if resp.hovered() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
        let secs = v.seconds.round() as u64;
        let info = format!("Screen recording · {}:{:02} · {} × {}", secs / 60, secs % 60, v.size[0], v.size[1]);
        p.text(shown.center_bottom() + vec2(0.0, 22.0), Align2::CENTER_CENTER, info, FontId::proportional(14.0), ICON);
        let buttons = Rect::from_center_size(shown.center_bottom() + vec2(0.0, 62.0), vec2(220.0, 32.0));
        let path = v.path.clone();
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(buttons).layout(Layout::left_to_right(Align::Center)));
        let button = |ui: &mut egui::Ui, text: &str| ui.add(egui::Button::new(text).fill(BUTTON_HOVER).min_size(vec2(100.0, 32.0))).clicked();
        let (play, folder) = (button(&mut row, "Play"), button(&mut row, "Show in folder"));
        if resp.clicked() || play {
            if let Err(e) = win::open_path(&path) {
                self.say(format!("Couldn't open the video: {e}"));
            }
        }
        if folder {
            if let Err(e) = win::show_in_folder(&path) {
                self.say(format!("Couldn't open the folder: {e}"));
            }
        }
    }
}

fn saved_note(path: &std::path::Path, result: Result<(), String>) -> String {
    match result {
        Ok(()) => format!("Saved {}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into())),
        Err(e) => format!("Couldn't save: {e}"),
    }
}

fn min_width_for(content: &Content, monitor_w: f32) -> f32 {
    match content {
        Content::Snip(_) => edit_min_width(monitor_w),
        Content::Video(_) => MIN_WINDOW_W,
    }
}

/// One open editor window. The builder is made once and reused every frame, because egui turns any
/// difference between frames into window commands.
struct Slot {
    id: ViewportId,
    title: String,
    builder: ViewportBuilder,
    editor: Arc<Mutex<Editor>>,
    /// The editor's toast channel, so the shell can reach it without taking the editor's lock.
    note_tx: Sender<String>,
}

/// All open editor windows, oldest first.
pub struct Editors {
    slots: Vec<Slot>,
    next: u64,
    icon: Option<Arc<egui::IconData>>,
    /// The New buttons' tooltip, shared with every editor.
    new_tip: Arc<Mutex<String>>,
}

/// What the editor windows asked of the shell since the last `Editors::poll`.
#[derive(Debug, Default, PartialEq)]
pub struct Requests {
    pub new_snip: bool,
    pub closed: usize,
}

impl Editors {
    pub fn new(icon: Option<Arc<egui::IconData>>) -> Self {
        Self { slots: Vec::new(), next: 0, icon, new_tip: Arc::default() }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether window `id` is still open.
    pub fn contains(&self, id: ViewportId) -> bool {
        self.slots.iter().any(|s| s.id == id)
    }

    #[cfg(test)]
    pub fn ids(&self) -> Vec<ViewportId> {
        self.slots.iter().map(|s| s.id).collect()
    }

    /// Runs `f` on the editor in window `id`, if it is still open and not in the middle of a frame.
    /// The root never waits on an editor: windows run on this same thread, so a busy lock means a
    /// re-entry that waiting would deadlock.
    pub fn with<R>(&self, id: ViewportId, f: impl FnOnce(&mut Editor) -> R) -> Option<R> {
        let slot = self.slots.iter().find(|s| s.id == id)?;
        let mut ed = slot.editor.try_lock().ok()?;
        Some(f(&mut ed))
    }

    /// Opens a snip in a new window. The tools carry over from the newest snip editor. Returns the
    /// window, a notifier for its toast, and the picture (for the automatic copy and save).
    pub fn open_snip(&mut self, ctx: &egui::Context, image: ColorImage, root_hwnd: isize, corner: Option<[i32; 2]>) -> (ViewportId, Notifier, Arc<ColorImage>) {
        let tools = self.slots.iter().rev().find_map(|s| s.editor.try_lock().ok().and_then(|e| e.snip().map(|s| s.tools))).unwrap_or_default();
        let snip = Snip::new(image, tools);
        let image = snip.doc.image.clone();
        let (id, report) = self.open(ctx, Content::Snip(Box::new(snip)), root_hwnd, corner);
        (id, report, image)
    }

    /// Opens a finished recording in a new window, with `msg` as its first toast.
    pub fn open_video(&mut self, ctx: &egui::Context, video: VideoResult, msg: String, root_hwnd: isize, corner: Option<[i32; 2]>) -> ViewportId {
        let (id, report) = self.open(ctx, Content::Video(video), root_hwnd, corner);
        report.send(msg);
        id
    }

    fn open(&mut self, ctx: &egui::Context, content: Content, root_hwnd: isize, corner: Option<[i32; 2]>) -> (ViewportId, Notifier) {
        self.next += 1;
        let id = ViewportId::from_hash_of(("snipscreen editor", self.next));
        let kind = match content {
            Content::Snip(_) => "Snip",
            Content::Video(_) => "Recording",
        };
        let title = unique_title(&format!("Snipscreen — {kind} {}", clock(&win::timestamp())), self.slots.iter().map(|s| s.title.as_str()));
        let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
        let px = match &content {
            Content::Snip(s) => vec2(s.doc.size()[0] as f32, s.doc.size()[1] as f32),
            Content::Video(v) => vec2(v.size[0] as f32, v.size[1] as f32 + 90.0),
        };
        let min_w = min_width_for(&content, monitor.x);
        let mut builder = ViewportBuilder::default()
            .with_title(title.clone())
            .with_inner_size(edit_window_size(px / ctx.pixels_per_point(), monitor))
            .with_min_inner_size([min_w, MIN_WINDOW_H]);
        if let Some(icon) = &self.icon {
            builder = builder.with_icon(icon.clone());
        }
        let corner = corner.map(|[x, y]| {
            let step = CASCADE * (self.slots.len() % 8) as i32;
            [x + step, y + step]
        });
        let editor = Editor::new(content, title.clone(), id, root_hwnd, corner, self.new_tip.clone(), min_w);
        let report = editor.notifier(ctx);
        let note_tx = editor.note_tx.clone();
        self.slots.push(Slot { id, title, builder, editor: Arc::new(Mutex::new(editor)), note_tx });
        ctx.request_repaint_of(id);
        (id, report)
    }

    /// Registers every editor window for this frame. Must run on every pass of the root window:
    /// egui closes a viewport whose parent stops showing it.
    pub fn show(&self, ctx: &egui::Context) {
        for slot in &self.slots {
            let editor = slot.editor.clone();
            ctx.show_viewport_deferred(slot.id, slot.builder.clone(), move |ui, _| {
                editor.lock().unwrap_or_else(PoisonError::into_inner).ui(ui);
            });
        }
    }

    /// Collects what the windows asked for and drops the ones that were closed.
    pub fn poll(&mut self) -> Requests {
        let mut req = Requests::default();
        self.slots.retain(|slot| {
            // Only this thread runs the windows and it never re-enters one, so the lock is free;
            // `try_lock` keeps a surprise from ever stalling the shell.
            let Ok(mut ed) = slot.editor.try_lock() else { return true };
            req.new_snip |= std::mem::take(&mut ed.new_requested);
            if ed.closed {
                req.closed += 1;
            }
            !ed.closed
        });
        req
    }

    /// Keeps every New button's tooltip in step with the shell's capture mode and hotkey. Each editor
    /// picks the new text up on its next frame.
    pub fn set_new_tip(&mut self, tip: String) {
        let mut shared = self.new_tip.lock().unwrap_or_else(PoisonError::into_inner);
        if *shared != tip {
            *shared = tip;
        }
    }

    /// The toast of the newest editor window; `None` when none is open.
    pub fn newest_notifier(&self, ctx: &egui::Context) -> Option<Notifier> {
        let slot = self.slots.last()?;
        Some(Notifier::new(slot.note_tx.clone(), ctx.clone(), slot.id))
    }
}

/// "14:15:30" from a `win::timestamp` such as "2026-11-05 141530".
fn clock(timestamp: &str) -> String {
    let t = timestamp.rsplit(' ').next().unwrap_or(timestamp);
    if t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit()) {
        format!("{}:{}:{}", &t[0..2], &t[2..4], &t[4..6])
    } else {
        t.to_string()
    }
}

/// `want`, or `want (2)`, `want (3)`… if an open window already has that title. Titles are how an
/// editor finds its own window handle, so they must not repeat.
fn unique_title<'a>(want: &str, taken: impl Iterator<Item = &'a str> + Clone) -> String {
    let free = |t: &str| !taken.clone().any(|x| x == t);
    if free(want) {
        return want.to_string();
    }
    (2..).map(|n| format!("{want} ({n})")).find(|t| free(t)).unwrap_or_else(|| want.to_string())
}

/// The canvas texture, cut into pieces no wider or taller than the GPU takes (`max_texture_side`, as
/// low as 8192 on some integrated GPUs; three 4K monitors side by side are 11520 wide). A picture
/// that fits is one piece sharing `shown`'s pixels.
// ceiling: neighbouring pieces are filtered separately, so a faint seam can show where they meet when
// the snip is scaled; overlap the pieces by a pixel if anyone ever sees it.
struct Chunks {
    ctx: egui::Context,
    /// Each piece and the image pixels it shows.
    parts: Vec<(PxRect, TextureHandle)>,
}

impl Chunks {
    fn new(ctx: &egui::Context, img: &Arc<ColorImage>) -> Self {
        let mut chunks = Self { ctx: ctx.clone(), parts: Vec::new() };
        chunks.set(img);
        chunks
    }

    /// The pieces' rectangles for a picture of `size`: tile-aligned, so a tile run splits cleanly.
    fn layout(&self, size: [usize; 2]) -> Vec<PxRect> {
        let side = (self.ctx.input(|i| i.max_texture_side) / markup::TILE).max(1) * markup::TILE;
        let mut out = Vec::new();
        for y0 in (0..size[1].max(1)).step_by(side) {
            for x0 in (0..size[0].max(1)).step_by(side) {
                out.push(PxRect { x0, y0, x1: (x0 + side).min(size[0]), y1: (y0 + side).min(size[1]) });
            }
        }
        out
    }

    /// Replaces the whole picture.
    fn set(&mut self, img: &Arc<ColorImage>) {
        let layout = self.layout(img.size);
        let piece = |r: &PxRect| if layout.len() == 1 { egui::ImageData::Color(img.clone()) } else { markup::sub_image(img, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0).into() };
        if layout.len() == self.parts.len() && layout.iter().zip(&self.parts).all(|(a, (b, _))| a == b) {
            for (r, tex) in &mut self.parts {
                tex.set(piece(r), TextureOptions::LINEAR);
            }
        } else {
            self.parts = layout.iter().map(|r| (*r, self.ctx.load_texture("snip", piece(r), TextureOptions::LINEAR))).collect();
        }
    }

    /// Replaces the pixels from `at` on with `patch`, in whichever pieces they fall.
    fn set_partial(&mut self, at: [usize; 2], patch: ColorImage) {
        let r = PxRect { x0: at[0], y0: at[1], x1: at[0] + patch.size[0], y1: at[1] + patch.size[1] };
        if let [(_, tex)] = self.parts.as_mut_slice() {
            tex.set_partial(at, patch, TextureOptions::LINEAR);
            return;
        }
        for (c, tex) in &mut self.parts {
            let (x0, y0, x1, y1) = (r.x0.max(c.x0), r.y0.max(c.y0), r.x1.min(c.x1), r.y1.min(c.y1));
            if x0 < x1 && y0 < y1 {
                let piece = markup::sub_image(&patch, x0 - r.x0, y0 - r.y0, x1 - x0, y1 - y0);
                tex.set_partial([x0 - c.x0, y0 - c.y0], piece, TextureOptions::LINEAR);
            }
        }
    }

    /// Draws the picture, `size` pixels, into `rect` (points).
    fn paint(&self, painter: &egui::Painter, rect: Rect, size: [usize; 2]) {
        let at = |x: usize, y: usize| rect.min + vec2(x as f32 / size[0] as f32 * rect.width(), y as f32 / size[1] as f32 * rect.height());
        for (c, tex) in &self.parts {
            painter.image(tex.id(), Rect::from_min_max(at(c.x0, c.y0), at(c.x1, c.y1)), Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
        }
    }
}

/// The snip as shown: the picture with committed marks burned in by the same rasteriser that saves
/// it. A mark still being drawn is painted over `shown` on the CPU, tile run by tile run, straight
/// into the texture, so the preview is pixel for pixel what the mark will burn (multiply included).
struct CanvasTex {
    tex: Chunks,
    /// The picture and its committed marks: exactly what copy and save write.
    shown: Arc<ColorImage>,
    /// `(image_rev, ink_rev)` of the doc that `shown` was built from.
    rev: (u64, u64),
    /// The marks burned into `shown`, to tell which ones an erase or undo took away or brought back.
    burned: Vec<Arc<Mark>>,
    live: Live,
    /// A private copy of `shown` being made on a worker while a mark is drawn, and the picture it copies.
    copying: Option<(Arc<ColorImage>, Receiver<ColorImage>)>,
}

/// The texture tiles that show a mark still being drawn instead of `shown`.
struct Live {
    tiles: Tiles,
    /// Points of the stroke already painted into `tiles`.
    done: usize,
    /// A shape, as last painted.
    last: Option<Mark>,
    /// Text being typed, as last painted, with the tiles of it rendered so far.
    text: Option<TextRaster>,
    /// The pixels the texture shows in each live tile (by tile index), so a commit burns them into
    /// `shown` as they are instead of painting the mark again.
    patches: HashMap<usize, Vec<Color32>>,
    /// Tiles out of view whose texture is behind: they should show the text being typed (those in
    /// `tiles`) or the picture (the rest). Brought up to date as they come into view, a few at a
    /// time on frames where the text doesn't change, and by a commit.
    pending: Tiles,
}

impl Live {
    fn new(size: [usize; 2]) -> Self {
        Self { tiles: Tiles::new(size), done: 0, last: None, text: None, patches: HashMap::new(), pending: Tiles::new(size) }
    }

    /// Every tile whose texture differs from the picture or is behind.
    fn touched(&self) -> Tiles {
        let mut t = self.tiles.clone();
        t.union(&self.pending);
        t
    }
}

/// Time a frame on which the text being typed didn't change spends on its tiles out of view.
const PENDING_BUDGET: std::time::Duration = std::time::Duration::from_millis(4);

impl CanvasTex {
    /// The canvas for `doc`, flattened in full.
    fn new(ctx: &egui::Context, doc: &Doc) -> Self {
        let shown = if doc.marks.is_empty() { doc.image.clone() } else { Arc::new(doc.flatten()) };
        let tex = Chunks::new(ctx, &shown);
        let live = Live::new(shown.size);
        Self { tex, shown, rev: (doc.image_rev, doc.ink_rev), burned: doc.marks.clone(), live, copying: None }
    }

    /// Makes sure `shown` will be this canvas's own when the mark being drawn is burned into it. Until
    /// the first mark it is the doc's bare picture (which undo needs untouched), and a copy or save
    /// worker may still hold it; copying 33 MB of 4K picture then would stall the mouse-up, so the copy
    /// is made on a worker from the press on (a click is a mark too) and adopted once it is ready, if
    /// `adopt` (a mark is being drawn).
    fn own_picture(&mut self, adopt: bool) {
        if self.copying.is_some() {
            if adopt {
                self.adopt_copy(false);
            }
        } else if Arc::strong_count(&self.shown) > 1 {
            let (tx, rx) = channel();
            let from = self.shown.clone();
            let source = from.clone();
            std::thread::spawn(move || {
                let _ = tx.send((*source).clone());
            });
            self.copying = Some((from, rx));
        }
    }

    /// Takes the copy `own_picture` started, if it is ready (or, with `wait`, once it is: it is
    /// already under way, and sooner done than a copy made from scratch) and still of the picture shown.
    fn adopt_copy(&mut self, wait: bool) {
        if let Some((from, rx)) = &self.copying {
            let copy = if wait { rx.recv().map_err(|_| std::sync::mpsc::TryRecvError::Disconnected) } else { rx.try_recv() };
            match copy {
                Ok(copy) => {
                    if Arc::ptr_eq(from, &self.shown) {
                        self.shown = Arc::new(copy);
                    }
                    self.copying = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.copying = None,
            }
        }
    }

    /// Brings the picture and texture up to date with `doc`. When only marks changed (an erase, an
    /// undo of a mark) just the tiles those marks cover are redrawn and uploaded; a new picture
    /// (crop, or undoing one) is flattened and uploaded whole.
    fn sync(&mut self, doc: &mut Doc) {
        let rev = (doc.image_rev, doc.ink_rev);
        if self.rev == rev {
            return;
        }
        if self.rev.0 == rev.0 {
            let mut tiles = doc.changed_tiles(&self.burned);
            tiles.union(&self.live.touched());
            let shown = Arc::make_mut(&mut self.shown);
            for (r, patch) in doc.render_runs(&tiles) {
                markup::paste(shown, &patch, r.pos());
                self.tex.set_partial(r.pos(), patch);
            }
        } else {
            self.shown = if doc.marks.is_empty() { doc.image.clone() } else { Arc::new(doc.flatten()) };
            self.tex.set(&self.shown);
        }
        self.rev = rev;
        self.burned = doc.marks.clone();
        self.live = Live::new(self.shown.size);
    }

    /// Uploads the runs of `tiles` with `raster` painted over the committed picture, or plain when
    /// it is `None`, and remembers what each live tile now shows.
    fn upload(&mut self, tiles: &Tiles, mut raster: Option<&mut markup::Raster>) {
        let cols = self.shown.size[0].div_ceil(markup::TILE);
        for r in tiles.runs() {
            let mut patch = markup::sub_image(&self.shown, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
            let first = (r.y0 / markup::TILE) * cols + r.x0 / markup::TILE;
            if let Some(raster) = raster.as_deref_mut() {
                raster.paint(&mut patch, r.pos());
                for (i, x) in (r.x0..r.x1).step_by(markup::TILE).enumerate() {
                    let w = markup::TILE.min(r.x1 - x);
                    let mut pixels = Vec::with_capacity(w * patch.size[1]);
                    for row in patch.pixels.chunks(patch.size[0]) {
                        pixels.extend_from_slice(&row[x - r.x0..x - r.x0 + w]);
                    }
                    self.live.patches.insert(first + i, pixels);
                }
            } else {
                for i in 0..(r.x1 - r.x0).div_ceil(markup::TILE) {
                    self.live.patches.remove(&(first + i));
                }
            }
            self.tex.set_partial(r.pos(), patch);
        }
    }

    /// Adds a finished mark, burning just that mark into the picture and texture rather than
    /// re-flattening the whole snip. Tiles the texture already shows with the mark (it was drawn
    /// live) are copied into the picture as shown, neither painted again nor uploaded.
    fn commit(&mut self, doc: &mut Doc, m: Mark) {
        let current = self.rev == (doc.image_rev, doc.ink_rev);
        let size = doc.size();
        let live_text = self.live.text.take().filter(|r| matches!(&m, Mark::Text(t) if r.text() == t));
        // The live tiles that show `m` exactly as it will be burned.
        let as_shown = match &m {
            _ if !current => Tiles::new(size),
            Mark::Text(_) if live_text.is_some() => self.live.tiles.clone(),
            Mark::Stroke(s) if self.live.last.is_none() && self.live.done > 0 && self.live.done <= s.points.len() => {
                // Points added since the last frame still have to be painted.
                let mut newer = Tiles::new(size);
                s.footprint_from(self.live.done, &mut newer);
                self.live.tiles.minus(&newer)
            }
            Mark::Shape(_) if self.live.last.as_ref() == Some(&m) => self.live.tiles.clone(),
            _ => Tiles::new(size),
        };
        let mut raster = match live_text {
            Some(text) => markup::Raster::of_text(&m, text),
            None => m.raster(size),
        };
        if current {
            let mut tiles = Tiles::new(size);
            m.footprint(&mut tiles);
            // Takes the copy made while the mark was drawn; without one `make_mut` copies here.
            self.adopt_copy(true);
            self.copying = None;
            let shown = Arc::make_mut(&mut self.shown);
            let cols = size[0].div_ceil(markup::TILE);
            let mut reused = tiles.intersect(&as_shown);
            reused.retain(|idx| self.live.patches.contains_key(&idx));
            for idx in reused.indices() {
                if let Some(pixels) = self.live.patches.get(&idx) {
                    let at = [(idx % cols) * markup::TILE, (idx / cols) * markup::TILE];
                    let w = markup::TILE.min(size[0] - at[0]);
                    markup::paste(shown, &ColorImage::new([w, pixels.len() / w], pixels.clone()), at);
                }
            }
            for r in tiles.minus(&reused).runs() {
                let mut patch = markup::sub_image(shown, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
                raster.paint(&mut patch, r.pos());
                markup::paste(shown, &patch, r.pos());
                self.tex.set_partial(r.pos(), patch);
            }
            // Live tiles the mark no longer reaches go back to the picture, as do tiles left behind.
            let stale = self.live.touched().minus(&tiles);
            self.upload(&stale, None);
        }
        match raster.into_text() {
            Some(text) => doc.add_text(text),
            None => doc.add_mark(m),
        }
        if current {
            self.rev.1 = doc.ink_rev;
            self.burned = doc.marks.clone();
        }
        self.live = Live::new(self.shown.size);
    }

    /// Shows `m`, the mark being drawn. A stroke only grows, so only the tiles its newest segments
    /// reach are repainted; text is repainted where its changed letters are; a shape is repainted
    /// where it is now; and the tiles a mark has left go back to the picture. Nothing is redone while
    /// the mark stays the same.
    fn show_live(&mut self, m: &Mark, visible: &Tiles) {
        let size = self.shown.size;
        let mut fresh = Tiles::new(size);
        let mut covered = Tiles::new(size);
        let mut raster = match m {
            Mark::Stroke(s) => {
                if s.points.len() < self.live.done {
                    self.clear_live();
                }
                if s.points.len() == self.live.done {
                    return;
                }
                s.footprint_from(self.live.done, &mut fresh);
                self.live.done = s.points.len();
                self.live.tiles.union(&fresh);
                let mut raster = m.raster(size);
                self.upload(&fresh, Some(&mut raster));
                return;
            }
            Mark::Text(t) => {
                let (text, changed) = match self.live.text.take() {
                    Some(text) if text.text() == t => (text, false),
                    // Typing changes a letter at a time: only the changed letters' tiles are repainted.
                    Some(mut text) => {
                        text.retext(t, &mut fresh);
                        (text, true)
                    }
                    None => {
                        let text = TextRaster::new(t, size);
                        fresh.add(text.bounds());
                        (text, true)
                    }
                };
                covered.add(text.bounds());
                let mut raster = markup::Raster::of_text(m, text);
                // Only the tiles in view are painted now (a size change redoes the whole text, which
                // on a big block is far more than one frame can paint); the rest wait, out of sight.
                if changed {
                    let mut stale = self.live.tiles.minus(&covered);
                    stale.union(&fresh.intersect(&covered));
                    self.live.pending.union(&stale);
                    self.live.tiles = covered;
                }
                self.catch_up(&mut raster, visible, !changed);
                self.live.text = raster.into_text();
                return;
            }
            Mark::Shape(_) => {
                if self.live.last.as_ref() == Some(m) {
                    return;
                }
                m.footprint(&mut covered);
                fresh = covered.clone();
                self.live.last = Some(m.clone());
                m.raster(size)
            }
        };
        let left = self.live.tiles.minus(&covered);
        self.upload(&left, None);
        self.live.tiles = covered.clone();
        self.upload(&fresh.intersect(&covered), Some(&mut raster));
        self.live.text = raster.into_text();
    }

    /// Brings the pending tiles in view up to date, and with `idle` some out of view as well, a
    /// row at a time within `PENDING_BUDGET`; asks for another frame while any are left.
    fn catch_up(&mut self, raster: &mut markup::Raster, visible: &Tiles, idle: bool) {
        if self.live.pending.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        let now = self.live.pending.intersect(visible);
        self.refresh(&now, raster);
        self.live.pending = self.live.pending.minus(&now);
        if idle {
            for run in self.live.pending.runs() {
                if started.elapsed() >= PENDING_BUDGET {
                    break;
                }
                let mut row = Tiles::new(self.shown.size);
                row.add(run.rect());
                self.refresh(&row, raster);
                self.live.pending = self.live.pending.minus(&row);
            }
        }
        // What a pending tile shows is behind, so a commit must not take it as shown.
        let behind: Vec<usize> = self.live.pending.indices().collect();
        for idx in behind {
            self.live.patches.remove(&idx);
        }
        if !self.live.pending.is_empty() {
            self.tex.ctx.request_repaint();
        }
    }

    /// Uploads `tiles` as they should be: the live text over the picture where it reaches, the picture elsewhere.
    fn refresh(&mut self, tiles: &Tiles, raster: &mut markup::Raster) {
        self.upload(&tiles.minus(&self.live.tiles), None);
        self.upload(&tiles.intersect(&self.live.tiles), Some(raster));
    }

    /// Puts back the committed picture wherever a cancelled mark was showing.
    fn clear_live(&mut self) {
        let tiles = self.live.touched();
        if !tiles.is_empty() {
            self.upload(&tiles, None);
        }
        self.live = Live::new(self.shown.size);
    }
}

/// Text worth placing: typed text with something visible in it. Anything else is dropped.
fn placeable(m: Option<Mark>) -> Option<Mark> {
    match m {
        Some(Mark::Text(t)) if !t.text.trim().is_empty() => Some(Mark::Text(t)),
        _ => None,
    }
}

/// Takes the text being typed to be placed, with an input method's composition, which shows after
/// it, taken in as if committed: what is placed is what was on screen. Sets `ime_taken` when it did.
fn take_typed(drawing: &mut Option<Mark>, draft: &mut Draft, ime_taken: &mut bool) -> Option<Mark> {
    let mut m = drawing.take();
    if let Some(Mark::Text(t)) = &mut m {
        if !draft.preedit.is_empty() {
            t.text.push_str(&std::mem::take(&mut draft.preedit));
            *ime_taken = true;
        }
    }
    placeable(m)
}

/// `t` with an input method's uncommitted `preedit` after it, as it shows while composing.
fn with_preedit(t: &Text, preedit: &str) -> Text {
    Text { text: format!("{}{preedit}", t.text), ..t.clone() }
}

/// What a change to the draft is, for grouping changes into undo steps.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Edit {
    #[default]
    None,
    Typing,
    Deleting,
}

/// Text being typed, beyond its letters (which live in the `Text` mark being drawn): the font size
/// it was last set from, the input method's composition, and its own undo history.
#[derive(Debug, Default)]
struct Draft {
    /// The size option, in points, that the text's size in pixels was last worked out from.
    size_pt: f32,
    /// What the input method is composing: shown after the text, not part of it until committed.
    preedit: String,
    undo: Vec<String>,
    redo: Vec<String>,
    /// The kind of the last change, so a run of typing (a word) or of deleting is one undo step.
    run: Edit,
}

impl Draft {
    fn new(size_pt: f32) -> Self {
        Self { size_pt, ..Self::default() }
    }

    /// Call before changing `text`: starts a new undo step unless the change continues the last
    /// run. Typing starts one at each new word; anything other than typing or deleting is a step of
    /// its own.
    fn begin(&mut self, kind: Edit, text: &str, new_word: bool) {
        if kind == Edit::None || kind != self.run || new_word {
            self.undo.push(text.to_string());
            self.redo.clear();
        }
        self.run = kind;
    }

    /// Takes back the last step. False when there is nothing to take back.
    fn undo(&mut self, text: &mut String) -> bool {
        let Some(before) = self.undo.pop() else { return false };
        self.redo.push(std::mem::replace(text, before));
        self.run = Edit::None;
        true
    }

    fn redo(&mut self, text: &mut String) {
        if let Some(after) = self.redo.pop() {
            self.undo.push(std::mem::replace(text, after));
            self.run = Edit::None;
        }
    }

    fn insert(&mut self, text: &mut String, s: &str, kind: Edit) {
        let s = clean(s);
        if s.is_empty() {
            return;
        }
        let new_word = kind == Edit::Typing && !s.starts_with(char::is_whitespace) && text.ends_with(|c: char| c.is_whitespace());
        self.begin(kind, text, new_word);
        text.push_str(&s);
    }
}

/// `s` as text can hold it: tabs become four spaces, and returns and other control characters go.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Applies this frame's typing to `text`, taking those events so nothing else reacts to them. An
/// input method's composition goes to `draft.preedit` until it is committed. True when Enter
/// (without Shift) asks to place the text.
fn type_into(ctx: &egui::Context, text: &mut String, draft: &mut Draft) -> bool {
    let mut place = false;
    ctx.input_mut(|i| {
        i.events.retain(|e| {
            match e {
                Event::Text(s) => draft.insert(text, s, Edit::Typing),
                Event::Paste(s) => draft.insert(text, s, Edit::None),
                Event::Ime(egui::ImeEvent::Preedit { text: s, .. }) => draft.preedit = clean(s).replace('\n', " "),
                Event::Ime(egui::ImeEvent::Commit(s)) => {
                    draft.preedit.clear();
                    draft.insert(text, s, Edit::None);
                }
                Event::Key { key: Key::Backspace, pressed: true, .. } => {
                    if !text.is_empty() {
                        draft.begin(Edit::Deleting, text, false);
                        text.pop();
                    }
                }
                Event::Key { key: Key::Enter, pressed: true, modifiers, .. } if modifiers.shift => draft.insert(text, "\n", Edit::None),
                Event::Key { key: Key::Enter, pressed: true, .. } => place = true,
                _ => return true,
            }
            false
        });
    });
    place
}

/// Where the canvas's scroll area is scrolled to, as of its last frame. Call from the `Ui` that holds it.
fn scroll_offset(ui: &egui::Ui) -> Vec2 {
    let id = ui.make_persistent_id(egui::IdSalt::new(SCROLL_ID));
    egui::scroll_area::State::load(ui.ctx(), id).map_or(Vec2::ZERO, |s| s.clamped_offset())
}

/// Scroll offset that keeps the spot under `anchor` (relative to the view's corner) where it was when
/// the snip is redrawn at `new_shown` instead of `old_shown`. The snip is centred in scrolling content
/// that is its size plus `CANVAS_MARGIN`, or the whole view if that is larger. All values in points.
fn anchored_offset(offset: Vec2, anchor: Vec2, old_shown: Vec2, new_shown: Vec2, view: Vec2) -> Vec2 {
    let snip_corner = |shown: Vec2| ((shown + CANVAS_MARGIN).max(view) - shown) / 2.0;
    let on_snip = (offset + anchor - snip_corner(old_shown)) / old_shown;
    (snip_corner(new_shown) + on_snip * new_shown - anchor).max(Vec2::ZERO)
}

/// Explicit zoom limits. The floor gives way to a smaller fit, so zooming out from a very large
/// snip never jumps up to the floor and enlarges it.
fn clamp_zoom(zoom: f32, fit: f32) -> f32 {
    zoom.clamp(MIN_ZOOM.min(fit), MAX_ZOOM)
}

fn zoom_label(zoom: f32) -> String {
    // Zoom 1.0 is one screen pixel per snip pixel.
    format!("{:.0}%", zoom * 100.0)
}

/// Current zoom in the bottom-right corner of the canvas, quiet enough to ignore.
fn zoom_badge(painter: &egui::Painter, view: Rect, zoom: f32) {
    let galley = painter.layout_no_wrap(zoom_label(zoom), FontId::proportional(11.0), ICON_OFF);
    let rect = Rect::from_min_size(view.right_bottom() - galley.size() - vec2(20.0, 16.0), galley.size()).expand2(vec2(6.0, 2.0));
    painter.rect_filled(rect, 4.0, TOOLBAR_BG.gamma_multiply(0.85));
    painter.galley(rect.min + vec2(6.0, 2.0), galley, ICON_OFF);
}

/// Undoes or redoes one step. The pending crop box is in pixels of the image it was drawn on,
/// and the restored image can be a different size, so the box is dropped rather than applied to it.
/// A mark still being drawn is in the old image's coordinates, so it is dropped as well.
fn step_history(doc: &mut Doc, crop: &mut Option<Rect>, drawing: &mut Option<Mark>, redo: bool) {
    if redo {
        doc.redo();
    } else {
        doc.undo();
    }
    *crop = None;
    *drawing = None;
}

/// Narrowest editing window: wide enough for all three toolbar groups, but never wider than the
/// monitor, where a window that cannot be reached edge to edge is worse than a crowded toolbar
/// (`markup_x` packs the groups against each other when space runs short).
fn edit_min_width(monitor_w: f32) -> f32 {
    MIN_EDIT_W.min(monitor_w.max(MIN_WINDOW_W))
}

/// Window size that shows a snip of `snip` points with its chrome, kept within 85% of the monitor.
fn edit_window_size(snip: Vec2, monitor: Vec2) -> Vec2 {
    let min = vec2(edit_min_width(monitor.x), 440.0);
    (snip + vec2(48.0, 120.0)).clamp(min, (monitor * 0.85).max(min))
}

/// Left edge of the markup group: centred in the toolbar when it fits clear of the New button and
/// output groups, otherwise pushed right up against the New button.
fn markup_x(center: f32, width: f32, left_end: f32, right_start: f32) -> f32 {
    (center - width / 2.0).min(right_start - GROUP_GAP - width).max(left_end + GROUP_GAP)
}

/// Editor shortcuts pressed this frame.
#[derive(Debug, Default, PartialEq)]
struct Keys {
    new: bool,
    save: bool,
    undo: bool,
    redo: bool,
    copy: bool,
    enter: bool,
    esc: bool,
    zoom_in: bool,
    zoom_out: bool,
    zoom_fit: bool,
}

/// `popup_open`: a popup or menu is showing. `focused`: a widget holds keyboard focus.
fn read_keys(i: &mut egui::InputState, popup_open: bool, focused: bool) -> Keys {
    let cmd = |k| KeyboardShortcut::new(Modifiers::COMMAND, k);
    // egui ignores an extra Shift when matching, so Ctrl+Shift+Z must be
    // consumed before Ctrl+Z or it would undo.
    let redo_shift = i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z));
    let zoom_in_plus = i.consume_shortcut(&cmd(Key::Plus));
    let zoom_in_equals = i.consume_shortcut(&cmd(Key::Equals));
    Keys {
        new: i.consume_shortcut(&cmd(Key::N)),
        save: i.consume_shortcut(&cmd(Key::S)),
        undo: i.consume_shortcut(&cmd(Key::Z)),
        redo: redo_shift | i.consume_shortcut(&cmd(Key::Y)),
        // egui turns Ctrl+C into a Copy event rather than a key press.
        copy: i.events.iter().any(|e| matches!(e, Event::Copy)),
        // Enter applies the crop only when nothing else claims it: with a button focused it activates that button.
        enter: !popup_open && !focused && i.consume_key(Modifiers::NONE, Key::Enter),
        esc: escape_for_app(i, popup_open),
        zoom_in: zoom_in_plus | zoom_in_equals,
        zoom_out: i.consume_shortcut(&cmd(Key::Minus)),
        zoom_fit: i.consume_shortcut(&cmd(Key::Num0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::{BAR_PAD, CAPTION_W};

    fn white(w: usize, h: usize) -> ColorImage {
        ColorImage::new([w, h], vec![Color32::WHITE; w * h])
    }

    #[test]
    fn undo_and_redo_drop_the_pending_crop_box() {
        let mut doc = Doc::new(white(100, 80));
        doc.crop(Rect::from_min_max(pos2(10.0, 10.0), pos2(60.0, 50.0)));
        assert_eq!(doc.size(), [50, 40]);
        let box_on_cropped = Some(Rect::from_min_max(pos2(5.0, 5.0), pos2(45.0, 35.0)));

        let mut crop = box_on_cropped;
        step_history(&mut doc, &mut crop, &mut None, false);
        assert_eq!(doc.size(), [100, 80], "undo restores the larger image");
        assert_eq!(crop, None, "the 50x40 box must not carry over to the 100x80 image");

        let mut crop = Some(Rect::from_min_max(pos2(80.0, 60.0), pos2(99.0, 79.0)));
        step_history(&mut doc, &mut crop, &mut None, true);
        assert_eq!(doc.size(), [50, 40], "redo re-crops");
        assert_eq!(crop, None, "a box drawn on the large image would fall outside the small one");
    }

    #[test]
    fn undo_drops_a_stroke_in_progress() {
        let mut doc = Doc::new(white(100, 80));
        doc.crop(Rect::from_min_max(pos2(10.0, 10.0), pos2(60.0, 50.0)));
        let mut drawing = Some(Mark::Stroke(Stroke { ink: Ink::Pen, color: Color32::RED, width: 4.0, points: vec![pos2(1.0, 1.0)] }));
        step_history(&mut doc, &mut None, &mut drawing, false);
        assert!(drawing.is_none(), "a stroke in the old image's coordinates must not land on the restored one");
    }

    #[test]
    fn anchored_offset_keeps_the_anchor_on_the_same_spot_of_the_snip() {
        let view = vec2(400.0, 300.0);
        let (old, new) = (vec2(800.0, 600.0), vec2(1000.0, 750.0));
        let (offset, anchor) = (vec2(150.0, 120.0), vec2(200.0, 150.0));
        let got = anchored_offset(offset, anchor, old, new, view);
        // The snip's corner sits at 16 in content space at both sizes (800+32 vs 1000+32 wide), so only the
        // scale about the anchor matters: new = (offset + anchor - 16) * 1.25 + 16 - anchor.
        assert_eq!(got, vec2((150.0 + 200.0 - 16.0) * 1.25 + 16.0 - 200.0, (120.0 + 150.0 - 16.0) * 1.25 + 16.0 - 150.0));
    }

    #[test]
    fn anchored_offset_never_goes_negative_and_handles_a_snip_smaller_than_the_view() {
        let view = vec2(400.0, 300.0);
        // Zooming out about the far corner of the view would ask for a negative offset.
        let off = anchored_offset(Vec2::ZERO, view - vec2(10.0, 10.0), vec2(1000.0, 800.0), vec2(200.0, 150.0), view);
        assert_eq!(off, Vec2::ZERO);
        // A snip that fits is centred, so growing it about the view centre keeps the centre.
        let off = anchored_offset(Vec2::ZERO, view / 2.0, vec2(100.0, 100.0), vec2(200.0, 200.0), view);
        assert_eq!(off, Vec2::ZERO);
    }

    /// Runs the canvas's scroll-area layout for one frame (viewport 400x300), optionally forcing the offset, and returns the offset it ends with.
    fn scroll_frame(ctx: &egui::Context, shown: Vec2, force: Option<Vec2>) -> Vec2 {
        let view = vec2(400.0, 300.0);
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, view)), ..Default::default() };
        let mut offset = Vec2::ZERO;
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                let mut area = egui::ScrollArea::both().id_salt(SCROLL_ID).auto_shrink(false);
                if let Some(f) = force {
                    area = area.scroll_offset(f);
                }
                area.show(ui, |ui| {
                    ui.allocate_exact_size((shown + CANVAS_MARGIN).max(view), Sense::click_and_drag());
                });
                offset = scroll_offset(ui);
            });
        });
        out.textures_delta.clear();
        offset
    }

    #[test]
    fn zooming_through_the_scroll_area_leaves_the_anchored_spot_in_place() {
        let (view, anchor) = (vec2(400.0, 300.0), vec2(120.0, 90.0));
        let sizes = [vec2(800.0, 600.0), vec2(1000.0, 750.0), vec2(500.0, 375.0), vec2(200.0, 150.0)];
        // The last pair grows a snip that fit the view, whose scroll range egui last measured as empty.
        for (old, new) in [(sizes[0], sizes[1]), (sizes[1], sizes[2]), (sizes[0], sizes[2]), (sizes[3], sizes[1])] {
            let ctx = egui::Context::default();
            // Let egui learn the content size before scrolling, as it has by the time the user zooms.
            for _ in 0..3 {
                scroll_frame(&ctx, old, None);
            }
            scroll_frame(&ctx, old, Some(vec2(150.0, 120.0)));
            let start = scroll_frame(&ctx, old, None);
            let want = anchored_offset(start, anchor, old, new, view);
            let got = scroll_frame(&ctx, new, Some(want));
            assert!((got - want).length() < 0.5, "egui moved the offset from {want:?} to {got:?} zooming {old:?} -> {new:?}");
        }
    }

    #[test]
    fn zoom_label_is_a_whole_percent() {
        assert_eq!(zoom_label(1.0), "100%");
        assert_eq!(zoom_label(0.625), "62%");
        assert_eq!(zoom_label(8.0), "800%");
    }

    #[test]
    fn zoom_out_never_enlarges_a_snip_that_fits_below_the_floor() {
        // Fit 3%: the 5% floor would make zooming out grow the snip.
        let fit = 0.03;
        assert!(clamp_zoom(fit / ZOOM_STEP, fit) <= fit);
        assert_eq!(clamp_zoom(fit * ZOOM_STEP, fit), fit * ZOOM_STEP, "zooming in works as usual");
        // Normal fit: the floor and ceiling hold.
        assert_eq!(clamp_zoom(0.01, 0.5), MIN_ZOOM);
        assert_eq!(clamp_zoom(100.0, 0.5), MAX_ZOOM);
    }

    #[test]
    fn editing_window_fits_a_narrow_monitor() {
        // 1280 px panel at 175% scaling is 731 pt wide; at 250% it is 512 pt.
        let size = edit_window_size(vec2(2000.0, 1000.0), vec2(731.0, 411.0));
        assert!(size.x <= 731.0, "window must fit the monitor width, got {}", size.x);
        let size = edit_window_size(vec2(2000.0, 1000.0), vec2(600.0, 338.0));
        assert!(size.x <= 600.0, "window must fit the monitor width, got {}", size.x);
        assert_eq!(edit_min_width(600.0), 600.0);
        assert_eq!(edit_min_width(512.0), MIN_WINDOW_W, "never below the plain window minimum");
        assert_eq!(edit_min_width(1920.0), MIN_EDIT_W, "a wide monitor keeps the full toolbar width");
        // Wide monitors: snip plus chrome, clamped to 85% of the monitor.
        let wide = vec2(1920.0, 1080.0);
        assert_eq!(edit_window_size(vec2(100.0, 100.0), wide), vec2(MIN_EDIT_W, 440.0));
        assert_eq!(edit_window_size(vec2(5000.0, 5000.0), wide), wide * 0.85);
    }

    /// Group widths as laid out by `Editor::toolbar`: New button 80, output 133, markup with crop buttons 430.
    const NEW_END: f32 = BAR_PAD + 72.0;
    const OUT_W: f32 = 117.0;
    const MARKUP_W: f32 = 382.0;
    /// Right end of the controls row: the caption buttons and the padding before them.
    const ROW_END_GAP: f32 = BAR_PAD + 3.0 * CAPTION_W;

    #[test]
    fn markup_width_matches_the_measured_toolbar() {
        let mut snip = Snip::new(white(4, 4), Tools::default());
        assert_eq!(snip.markup_width(), 8.0 * BTN.x + 2.0 * DIVIDER_W + 9.0 * GAP);
        snip.crop = Some(Rect::from_min_max(Pos2::ZERO, pos2(2.0, 2.0)));
        assert_eq!(snip.markup_width(), MARKUP_W);
        const { assert!(NEW_END + GROUP_GAP + MARKUP_W + GROUP_GAP + OUT_W + ROW_END_GAP <= MIN_EDIT_W, "MIN_EDIT_W fits all three groups") };
    }

    #[test]
    fn markup_group_never_overlaps_its_neighbours() {
        for window in [MIN_EDIT_W as i32, 900, 1200, 2000] {
            let row_end = window as f32 - ROW_END_GAP;
            let right_start = row_end - OUT_W;
            let x = markup_x(window as f32 / 2.0, MARKUP_W, NEW_END, right_start);
            assert!(x >= NEW_END + GROUP_GAP, "overlaps the New button at {window}");
            assert!(x + MARKUP_W <= right_start - GROUP_GAP, "overlaps output group at {window}");
        }
    }

    #[test]
    fn markup_group_centres_when_room_and_hugs_the_capture_group_otherwise() {
        assert_eq!(markup_x(500.0, 200.0, 100.0, 900.0), 400.0);
        assert_eq!(markup_x(500.0, 300.0, 100.0, 600.0), 284.0, "pushed left by the output group");
        assert_eq!(markup_x(500.0, 800.0, 100.0, 1000.0), 116.0, "pulled right by the capture group");
    }

    #[test]
    fn markup_group_survives_a_window_narrower_than_the_toolbar() {
        for window in [520.0_f32, 560.0, 600.0] {
            let row_end = window - ROW_END_GAP;
            let x = markup_x(window / 2.0, MARKUP_W, NEW_END, row_end - OUT_W);
            assert!(x.is_finite() && x >= NEW_END + GROUP_GAP, "packs against the New button at {window}");
        }
    }

    /// Keys seen by `read_keys` in one frame after pressing `key` with `mods`.
    fn press(key: Key, mods: Modifiers) -> Keys {
        press_in(key, mods, false, false)
    }

    /// Like `press`, with a popup open and/or a widget focused.
    fn press_in(key: Key, mods: Modifiers, popup_open: bool, focused: bool) -> Keys {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: mods }],
            ..Default::default()
        };
        let mut keys = Keys::default();
        let mut out = ctx.run_ui(input, |ui| keys = ui.ctx().input_mut(|i| read_keys(i, popup_open, focused)));
        out.textures_delta.clear();
        keys
    }

    #[test]
    fn esc_and_enter_act_when_nothing_else_wants_them() {
        assert_eq!(press(Key::Escape, Modifiers::NONE), Keys { esc: true, ..Default::default() });
        assert_eq!(press(Key::Enter, Modifiers::NONE), Keys { enter: true, ..Default::default() });
    }

    #[test]
    fn esc_is_left_to_an_open_popup() {
        assert_eq!(press_in(Key::Escape, Modifiers::NONE, true, false), Keys::default());
    }

    #[test]
    fn enter_is_left_to_an_open_popup_or_a_focused_widget() {
        assert_eq!(press_in(Key::Enter, Modifiers::NONE, true, false), Keys::default());
        assert_eq!(press_in(Key::Enter, Modifiers::NONE, false, true), Keys::default());
    }

    #[test]
    fn esc_still_cancels_with_a_widget_focused() {
        // egui drops keyboard focus on Esc itself, so a focused button must not hold the key back.
        assert_eq!(press_in(Key::Escape, Modifiers::NONE, false, true), Keys { esc: true, ..Default::default() });
    }

    #[test]
    fn ctrl_shift_z_redoes() {
        assert_eq!(press(Key::Z, Modifiers::COMMAND | Modifiers::SHIFT), Keys { redo: true, ..Default::default() });
    }

    #[test]
    fn ctrl_plus_equals_minus_and_zero_zoom_the_snip() {
        assert_eq!(press(Key::Plus, Modifiers::COMMAND), Keys { zoom_in: true, ..Default::default() });
        assert_eq!(press(Key::Equals, Modifiers::COMMAND), Keys { zoom_in: true, ..Default::default() });
        assert_eq!(press(Key::Minus, Modifiers::COMMAND), Keys { zoom_out: true, ..Default::default() });
        assert_eq!(press(Key::Num0, Modifiers::COMMAND), Keys { zoom_fit: true, ..Default::default() });
    }

    #[test]
    fn zoom_keys_need_ctrl() {
        assert_eq!(press(Key::Minus, Modifiers::NONE), Keys::default());
        assert_eq!(press(Key::Num0, Modifiers::NONE), Keys::default());
    }

    #[test]
    fn ctrl_z_undoes_and_ctrl_y_redoes() {
        assert_eq!(press(Key::Z, Modifiers::COMMAND), Keys { undo: true, ..Default::default() });
        assert_eq!(press(Key::Y, Modifiers::COMMAND), Keys { redo: true, ..Default::default() });
    }

    #[test]
    fn ctrl_n_asks_for_a_new_snip() {
        assert_eq!(press(Key::N, Modifiers::COMMAND), Keys { new: true, ..Default::default() });
    }

    #[test]
    fn clock_reads_the_time_out_of_a_timestamp() {
        assert_eq!(clock("2026-11-05 221005"), "22:10:05");
        assert_eq!(clock("odd"), "odd");
    }

    #[test]
    fn titles_never_repeat() {
        let open = ["Snipscreen — Snip 22:10:05", "Snipscreen — Snip 22:10:05 (2)"];
        assert_eq!(unique_title("Snipscreen — Snip 22:10:06", open.iter().copied()), "Snipscreen — Snip 22:10:06");
        assert_eq!(unique_title("Snipscreen — Snip 22:10:05", open.iter().copied()), "Snipscreen — Snip 22:10:05 (3)");
    }

    #[test]
    fn copy_and_save_use_the_canvas_picture_only_while_it_is_current() {
        let mut snip = Snip::new(white(8, 8), Tools::default());
        let stroke = || Stroke { ink: Ink::Pen, color: Color32::RED, width: 2.0, points: vec![pos2(1.0, 1.0), pos2(6.0, 6.0)] };
        snip.doc.add_mark(stroke());
        assert_eq!(*snip.flattened(), snip.doc.flatten(), "no canvas yet: flattened from the doc");
        let ctx = egui::Context::default();
        snip.texture = Some(CanvasTex::new(&ctx, &snip.doc));
        let shown = snip.texture.as_ref().unwrap().shown.clone();
        assert!(
Arc::ptr_eq(&snip.flattened(), &shown), "a current canvas is reused, not re-burned");
        snip.doc.erase_at(pos2(3.0, 3.0), 2.0);
        assert!(!Arc::ptr_eq(&snip.flattened(), &shown), "a stale canvas is never copied");
        assert_eq!(*snip.flattened(), snip.doc.flatten());
    }

    /// A picture with every channel varying, so blend mistakes show up.
    fn gradient(w: usize, h: usize) -> ColorImage {
        let px = (0..w * h).map(|i| Color32::from_rgb((i % w * 255 / w) as u8, (i / w * 255 / h) as u8, ((i * 7) % 256) as u8)).collect();
        ColorImage::new([w, h], px)
    }

    /// The canvas texture as the GPU holds it, rebuilt from the uploads egui hands the backend.
    struct Screen {
        image: Option<ColorImage>,
    }

    impl Screen {
        /// Applies the uploads to the canvas's texture pieces, each at its place in the picture.
        fn apply(&mut self, delta: &egui::TexturesDelta, tex: Option<&Chunks>) {
            let Some(tex) = tex else { return };
            let size = tex.parts.iter().fold([0, 0], |s, (r, _)| [s[0].max(r.x1), s[1].max(r.y1)]);
            for (c, t) in &tex.parts {
                for d in delta.set.get(&t.id()).into_iter().flatten() {
                    let egui::ImageData::Color(img) = &d.image;
                    assert!(img.size[0] <= c.x1 - c.x0 && img.size[1] <= c.y1 - c.y0, "an upload larger than its piece");
                    match (d.pos, self.image.as_mut()) {
                        (None, screen) => {
                            let screen = match screen {
                                Some(s) if s.size == size => s,
                                _ => self.image.insert(ColorImage::new(size, vec![Color32::TRANSPARENT; size[0] * size[1]])),
                            };
                            markup::paste(screen, img, c.pos());
                        }
                        (Some(at), Some(screen)) => markup::paste(screen, img, [c.x0 + at[0], c.y0 + at[1]]),
                        (Some(_), None) => panic!("partial upload before the texture exists"),
                    }
                }
            }
        }

        fn pixels(&self) -> &[Color32] {
            &self.image.as_ref().expect("the canvas uploaded its texture").pixels
        }
    }

    /// One headless frame of the snip's canvas (1280x800 window at 100%), applying its texture uploads to `screen`.
    fn canvas_frame(ctx: &egui::Context, snip: &mut Snip, screen: &mut Screen, events: Vec<Event>) {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))), events, ..Default::default() };
        let guard = PopupClickGuard::default();
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| snip.canvas(ui, &guard));
        });
        screen.apply(&out.textures_delta, snip.texture.as_ref().map(|c| &c.tex));
        out.textures_delta.clear();
    }

    fn pointer(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }
    }

    /// Drags along `path` over a snip shown at 100% (a 300x200 one is centred on (640, 400)) with the
    /// current tool, checking at every frame that the screen shows exactly what would be saved.
    fn drag_and_compare(snip: &mut Snip, path: &[Pos2]) {
        let ctx = egui::Context::default();
        let mut screen = Screen { image: None };
        // A fresh context means a fresh GPU: the canvas must upload its texture again.
        snip.texture = None;
        let marks = snip.doc.marks.len();
        canvas_frame(&ctx, snip, &mut screen, vec![]);
        canvas_frame(&ctx, snip, &mut screen, vec![Event::PointerMoved(path[0]), pointer(path[0], true)]);
        for &p in &path[1..] {
            canvas_frame(&ctx, snip, &mut screen, vec![Event::PointerMoved(p)]);
            let mut want = snip.doc.flatten();
            markup::burn(&mut want, snip.drawing.as_ref().expect("a drag in progress draws"));
            assert!(screen.pixels() == want.pixels, "mid-drag screen differs from the saved snip");
        }
        canvas_frame(&ctx, snip, &mut screen, vec![pointer(*path.last().unwrap(), false)]);
        canvas_frame(&ctx, snip, &mut screen, vec![]);
        assert!(snip.drawing.is_none() && snip.doc.marks.len() == marks + 1, "the drag committed one mark");
        assert!(screen.pixels() == snip.doc.flatten().pixels, "after the drag the screen differs from the saved snip");
        assert!(*snip.flattened() == snip.doc.flatten(), "copy and save write what is shown");
    }

    const PATH: [Pos2; 5] = [pos2(600.0, 380.0), pos2(640.0, 400.0), pos2(700.0, 390.0), pos2(620.0, 420.0), pos2(600.0, 360.0)];

    #[test]
    fn pen_and_highlighter_show_exactly_what_is_saved() {
        for tool in [Tool::Pen, Tool::Highlighter] {
            let mut snip = Snip::new(gradient(300, 200), Tools { tool, ..Tools::default() });
            drag_and_compare(&mut snip, &PATH);
            drag_and_compare(&mut snip, &PATH[1..]);
        }
    }
    #[test]
    fn the_highlighter_on_a_dark_or_light_snip_shows_exactly_what_is_saved() {
        for base in [Color32::from_rgb(28, 30, 34), Color32::from_rgb(250, 250, 248)] {
            let mut snip = Snip::new(ColorImage::new([300, 200], vec![base; 60000]), Tools { tool: Tool::Highlighter, ..Tools::default() });
            drag_and_compare(&mut snip, &PATH);
            drag_and_compare(&mut snip, &PATH[1..]);
        }
    }

    #[test]
    fn a_snip_wider_than_the_gpu_allows_is_shown_in_pieces_exactly_as_saved() {
        // Headless egui allows 2048 px a side; a 12000 x 2000 snip (three 4K monitors and more) needs six pieces.
        let mut snip = Snip::new(gradient(12000, 2000), Tools { tool: Tool::Highlighter, ..Tools::default() });
        drag_and_compare(&mut snip, &PATH);
        let parts: Vec<PxRect> = snip.texture.as_ref().unwrap().tex.parts.iter().map(|(r, _)| *r).collect();
        assert_eq!(parts.len(), 6, "{parts:?}");
        assert!(parts.iter().all(|r| r.x1 - r.x0 <= 2048 && r.y1 - r.y0 <= 2048));
        let s = &snip.doc.marks[0].bounds();
        assert!(parts.iter().filter(|r| s.intersects(r.rect())).count() > 1, "the stroke crosses a seam between pieces");
    }

    #[test]
    fn shapes_show_exactly_what_is_saved_while_they_are_dragged() {
        for (kind, _, _) in SHAPES {
            let mut snip = Snip::new(gradient(300, 200), Tools { tool: Tool::Shape, shape_kind: kind, ..Tools::default() });
            drag_and_compare(&mut snip, &PATH);
            assert!(matches!(&*snip.doc.marks[0], Mark::Shape(s) if s.kind == kind && s.to == pos2(600.0 - 640.0 + 150.0, 360.0 - 400.0 + 100.0)), "{kind:?} ends where the drag ended");
        }
    }

    #[test]
    fn grips_are_the_corners_then_the_sides_then_the_inside() {
        let f = Rect::from_min_max(pos2(100.0, 100.0), pos2(300.0, 200.0));
        assert_eq!(grip_at(f, pos2(95.0, 105.0), 10.0), Some(Grip::NW));
        assert_eq!(grip_at(f, pos2(305.0, 195.0), 10.0), Some(Grip::SE));
        assert_eq!(grip_at(f, pos2(298.0, 102.0), 10.0), Some(Grip::NE));
        assert_eq!(grip_at(f, pos2(200.0, 208.0), 10.0), Some(Grip::S));
        assert_eq!(grip_at(f, pos2(92.0, 150.0), 10.0), Some(Grip::W));
        assert_eq!(grip_at(f, pos2(200.0, 150.0), 10.0), Some(Grip::Move));
        assert_eq!(grip_at(f, pos2(200.0, 230.0), 10.0), None, "beyond reach");
        // A frame thinner than two reaches: the nearer side wins.
        let thin = Rect::from_min_max(pos2(100.0, 100.0), pos2(112.0, 200.0));
        assert_eq!(grip_at(thin, pos2(103.0, 150.0), 10.0), Some(Grip::W));
        assert_eq!(grip_at(thin, pos2(110.0, 150.0), 10.0), Some(Grip::E));
    }

    #[test]
    fn dragging_the_frame_stays_inside_the_snip_and_above_the_minimum() {
        let bounds = Rect::from_min_max(Pos2::ZERO, pos2(400.0, 300.0));
        let start = Rect::from_min_max(pos2(50.0, 50.0), pos2(250.0, 200.0));
        assert_eq!(drag_frame(start, Grip::SE, vec2(30.0, 20.0), bounds, 16.0), Rect::from_min_max(pos2(50.0, 50.0), pos2(280.0, 220.0)));
        assert_eq!(drag_frame(start, Grip::NW, vec2(-80.0, -80.0), bounds, 16.0), Rect::from_min_max(pos2(0.0, 0.0), pos2(250.0, 200.0)), "clamped to the snip");
        assert_eq!(drag_frame(start, Grip::E, vec2(-500.0, 99.0), bounds, 16.0), Rect::from_min_max(pos2(50.0, 50.0), pos2(66.0, 200.0)), "never thinner than the minimum");
        assert_eq!(drag_frame(start, Grip::N, vec2(9.0, 500.0), bounds, 16.0), Rect::from_min_max(pos2(50.0, 184.0), pos2(250.0, 200.0)));
        assert_eq!(drag_frame(start, Grip::Move, vec2(500.0, -20.0), bounds, 16.0), Rect::from_min_max(pos2(200.0, 30.0), pos2(400.0, 180.0)), "moves whole, stopping at the edge");
        let tiny = Rect::from_min_max(Pos2::ZERO, pos2(10.0, 8.0));
        assert_eq!(drag_frame(tiny, Grip::SE, vec2(-50.0, -50.0), tiny, 16.0), tiny, "a snip smaller than the minimum keeps its size");
    }

    #[test]
    fn crop_starts_on_the_whole_snip_and_its_handles_shape_the_result() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools::default());
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        snip.toggle_crop();
        assert_eq!(snip.crop, Some(Rect::from_min_max(Pos2::ZERO, pos2(300.0, 200.0))), "the frame starts around the whole snip");
        // The snip is shown at 100% from (490, 300): drag its bottom-right handle, then the inside.
        let drag = |snip: &mut Snip, screen: &mut Screen, from: Pos2, to: Pos2| {
            canvas_frame(&ctx, snip, screen, vec![Event::PointerMoved(from), pointer(from, true)]);
            canvas_frame(&ctx, snip, screen, vec![Event::PointerMoved(to)]);
            canvas_frame(&ctx, snip, screen, vec![pointer(to, false)]);
        };
        drag(&mut snip, &mut screen, pos2(791.0, 501.0), pos2(700.0, 450.0));
        assert_eq!(snip.crop, Some(Rect::from_min_max(Pos2::ZERO, pos2(209.0, 149.0))));
        drag(&mut snip, &mut screen, pos2(600.0, 400.0), pos2(620.0, 410.0));
        assert_eq!(snip.crop, Some(Rect::from_min_max(pos2(20.0, 10.0), pos2(229.0, 159.0))));
        assert!(snip.doc.marks.is_empty(), "the pen under the frame drew nothing");
        snip.apply_crop();
        assert_eq!(snip.doc.size(), [209, 149]);
        assert_eq!(snip.crop, None);
        snip.step(false);
        assert_eq!(snip.doc.size(), [300, 200], "undo brings the whole snip back");
    }

    #[test]
    fn esc_cancels_the_crop_frame_and_leaves_the_snip_alone() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(40, 30), 0, None);
        frame(&ctx, &editors, vec![]);
        editors.with(a, |e| {
            let Content::Snip(s) = &mut e.content else { panic!("a snip") };
            s.toggle_crop();
        });
        let esc = Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame(&ctx, &editors, vec![esc]);
        assert_eq!(editors.with(a, |e| e.snip().map(|s| (s.crop, s.doc.size(), s.doc.can_undo()))).unwrap(), Some((None, [40, 30], false)));
    }

    #[test]
    fn erase_and_undo_show_at_once_and_match_the_saved_snip() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools::default());
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        for (tool, path) in [(Tool::Pen, &PATH[..]), (Tool::Highlighter, &[pos2(600.0, 450.0), pos2(650.0, 455.0), pos2(700.0, 460.0)][..])] {
            snip.tools.tool = tool;
            canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(path[0]), pointer(path[0], true)]);
            for &p in &path[1..] {
                canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(p)]);
            }
            canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(*path.last().unwrap(), false)]);
        }
        assert_eq!(snip.doc.marks.len(), 2);
        let ids = |s: &Snip| s.texture.as_ref().map(|c| c.tex.parts.iter().map(|(_, t)| t.id()).collect::<Vec<_>>());
        let full = ids(&snip);
        snip.tools.tool = Tool::Eraser;
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[0]), pointer(PATH[0], true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(PATH[0], false)]);
        assert_eq!(snip.doc.marks.len(), 1, "the click erased the pen stroke");
        assert!(screen.pixels() == snip.doc.flatten().pixels, "the erase shows in the frame that made it");
        snip.step(false);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(screen.pixels() == snip.doc.flatten().pixels, "undo shows the stroke again");
        assert_eq!(ids(&snip), full, "the same texture, patched rather than replaced");
        assert!(*snip.flattened() == snip.doc.flatten());
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    #[test]
    fn typed_text_shows_exactly_what_is_saved_and_enter_places_it() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools { tool: Tool::Text, ..Tools::default() });
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        let at = pos2(520.0, 360.0);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(at), pointer(at, true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(at, false)]);
        assert!(snip.typing(), "a click starts typing");
        let live_matches = |snip: &Snip, screen: &Screen| {
            let mut want = snip.doc.flatten();
            markup::burn(&mut want, snip.drawing.as_ref().unwrap());
            screen.pixels() == want.pixels
        };
        for events in [
            vec![Event::Text("Hey".into())],
            vec![key(Key::Backspace, Modifiers::NONE), Event::Text("llo".into())],
            vec![key(Key::Enter, Modifiers::SHIFT), Event::Text("2nd".into())],
        ] {
            canvas_frame(&ctx, &mut snip, &mut screen, events);
            assert!(live_matches(&snip, &screen), "the text on screen is the text that will be saved");
        }
        canvas_frame(&ctx, &mut snip, &mut screen, vec![key(Key::Enter, Modifiers::NONE)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(!snip.typing());
        assert!(matches!(&*snip.doc.marks[0], Mark::Text(t) if t.text == "Hello\n2nd" && t.outline), "{:?}", snip.doc.marks);
        assert!(screen.pixels() == snip.doc.flatten().pixels);
        assert!(*snip.flattened() == snip.doc.flatten());
    }

    #[test]
    fn esc_drops_the_draft_empty_text_is_never_placed_and_a_crop_places_it_first() {
        let ctx = egui::Context::default();
        let fresh = Snip::new(white(300, 200), Tools { tool: Tool::Text, ..Tools::default() });
        let mut ed = Editor::new(Content::Snip(Box::new(fresh)), "t".into(), ViewportId::ROOT, 0, None, Default::default(), MIN_EDIT_W);
        let mut screen = Screen { image: None };
        fn snip(ed: &mut Editor) -> &mut Snip {
            match &mut ed.content {
                Content::Snip(s) => s,
                Content::Video(_) => panic!("a snip"),
            }
        }
        let canvas = |ed: &mut Editor, screen: &mut Screen, events: Vec<Event>| canvas_frame(&ctx, snip(ed), screen, events);
        // Keys go through the editor's own shortcut handling, as in its window.
        let esc = |ed: &mut Editor| {
            let input = egui::RawInput { events: vec![key(Key::Escape, Modifiers::NONE)], ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| ed.shortcuts(ui.ctx()));
            out.textures_delta.clear();
        };
        let click = |ed: &mut Editor, screen: &mut Screen| {
            let at = pos2(640.0, 400.0);
            canvas(ed, screen, vec![Event::PointerMoved(at), pointer(at, true)]);
            canvas(ed, screen, vec![pointer(at, false)]);
        };
        let state = |ed: &mut Editor| {
            let s = snip(ed);
            (s.typing(), s.doc.marks.len(), s.crop.is_some())
        };
        canvas(&mut ed, &mut screen, vec![]);
        click(&mut ed, &mut screen);
        canvas(&mut ed, &mut screen, vec![Event::Text("draft".into())]);
        assert_eq!(state(&mut ed), (true, 0, false));
        esc(&mut ed);
        assert_eq!(state(&mut ed), (false, 0, false), "Esc throws the draft away");
        click(&mut ed, &mut screen);
        click(&mut ed, &mut screen);
        assert_eq!(state(&mut ed), (true, 0, false), "clicking away from empty text places nothing");
        canvas(&mut ed, &mut screen, vec![Event::Text("kept".into())]);
        snip(&mut ed).toggle_crop();
        assert_eq!(state(&mut ed), (false, 1, true), "starting a crop places the text first");
        esc(&mut ed);
        assert_eq!(state(&mut ed), (false, 1, false), "with nothing typed, Esc cancels the crop as usual");
        canvas(&mut ed, &mut screen, vec![]);
        assert!(screen.pixels() == snip(&mut ed).doc.flatten().pixels);
    }

    #[test]
    fn a_click_with_the_shape_tool_draws_nothing() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools { tool: Tool::Shape, ..Tools::default() });
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[0]), pointer(PATH[0], true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(PATH[0], false)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(snip.doc.marks.is_empty() && snip.drawing.is_none());
        assert!(screen.pixels() == snip.doc.image.pixels);
    }

    #[test]
    fn a_cancelled_stroke_leaves_the_picture_as_it_was() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools { tool: Tool::Highlighter, ..Tools::default() });
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[0]), pointer(PATH[0], true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[2])]);
        assert!(screen.pixels() != snip.doc.image.pixels, "the stroke shows while drawn");
        snip.step(false);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(screen.pixels() == snip.doc.image.pixels, "undo mid-stroke puts the picture back");
    }

    /// One headless frame of every open editor. Without a native backend egui embeds each viewport in
    /// the calling frame, which is enough to run the editors' code.
    fn frame(ctx: &egui::Context, editors: &Editors, events: Vec<Event>) {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))), events, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| editors.show(ui.ctx()));
        out.textures_delta.clear();
    }

    #[test]
    fn each_snip_gets_its_own_window_and_document() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(40, 30), 0, None);
        let (b, _, _) = editors.open_snip(&ctx, white(60, 50), 0, Some([100, 100]));
        assert_ne!(a, b, "every editor has its own viewport");
        assert_eq!(editors.ids(), vec![a, b]);
        frame(&ctx, &editors, vec![]);

        editors.with(a, |e| {
            let Content::Snip(s) = &mut e.content else { panic!("a snip") };
            s.doc.add_mark(Stroke { ink: Ink::Pen, color: Color32::RED, width: 2.0, points: vec![pos2(1.0, 1.0)] });
        });
        let undo = |id| editors.with(id, |e| e.snip().is_some_and(|s| s.doc.can_undo())).unwrap();
        assert!(undo(a) && !undo(b), "undo stacks are separate");
        assert_eq!(editors.with(b, |e| e.snip().map(|s| s.doc.size())).unwrap(), Some([60, 50]));
        let title = |id| editors.slots.iter().find(|s| s.id == id).map(|s| s.title.clone()).unwrap();
        assert_ne!(title(a), title(b), "titles stay unique so each window can find its handle");
    }

    #[test]
    fn closing_one_editor_leaves_the_others_open() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        let (b, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        let c = editors.open_video(&ctx, VideoResult { path: PathBuf::from("x.mp4"), seconds: 1.0, size: [64, 48], thumb: None }, "Saved".into(), 0, None);
        frame(&ctx, &editors, vec![]);
        assert_eq!(editors.poll(), Requests::default());
        editors.with(b, |e| e.closed = true);
        assert_eq!(editors.poll(), Requests { new_snip: false, closed: 1 });
        assert_eq!(editors.ids(), vec![a, c]);
        frame(&ctx, &editors, vec![]);
        assert_eq!(editors.ids(), vec![a, c], "the rest stay open");
    }

    #[test]
    fn new_tools_start_from_the_newest_snip() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        let picked = Tools {
            tool: Tool::Highlighter,
            pen: (PEN_COLORS[3], 8.0),
            highlighter: (HIGHLIGHT_COLORS[2], 30.0),
            shape_kind: ShapeKind::Ellipse,
            shape: (PEN_COLORS[4], 12.0),
            text: (PEN_COLORS[1], 40.0),
            text_outline: false,
        };

        editors.with(a, |e| {
            let Content::Snip(s) = &mut e.content else { panic!("a snip") };
            s.tools = picked;
        });
        let (b, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        assert_eq!(editors.with(b, |e| e.snip().map(|s| s.tools)).unwrap(), Some(picked));
    }

    #[test]
    fn ctrl_n_in_an_editor_reaches_the_shell_once() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        editors.open_snip(&ctx, white(10, 10), 0, None);
        let n = Event::Key { key: Key::N, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
        frame(&ctx, &editors, vec![n]);
        assert_eq!(editors.poll(), Requests { new_snip: true, closed: 0 });
        assert_eq!(editors.poll(), Requests::default(), "a request is handed over once");
    }

    #[test]
    fn background_messages_reach_their_own_editor() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, report_a, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        let (b, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        report_a.send("Copied to clipboard".into());
        frame(&ctx, &editors, vec![]);
        let msg = |id| editors.with(id, |e| e.note.as_ref().map(|n| n.msg.clone())).unwrap();
        assert_eq!(msg(a).as_deref(), Some("Copied to clipboard"));
        assert_eq!(msg(b), None);
    }

    #[test]
    fn the_canvas_copies_the_shared_picture_while_a_mark_is_drawn_not_on_mouse_up() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools::default());
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        let shared = |s: &Snip| Arc::ptr_eq(&s.texture.as_ref().unwrap().shown, &s.doc.image);
        assert!(shared(&snip), "before any mark the canvas shows the doc's own picture");
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[0]), pointer(PATH[0], true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[1])]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while shared(&snip) {
            assert!(std::time::Instant::now() < deadline, "the copy should arrive while drawing");
            std::thread::sleep(std::time::Duration::from_millis(1));
            canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(PATH[1])]);
        }
        let own = snip.texture.as_ref().map(|c| Arc::as_ptr(&c.shown)).unwrap();
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(PATH[1], false)]);
        assert_eq!(snip.texture.as_ref().map(|c| Arc::as_ptr(&c.shown)).unwrap(), own, "the mark went into the copy made while drawing");
        assert!(screen.pixels() == snip.doc.flatten().pixels);
        assert_eq!(*snip.doc.image, gradient(300, 200), "the doc's picture is untouched");
    }

    #[test]
    fn a_text_size_change_paints_what_is_in_view_and_the_rest_before_it_is_seen() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(2400, 1600), Tools { tool: Tool::Text, text: (PEN_COLORS[2], 96.0), ..Tools::default() });
        snip.zoom = Some(1.0);
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        let at = pos2(60.0, 110.0);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(at), pointer(at, true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(at, false)]);
        for line in 0..10 {
            if line > 0 {
                canvas_frame(&ctx, &mut snip, &mut screen, vec![key(Key::Enter, Modifiers::SHIFT)]);
            }
            canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::Text("A wide line of text".into())]);
        }
        let live = |snip: &Snip| {
            let mut want = snip.doc.flatten();
            markup::burn(&mut want, snip.drawing.as_ref().expect("typing"));
            want
        };
        let pending = |snip: &Snip| snip.texture.as_ref().unwrap().live.pending.clone();
        // The 1280x800 view shows the picture from (-16, -16), inside the canvas margin.
        let in_view = |a: &ColorImage, b: &[Color32]| (0..760).all(|y| a.pixels[y * 2400..y * 2400 + 1240] == b[y * 2400..y * 2400 + 1240]);
        assert!(in_view(&live(&snip), screen.pixels()), "typing shows exactly what will be placed");
        let settle = |snip: &mut Snip, screen: &mut Screen| {
            let mut frames = 0;
            while !pending(snip).is_empty() {
                frames += 1;
                assert!(frames < 500, "frames where the text doesn't change catch up");
                canvas_frame(&ctx, snip, screen, vec![]);
            }
        };
        settle(&mut snip, &mut screen);
        assert!(screen.pixels() == live(&snip).pixels, "lines typed out of view too, once typing stops");
        snip.tools.text.1 = 120.0;
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(!pending(&snip).is_empty(), "the block reaches past the view; that part waits");
        assert!(in_view(&live(&snip), screen.pixels()), "what is in view is exact at once");
        settle(&mut snip, &mut screen);
        assert!(screen.pixels() == live(&snip).pixels, "and then all of it is exact");
        // Placed right after a size change, with tiles still behind: exactly as saved.
        snip.tools.text.1 = 80.0;
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(!pending(&snip).is_empty());
        canvas_frame(&ctx, &mut snip, &mut screen, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(snip.drawing.is_none() && snip.doc.marks.len() == 1);
        assert!(screen.pixels() == snip.doc.flatten().pixels);
        // A size change, then zooming out to see it all: everything shows as it will be placed.
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(at), pointer(at, true)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(at, false)]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::Text("Another\nblock of text that runs well past the view on the right".into())]);
        snip.tools.text.1 = 160.0;
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        snip.zoom = None;
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        assert!(pending(&snip).is_empty(), "in view now, so painted now");
        assert!(screen.pixels() == live(&snip).pixels);
    }

    #[test]
    fn a_click_dot_takes_the_copy_started_at_the_press() {
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(300, 200), Tools::default());
        let mut screen = Screen { image: None };
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        let at = pos2(640.0, 400.0);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(at), pointer(at, true)]);
        assert!(snip.texture.as_ref().unwrap().copying.is_some(), "the copy starts on the press, before egui knows it's a click");
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(at, false)]);
        let canvas = snip.texture.as_ref().unwrap();
        assert!(canvas.copying.is_none() && !Arc::ptr_eq(&canvas.shown, &snip.doc.image));
        assert_eq!(snip.doc.marks.len(), 1, "the click drew a dot");
        assert!(screen.pixels() == snip.doc.flatten().pixels);
        assert_eq!(*snip.doc.image, gradient(300, 200), "the doc's picture is untouched");
        // A press with a tool that draws nothing on a click starts no copy.
        let mut snip = Snip::new(gradient(300, 200), Tools { tool: Tool::Shape, ..Tools::default() });
        canvas_frame(&ctx, &mut snip, &mut screen, vec![]);
        canvas_frame(&ctx, &mut snip, &mut screen, vec![Event::PointerMoved(at), pointer(at, true)]);
        assert!(snip.texture.as_ref().unwrap().copying.is_none());
        canvas_frame(&ctx, &mut snip, &mut screen, vec![pointer(at, false)]);
        assert!(Arc::ptr_eq(&snip.texture.as_ref().unwrap().shown, &snip.doc.image), "nothing drawn, nothing copied");
    }

    #[test]
    fn a_second_save_while_one_is_under_way_is_ignored() {
        let ctx = egui::Context::default();
        let fresh = Snip::new(white(10, 10), Tools::default());
        let mut ed = Editor::new(Content::Snip(Box::new(fresh)), "t".into(), ViewportId::ROOT, 0, None, Default::default(), MIN_EDIT_W);
        // As if a Save dialog were open: another Save must return at once and open nothing.
        ed.hwnd = Some(1);
        ed.saving.store(true, Ordering::Release);
        ed.save_as(&ctx);
        assert!(ed.saving.load(Ordering::Acquire), "the first save still owns the flag");
        assert_eq!(ed.save_waiting, 0);
        // The worker frees it however it ends, a panic included.
        let busy = Busy(ed.saving.clone());
        let _ = std::thread::spawn(move || {
            let _busy = busy;
            panic!("a failed save");
        })
        .join();
        assert!(!ed.saving.load(Ordering::Acquire), "a new Save can start");
    }

    #[test]
    fn a_save_before_the_window_is_found_waits_for_it_and_never_borrows_the_shells() {
        let ctx = egui::Context::default();
        let fresh = Snip::new(white(10, 10), Tools::default());
        let mut ed = Editor::new(Content::Snip(Box::new(fresh)), "no window has this title 7f3a".into(), ViewportId::ROOT, 0, None, Default::default(), MIN_EDIT_W);
        ed.save_as(&ctx);
        assert!(!ed.saving.load(Ordering::Acquire), "no dialog yet: there is no window of its own to own it");
        assert_eq!(ed.save_waiting, SAVE_WAIT_FRAMES);
        for _ in 0..SAVE_WAIT_FRAMES {
            assert!(ed.note.is_none());
            ed.retry_save(&ctx);
        }
        assert_eq!(ed.save_waiting, 0);
        assert!(!ed.saving.load(Ordering::Acquire));
        assert!(ed.note.is_some(), "giving up says so");
    }

    #[test]
    fn the_shell_never_waits_on_an_editor_mid_frame() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        // Hold the editor as its own frame would; on this thread a blocking lock would never return.
        let held = editors.slots[0].editor.clone();
        let guard = held.lock().unwrap();
        editors.set_new_tip("New snip (Alt+Shift+S)".into());
        editors.newest_notifier(&ctx).expect("a notifier without the lock").send("Saved".into());
        assert!(editors.with(a, |_| ()).is_none(), "a busy editor is skipped, not waited for");
        assert_eq!(editors.poll(), Requests::default());
        drop(guard);
        frame(&ctx, &editors, vec![]);
        assert_eq!(editors.with(a, |e| (e.new_tip.clone(), e.note.as_ref().map(|n| n.msg.clone()))).unwrap(), ("New snip (Alt+Shift+S)".to_string(), Some("Saved".to_string())));
    }

    #[test]
    fn new_button_tooltips_follow_the_shell() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        editors.set_new_tip("New recording (Ctrl+N)".into());
        let (b, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        frame(&ctx, &editors, vec![]);
        for id in [a, b] {
            assert_eq!(editors.with(id, |e| e.new_tip.clone()).unwrap(), "New recording (Ctrl+N)");
        }
    }

    /// A snip editor with the text tool, typing `typed` into a draft started by a click on the middle of its 300x200 snip.
    fn typing(typed: &str) -> (egui::Context, Editor, Screen) {
        let ctx = egui::Context::default();
        let fresh = Snip::new(gradient(300, 200), Tools { tool: Tool::Text, ..Tools::default() });
        let mut ed = Editor::new(Content::Snip(Box::new(fresh)), "t".into(), ViewportId::ROOT, 0, None, Default::default(), MIN_EDIT_W);
        let mut screen = Screen { image: None };
        let at = pos2(640.0, 400.0);
        for events in [vec![], vec![Event::PointerMoved(at), pointer(at, true)], vec![pointer(at, false)]] {
            canvas_frame(&ctx, snip_of(&mut ed), &mut screen, events);
        }
        for ch in typed.chars() {
            canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![Event::Text(ch.to_string())]);
        }
        (ctx, ed, screen)
    }

    fn snip_of(ed: &mut Editor) -> &mut Snip {
        match &mut ed.content {
            Content::Snip(s) => s,
            Content::Video(_) => panic!("a snip"),
        }
    }

    fn draft_text(ed: &mut Editor) -> Option<String> {
        match &snip_of(ed).drawing {
            Some(Mark::Text(t)) => Some(t.text.clone()),
            _ => None,
        }
    }

    /// Presses `k` with `mods` through the editor's own shortcut handling.
    fn shortcut(ctx: &egui::Context, ed: &mut Editor, k: Key, mods: Modifiers) {
        let input = egui::RawInput { events: vec![key(k, mods)], ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| ed.shortcuts(ui.ctx()));
        out.textures_delta.clear();
    }

    #[test]
    fn ctrl_z_while_typing_takes_back_a_word_at_a_time_and_ctrl_y_never_discards() {
        let (ctx, mut ed, _) = typing("hello big world");
        shortcut(&ctx, &mut ed, Key::Y, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("hello big world"), "nothing to redo leaves the draft alone");
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("hello big "));
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("hello "));
        shortcut(&ctx, &mut ed, Key::Y, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("hello big "));
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed).as_deref(), Some(""), "back to the empty draft");
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed), None, "Ctrl+Z on the empty draft cancels it");
        assert!(snip_of(&mut ed).doc.marks.is_empty(), "and never touched the snip's own history");
    }

    #[test]
    fn placing_text_mid_composition_keeps_what_the_input_method_showed() {
        let (ctx, mut ed, mut screen) = typing("Hi ");
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![Event::Ime(egui::ImeEvent::Preedit { text: "にほ".into(), active_range_chars: None })]);
        let shown = screen.pixels();
        // Switching tools places the text, as copy and save do.
        snip_of(&mut ed).finish_text();
        let snip = snip_of(&mut ed);
        assert!(matches!(&*snip.doc.marks[0], Mark::Text(t) if t.text == "Hi にほ"), "the composition is placed with it");
        assert!(snip.ime_taken && snip.draft.preedit.is_empty(), "and the input method is told to drop its copy");
        assert!(snip.doc.flatten().pixels == shown, "exactly as it showed");
        // A click elsewhere places it too, then starts new text there without the composition.
        let (ctx, mut ed, mut screen) = typing("Hi ");
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![Event::Ime(egui::ImeEvent::Preedit { text: "にほ".into(), active_range_chars: None })]);
        let at = pos2(100.0, 100.0);
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![Event::PointerMoved(at), pointer(at, true)]);
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![pointer(at, false)]);
        assert!(matches!(&*snip_of(&mut ed).doc.marks[0], Mark::Text(t) if t.text == "Hi にほ"));
        assert_eq!(draft_text(&mut ed).as_deref(), Some(""));
        assert!(snip_of(&mut ed).draft.preedit.is_empty());
    }

    #[test]
    fn undo_and_redo_buttons_follow_the_text_being_typed() {
        let (ctx, mut ed, _) = typing("one two");
        let buttons = |ed: &mut Editor| {
            let s = snip_of(ed);
            (s.can_undo(), s.can_redo())
        };
        assert!(!snip_of(&mut ed).doc.can_undo(), "the snip itself has nothing to undo yet");
        assert_eq!(buttons(&mut ed), (true, false), "undo takes back typing; nothing to redo");
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        assert_eq!(buttons(&mut ed), (true, true));
        shortcut(&ctx, &mut ed, Key::Y, Modifiers::COMMAND);
        assert_eq!(buttons(&mut ed), (true, false));
        // A placed mark redone by the snip's history doesn't light Redo while new text is typed.
        snip_of(&mut ed).finish_text();
        snip_of(&mut ed).step(false);
        assert_eq!(buttons(&mut ed), (false, true), "back on the snip's own history");
    }

    #[test]
    fn deleting_is_its_own_undo_step() {
        let (ctx, mut ed, mut screen) = typing("abc");
        let back = || vec![key(Key::Backspace, Modifiers::NONE)];
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, back());
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, back());
        assert_eq!(draft_text(&mut ed).as_deref(), Some("a"));
        shortcut(&ctx, &mut ed, Key::Z, Modifiers::COMMAND);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("abc"), "both deletions come back at once");
    }

    #[test]
    fn pasted_text_loses_returns_and_control_characters_and_tabs_become_spaces() {
        let (ctx, mut ed, mut screen) = typing("");
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![Event::Paste("a\tb\r\nc\u{7}d\u{1F600}".into())]);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("a    b\ncd\u{1F600}"));
        // Accented letters from dead keys arrive as ordinary text and stay.
        for s in ["á", "ç", "ã"] {
            canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![Event::Text(s.into())]);
        }
        assert_eq!(draft_text(&mut ed).as_deref(), Some("a    b\ncd\u{1F600}áçã"));
        let mut want = snip_of(&mut ed).doc.flatten();
        markup::burn(&mut want, snip_of(&mut ed).drawing.as_ref().unwrap());
        assert!(screen.pixels() == want.pixels, "the emoji draws nothing, the rest shows as saved");
    }

    #[test]
    fn an_input_method_composes_inline_and_commits_into_the_text() {
        let (ctx, mut ed, mut screen) = typing("Hi ");
        // Runs one canvas frame and returns the input method request it made.
        let frame = |ed: &mut Editor, screen: &mut Screen, events: Vec<Event>| {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))), events, ..Default::default() };
            let guard = PopupClickGuard::default();
            let snip = snip_of(ed);
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| snip.canvas(ui, &guard));
            });
            screen.apply(&out.textures_delta, snip.texture.as_ref().map(|c| &c.tex));
            out.textures_delta.clear();
            out.platform_output.ime
        };
        let ime = frame(&mut ed, &mut screen, vec![Event::Ime(egui::ImeEvent::Preedit { text: "にほ".into(), active_range_chars: None })]);
        let ime = ime.expect("typing turns the input method on");
        assert!(ime.cursor_rect.height() > 10.0 && ime.rect.contains(ime.cursor_rect.center()), "{ime:?}");
        assert_eq!(draft_text(&mut ed).as_deref(), Some("Hi "), "the composition is not part of the text yet");
        let Some(Mark::Text(t)) = snip_of(&mut ed).drawing.clone() else { panic!("typing") };
        let mut want = snip_of(&mut ed).doc.flatten();
        markup::burn(&mut want, &Mark::Text(Text { text: "Hi にほ".into(), ..t }));
        assert!(screen.pixels() == want.pixels, "but it shows inline");
        frame(&mut ed, &mut screen, vec![Event::Ime(egui::ImeEvent::Preedit { text: String::new(), active_range_chars: None }), Event::Ime(egui::ImeEvent::Commit("日本".into()))]);
        assert_eq!(draft_text(&mut ed).as_deref(), Some("Hi 日本"));
        let ime = frame(&mut ed, &mut screen, vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(ime.is_none(), "placing the text turns the input method off");
        assert!(matches!(&*snip_of(&mut ed).doc.marks[0], Mark::Text(t) if t.text == "Hi 日本"));
    }

    #[test]
    fn zooming_while_typing_keeps_the_text_size_and_the_size_option_still_sets_it() {
        let (ctx, mut ed, mut screen) = typing("Hey");
        let size = |ed: &mut Editor| match &snip_of(ed).drawing {
            Some(Mark::Text(t)) => t.size,
            _ => panic!("typing"),
        };
        let first = size(&mut ed);
        snip_of(&mut ed).zoom = Some(0.5);
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![]);
        assert_eq!(size(&mut ed), first, "zooming out leaves the text alone");
        snip_of(&mut ed).tools.text.1 = 40.0;
        canvas_frame(&ctx, snip_of(&mut ed), &mut screen, vec![]);
        assert_eq!(size(&mut ed), 80.0, "40 pt at 50% zoom is 80 px");
    }

    #[test]
    fn clicking_the_chosen_text_tool_opens_its_options_without_placing_the_draft() {
        let (ctx, mut ed, _) = typing("draft");
        // The markup buttons laid out from the window's corner: Text is the fourth.
        let button = |n: f32| pos2(n * (BTN.x + GAP) + BTN.x / 2.0, BTN.y / 2.0);
        let click = |ed: &mut Editor, at: Pos2| {
            // egui hit-tests against the last frame's layout, so the buttons are laid out once first.
            for events in [vec![Event::PointerMoved(at)], vec![pointer(at, true)], vec![pointer(at, false)], vec![]] {
                let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))), events, ..Default::default() };
                let snip = snip_of(ed);
                let mut out = ctx.run_ui(input, |ui| {
                    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
                            snip.markup_controls(ui);
                        });
                    });
                });
                out.textures_delta.clear();
            }
        };
        click(&mut ed, button(3.0));
        assert_eq!(draft_text(&mut ed).as_deref(), Some("draft"), "the draft is still being typed");
        assert!(egui::Popup::is_any_open(&ctx), "the text options opened");
        click(&mut ed, button(0.0));
        click(&mut ed, button(0.0));
        assert_eq!(draft_text(&mut ed), None, "switching tools places it");
        assert_eq!(snip_of(&mut ed).doc.marks.len(), 1);
    }

    /// Writes review renders, made by the same rasteriser as a saved snip, into `SNIPSCREEN_RENDER_DIR`.
    #[test]
    #[ignore = "writes PNGs for a person to look at"]
    fn render_samples_for_review() {
        let Ok(dir) = std::env::var("SNIPSCREEN_RENDER_DIR") else { return };
        let dir = PathBuf::from(dir);
        let lines = ["The quick brown fox jumps over", "the lazy dog, then naps again.", "A third line, crossed twice."];
        for (name, bg, fg) in [("highlighter-on-light", Color32::from_rgb(250, 250, 248), PEN_COLORS[0]), ("highlighter-on-dark", Color32::from_rgb(30, 31, 34), Color32::from_gray(225))] {
            let mut d = Doc::new(ColorImage::new([560, 190], vec![bg; 560 * 190]));
            for (i, line) in lines.iter().enumerate() {
                d.add_mark(Text { text: line.to_string(), color: fg, size: 30.0, outline: false, pos: pos2(20.0, 16.0 + i as f32 * 56.0) });
            }
            for (i, color) in [HIGHLIGHT_COLORS[0], HIGHLIGHT_COLORS[3], HIGHLIGHT_COLORS[0]].into_iter().enumerate() {
                let y = 16.0 + i as f32 * 56.0 + 20.0;
                let points = vec![pos2(16.0, y), pos2(500.0, y)];
                d.add_mark(Stroke { ink: Ink::Highlighter, color, width: 30.0, points });
            }
            win::save_png(&dir.join(format!("{name}.png")), &d.flatten()).expect("render saved");
        }
        let pasted = clean("Tab\there, emoji \u{1F600} skipped\r\nnext line");
        let mut d = Doc::new(gradient(900, 420));
        for (i, (size, color)) in [(24.0, PEN_COLORS[2]), (96.0, PEN_COLORS[3])].into_iter().enumerate() {
            d.add_mark(Text { text: format!("{size} pt: {pasted}"), color, size, outline: true, pos: pos2(16.0, 10.0 + i as f32 * 80.0) });
        }
        win::save_png(&dir.join("text-24-and-96-pt.png"), &d.flatten()).expect("render saved");
    }

    /// One headless frame of the snip's canvas without mirroring its uploads, for timing.
    fn bench_frame(ctx: &egui::Context, snip: &mut Snip, events: Vec<Event>) -> f64 {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))), max_texture_side: Some(16384), events, ..Default::default() };
        let guard = PopupClickGuard::default();
        let t = std::time::Instant::now();
        let mut out = ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| snip.canvas(ui, &guard));
        });
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        out.textures_delta.clear();
        ms
    }

    fn stats(name: &str, v: &[f64]) {
        let mut s = v.to_vec();
        s.sort_by(f64::total_cmp);
        println!("{name:<46} n={:<4} median {:>7.2} ms   max {:>7.2} ms", s.len(), s[s.len() / 2], s[s.len() - 1]);
    }

    /// Timings on a 4K snip. Run in release: `cargo test --release perf_ -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing report, not a check"]
    fn perf_editor_on_4k() {
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        // Typing a 12-line block at 160 px, shown at 100%.
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(3840, 2160), Tools { tool: Tool::Text, text: (PEN_COLORS[2], 160.0), ..Tools::default() });
        snip.zoom = Some(1.0);
        bench_frame(&ctx, &mut snip, vec![]);
        let at = pos2(40.0, 110.0);
        bench_frame(&ctx, &mut snip, vec![Event::PointerMoved(at), pointer(at, true)]);
        bench_frame(&ctx, &mut snip, vec![pointer(at, false)]);
        let mut lines = Vec::new();
        for l in 0..12 {
            if l > 0 {
                bench_frame(&ctx, &mut snip, vec![key(Key::Enter, Modifiers::SHIFT)]);
            }
            let mut line = Vec::new();
            for ch in "The quick brown fox jumps".chars() {
                line.push(bench_frame(&ctx, &mut snip, vec![Event::Text(ch.to_string())]));
            }
            lines.push(line);
        }
        let Some(Mark::Text(t)) = snip.drawing.as_ref() else { panic!("typing") };
        // The last line wholly on the snip: lower ones are cut off by its bottom edge.
        let on = ((2160.0 - t.pos.y) / t.line_height()).floor() as usize;
        stats("type: keystroke, line 1", &lines[0]);
        stats(&format!("type: keystroke, line {on} (last whole line on 4K)"), &lines[on - 1]);
        stats("type: keystroke, any of the 12 lines", &lines.concat());
        let mut back = Vec::new();
        for _ in 0..5 {
            back.push(bench_frame(&ctx, &mut snip, vec![key(Key::Backspace, Modifiers::NONE)]));
        }
        stats("type: backspace at the end of the block", &back);
        let mut slider = Vec::new();
        for i in 0..10 {
            snip.tools.text.1 = 159.0 - i as f32;
            slider.push(bench_frame(&ctx, &mut snip, vec![]));
        }
        stats("type: size slider frame (12 lines)", &slider);
        // The tiles out of view catch up on the frames after the slider stops, before Enter.
        let mut catch_up = Vec::new();
        while !snip.texture.as_ref().is_some_and(|c| c.live.pending.is_empty()) {
            catch_up.push(bench_frame(&ctx, &mut snip, vec![]));
        }
        stats("type: catch-up frame after the slider", &catch_up);
        let commit = bench_frame(&ctx, &mut snip, vec![key(Key::Enter, Modifiers::NONE)]);
        stats("type: Enter places the 12-line block", &[commit]);

        // A 100k-character paste (lines of 80), then typing after it.
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(3840, 2160), Tools { tool: Tool::Text, ..Tools::default() });
        snip.zoom = Some(1.0);
        bench_frame(&ctx, &mut snip, vec![]);
        bench_frame(&ctx, &mut snip, vec![Event::PointerMoved(at), pointer(at, true)]);
        bench_frame(&ctx, &mut snip, vec![pointer(at, false)]);
        let line = format!("{}
", "pasted log line 0123456789 ".repeat(3).chars().take(79).collect::<String>());
        let paste = bench_frame(&ctx, &mut snip, vec![Event::Paste(line.repeat(100_000 / line.len()))]);
        stats("paste: 100k characters", &[paste]);
        let typed: Vec<f64> = "typing after it".chars().map(|ch| bench_frame(&ctx, &mut snip, vec![Event::Text(ch.to_string())])).collect();
        stats("paste: keystroke after 100k characters", &typed);
        let back: Vec<f64> = (0..5).map(|_| bench_frame(&ctx, &mut snip, vec![key(Key::Backspace, Modifiers::NONE)])).collect();
        stats("paste: backspace after 100k characters", &back);

        // Highlighter drag across the whole snip at fit zoom.
        let ctx = egui::Context::default();
        let mut snip = Snip::new(gradient(3840, 2160), Tools { tool: Tool::Highlighter, ..Tools::default() });
        bench_frame(&ctx, &mut snip, vec![]);
        let path: Vec<Pos2> = (0..120).map(|i| pos2(40.0 + i as f32 * 10.0, 300.0 + (i as f32 * 0.3).sin() * 120.0)).collect();
        bench_frame(&ctx, &mut snip, vec![Event::PointerMoved(path[0]), pointer(path[0], true)]);
        let mut drag = Vec::new();
        for &p in &path[1..] {
            drag.push(bench_frame(&ctx, &mut snip, vec![Event::PointerMoved(p)]));
        }
        stats("highlighter: drag frame (120 points)", &drag);
        let up = bench_frame(&ctx, &mut snip, vec![pointer(*path.last().unwrap(), false)]);
        stats("highlighter: first stroke's mouse-up", &[up]);
        let path2: Vec<Pos2> = path.iter().map(|p| *p + vec2(0.0, 60.0)).collect();
        bench_frame(&ctx, &mut snip, vec![Event::PointerMoved(path2[0]), pointer(path2[0], true)]);
        for &p in &path2[1..] {
            bench_frame(&ctx, &mut snip, vec![Event::PointerMoved(p)]);
        }
        let up = bench_frame(&ctx, &mut snip, vec![pointer(*path2.last().unwrap(), false)]);
        stats("highlighter: second stroke's mouse-up", &[up]);

        // Erase, undo and redo beside 20 text marks.
        let mut d = Doc::new(gradient(3840, 2160));
        for i in 0..20 {
            let pos = pos2(100.0 + (i % 4) as f32 * 900.0, 100.0 + (i / 4) as f32 * 400.0);
            d.add_mark(Text { text: "Label 12345\nsecond line".into(), color: PEN_COLORS[2], size: 96.0, outline: true, pos });
        }
        d.add_mark(Shape { kind: ShapeKind::Line, color: Color32::RED, width: 4.0, from: pos2(3700.0, 2100.0), to: pos2(3710.0, 2100.0) });
        let (mut erase, mut undo, mut redo, mut hits) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for _ in 0..5 {
            let before = d.marks.clone();
            let t = std::time::Instant::now();
            d.checkpoint();
            assert!(d.erase_at(pos2(3705.0, 2100.0), 3.0));
            let tiles = d.changed_tiles(&before);
            std::hint::black_box(d.render_runs(&tiles));
            erase.push(ms(t));
            let before = d.marks.clone();
            let t = std::time::Instant::now();
            d.undo();
            let tiles = d.changed_tiles(&before);
            std::hint::black_box(d.render_runs(&tiles));
            undo.push(ms(t));
            let before = d.marks.clone();
            let t = std::time::Instant::now();
            d.redo();
            let tiles = d.changed_tiles(&before);
            std::hint::black_box(d.render_runs(&tiles));
            redo.push(ms(t));
            d.undo();
            let t = std::time::Instant::now();
            for i in 0..20 {
                std::hint::black_box(d.hits(pos2(150.0 + i as f32 * 10.0, 150.0), 10.0));
            }
            hits.push(ms(t) / 20.0);
        }
        stats("erase a dot beside 20 text marks", &erase);
        stats("undo that erase", &undo);
        stats("redo that erase", &redo);
        stats("eraser hit test over 20 text marks", &hits);
    }
}
