//! One result in its own window: a snip being marked up, or a finished screen recording.
//!
//! Every `Editor` is shown as a deferred egui viewport (`Editors::show`), so a window repaints only
//! for its own input and timers: drawing in one editor never re-runs the home window or the other
//! editors, and idle editors cost nothing. The shell owns the `Editors` list; an editor reports
//! back through two flags (`new_requested`, `closed`) that the shell collects in `Editors::poll`.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CursorIcon, Event, FontId, Key, KeyboardShortcut, Layout, Modifiers, Pos2, Rect, Sense,
    StrokeKind, TextureHandle, TextureOptions, Vec2, ViewportBuilder, ViewportCommand, ViewportId,
};
use windows_sys::Win32::Foundation::HWND;

use crate::icons::{self, Icon};
use crate::markup::{self, Doc, Ink, Mark, Shape, ShapeKind, Stroke, Tiles};
use crate::widgets::{
    self, dim_outside, divider, escape_for_app, icon_button, Note, Notifier, PopupClickGuard, ACCENT, BTN, BUTTON_HOVER, DIVIDER_W, GAP, GROUP_GAP, ICON,
    ICON_OFF, MIN_WINDOW_H, MIN_WINDOW_W, TOOLBAR_BG,
};
use crate::win;

/// Measured: New button 80 + markup with crop buttons 390 + output group 133 + 2 gaps + 24 toolbar margin = 659.
pub const MIN_EDIT_W: f32 = 670.0;
const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 8.0;
/// Snip zoom change per Ctrl+Plus / Ctrl+Minus press.
const ZOOM_STEP: f32 = 1.25;
/// Empty space kept around the snip inside the scrolling canvas, in points.
const CANVAS_MARGIN: Vec2 = vec2(32.0, 32.0);
const SCROLL_ID: &str = "snip_scroll";
/// Frames spent keeping a new window on its monitor while Windows finishes any rescaling.
const SETTLE_FRAMES: u8 = 8;
/// Each further open editor starts this many pixels down and right of the last, so none hides another.
const CASCADE: i32 = 28;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Pen,
    Highlighter,
    Shape,
    Eraser,
    Crop,
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
    Color32::from_rgb(0xff, 0xe6, 0x00),
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
}

impl Default for Tools {
    fn default() -> Self {
        Self {
            tool: Tool::Pen,
            pen: (PEN_COLORS[2], 4.0),
            highlighter: (HIGHLIGHT_COLORS[0], 18.0),
            shape_kind: ShapeKind::Arrow,
            shape: (PEN_COLORS[2], 4.0),
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
    crop: Option<(Pos2, Pos2)>,
    /// `None` fits the snip to the window; otherwise 1.0 is one screen pixel per pixel.
    zoom: Option<f32>,
    /// Zoom factor requested from the keyboard this frame; `canvas` applies it.
    key_zoom: f32,
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
    /// Screen corner to move the new window to on its first frame.
    place: Option<[i32; 2]>,
    /// Frames left in which to keep the new window on screen.
    settle: u8,
    /// Minimum window width last sent to the OS, so the command goes out only on change.
    min_width: f32,
    popup_guard: PopupClickGuard,
    note: Option<Note>,
    note_tx: Sender<String>,
    note_rx: Receiver<String>,
    /// The New button's tooltip, kept current by the shell (it knows the capture mode and hotkey).
    new_tip: String,
    /// New button or Ctrl+N pressed; the shell starts the capture.
    new_requested: bool,
    /// The window's close button was pressed; the shell drops this editor.
    closed: bool,
}

impl Snip {
    pub fn new(image: ColorImage, tools: Tools) -> Self {
        Self { doc: Doc::new(image), tools, texture: None, drawing: None, erase_marked: false, crop: None, zoom: None, key_zoom: 1.0 }
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
        if let Some((a, b)) = self.crop.take() {
            self.doc.crop(Rect::from_two_pos(a, b));
            self.zoom = None;
        }
    }

    fn step(&mut self, redo: bool) {
        step_history(&mut self.doc, &mut self.crop, &mut self.drawing, redo);
    }

    fn markup_width(&self) -> f32 {
        let buttons = if self.crop.is_some() { 9.0 } else { 7.0 };
        let dividers = 2.0;
        buttons * BTN.x + dividers * DIVIDER_W + (buttons + dividers - 1.0) * GAP
    }

    fn markup_controls(&mut self, ui: &mut egui::Ui) {
        for (tool, icon, tip) in [
            (Tool::Pen, Icon::Pen, "Pen (click again for colour and size)"),
            (Tool::Highlighter, Icon::Highlighter, "Highlighter (click again for colour and size)"),
            (Tool::Shape, shape_icon(self.tools.shape_kind), "Shapes (click again for shape, colour and size; Shift keeps lines at 45° and boxes square)"),
            (Tool::Eraser, Icon::Eraser, "Eraser: rubs out whole marks"),
        ] {
            let ink = match tool {
                Tool::Pen => Some(self.tools.pen.0),
                Tool::Highlighter => Some(self.tools.highlighter.0),
                Tool::Shape => Some(self.tools.shape.0),
                _ => None,
            };
            let resp = icon_button(ui, icon, ink, self.tools.tool == tool, true, tip);
            let again = resp.clicked() && self.tools.tool == tool;
            if resp.clicked() {
                self.tools.tool = tool;
                self.crop = None;
                self.drawing = None;
            }
            if ink.is_some() {
                egui::Popup::from_response(&resp)
                    .open_memory(again.then_some(egui::SetOpenCommand::Toggle))
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| self.ink_options(ui, tool));
            }
        }
        divider(ui);
        if icon_button(ui, Icon::Crop, None, self.tools.tool == Tool::Crop, true, "Crop: drag a box, then Enter").clicked() {
            self.tools.tool = Tool::Crop;
        }
        if self.crop.is_some() {
            if icon_button(ui, Icon::Check, None, false, true, "Apply crop (Enter)").clicked() {
                self.apply_crop();
            }
            if icon_button(ui, Icon::Close, None, false, true, "Cancel crop (Esc)").clicked() {
                self.crop = None;
            }
        }
        divider(ui);
        if icon_button(ui, Icon::Undo, None, false, self.doc.can_undo(), "Undo (Ctrl+Z)").clicked() {
            self.step(false);
        }
        if icon_button(ui, Icon::Redo, None, false, self.doc.can_redo(), "Redo (Ctrl+Y)").clicked() {
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
    }

    fn canvas(&mut self, ui: &mut egui::Ui, guard: &PopupClickGuard) {
        let ctx = ui.ctx().clone();
        let Snip { doc, tools, texture, drawing, erase_marked, crop, zoom: zoom_choice, key_zoom } = self;
        let rev = (doc.image_rev, doc.ink_rev);
        if texture.as_ref().is_none_or(|c| c.rev != rev) {
            // ceiling: erase/undo re-flatten the whole snip (~150 ms for 20 long strokes on 4K); burn per-stroke tiles if erasing feels sticky.
            let shown = if doc.marks.is_empty() { doc.image.clone() } else { Arc::new(doc.flatten()) };
            match texture {
                Some(c) => {
                    c.tex.set(shown.clone(), TextureOptions::LINEAR);
                    *c = CanvasTex::new(c.tex.clone(), shown, rev);
                }
                None => {
                    let tex = ctx.load_texture("snip", shown.clone(), TextureOptions::LINEAR);
                    *texture = Some(CanvasTex::new(tex, shown, rev));
                }
            }
        }
        let Some(canvas) = texture else { return };

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
            painter.image(canvas.tex.id(), img_rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);

            let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
            // Drags are noticed a few pixels after the button goes down; start where it went down.
            let origin = ctx.input(|i| i.pointer.press_origin()).or(pointer);
            let swallowed = guard.swallows(&ctx, resp.clicked());
            match tools.tool {
                _ if swallowed => {}
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
                Tool::Crop => {
                    if resp.hovered() {
                        ctx.set_cursor_icon(CursorIcon::Crosshair);
                    }
                    let clamp = |p: Pos2| p.clamp(Pos2::ZERO, px_size.to_pos2());
                    if resp.drag_started() {
                        if let Some(p) = origin {
                            *crop = Some((clamp(to_px(p)), clamp(to_px(p))));
                        }
                    }
                    if resp.dragged() {
                        if let (Some((_, b)), Some(p)) = (crop.as_mut(), pointer) {
                            *b = clamp(to_px(p));
                        }
                    }
                    if let Some((a, b)) = *crop {
                        let sel = Rect::from_two_pos(to_pt(a), to_pt(b));
                        dim_outside(painter, img_rect, sel);
                        painter.rect_stroke(sel, 0.0, (1.5, Color32::WHITE), StrokeKind::Outside);
                        let r = Rect::from_two_pos(a, b);
                        painter.text(
                            sel.left_top() + vec2(0.0, -6.0),
                            Align2::LEFT_BOTTOM,
                            format!("{:.0} × {:.0}  ·  Enter to crop", r.width(), r.height()),
                            FontId::proportional(12.0),
                            Color32::WHITE,
                        );
                    }
                }
            }
            match drawing {
                Some(m) => canvas.show_live(m),
                None => canvas.clear_live(),
            }
        });
        zoom_badge(ui.painter(), view_rect, zoom);
    }
}

impl Editor {
    fn new(content: Content, title: String, viewport: ViewportId, root_hwnd: isize, place: Option<[i32; 2]>, new_tip: String, min_width: f32) -> Self {
        let (note_tx, note_rx) = channel();
        Self {
            content,
            title,
            viewport,
            root_hwnd,
            hwnd: None,
            place,
            settle: SETTLE_FRAMES,
            min_width,
            popup_guard: PopupClickGuard::default(),
            note: None,
            note_tx,
            note_rx,
            new_tip,
            new_requested: false,
            closed: false,
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
        self.settle_window(&ctx);
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
    }

    /// The window's own handle, once Windows has created it.
    fn own_hwnd(&mut self) -> Option<HWND> {
        if self.hwnd.is_none() {
            self.hwnd = win::find_window(&self.title).map(|h| h as isize);
        }
        self.hwnd.map(|h| h as HWND)
    }

    /// Owner for dialogs: this window, or the shell's until this one is found.
    fn owner(&mut self) -> HWND {
        self.own_hwnd().unwrap_or(self.root_hwnd as HWND)
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
        let Keys { new, save, undo, redo, copy, enter, esc, zoom_in, zoom_out, zoom_fit } = ctx.input_mut(|i| read_keys(i, popup_open, focused));
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
        if undo {
            snip.step(false);
        }
        if redo {
            snip.step(true);
        }
        if enter {
            snip.apply_crop();
        }
        if esc {
            snip.crop = None;
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
        egui::Panel::top("editor_toolbar")
            .frame(egui::Frame::new().fill(TOOLBAR_BG).inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
                let row = Rect::from_min_size(ui.available_rect_before_wrap().min, vec2(ui.available_width(), BTN.y));
                ui.allocate_rect(row, Sense::hover());
                let layout = |l| egui::UiBuilder::new().max_rect(row).layout(l);

                let mut left = ui.new_child(layout(Layout::left_to_right(Align::Center)));
                if widgets::new_button(&mut left, &self.new_tip).clicked() {
                    self.request_new(&left.ctx().clone());
                }
                let left_end = left.min_rect().max.x;

                let mut right = ui.new_child(layout(Layout::right_to_left(Align::Center)));
                self.output_controls(&mut right);
                let right_start = right.min_rect().min.x;

                if let Content::Snip(snip) = &mut self.content {
                    let width = snip.markup_width();
                    let x = markup_x(row.center().x, width, left_end, right_start);
                    let mid_rect = Rect::from_min_size(pos2(x, row.min.y), vec2(width + 1.0, BTN.y));
                    let mut mid = ui.new_child(egui::UiBuilder::new().max_rect(mid_rect).layout(Layout::left_to_right(Align::Center)));
                    snip.markup_controls(&mut mid);
                }
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

    fn copy(&self, ctx: &egui::Context) {
        let Content::Snip(snip) = &self.content else { return };
        let (image, hwnd, report) = (snip.flattened(), self.root_hwnd, self.notifier(ctx));
        std::thread::spawn(move || {
            report.send(match win::copy_image(hwnd as HWND, &image) {
                Ok(()) => "Copied to clipboard".to_string(),
                Err(e) => format!("Couldn't copy: {e}"),
            });
        });
    }

    /// Asks where to save, then writes the file on a worker thread: a big PNG takes a moment to
    /// encode, and a long recording a moment to copy.
    fn save_as(&mut self, ctx: &egui::Context) {
        let owner = self.owner();
        let report = self.notifier(ctx);
        match &self.content {
            Content::Snip(snip) => {
                let start = win::pictures_dir().map(|d| d.join("Screenshots"));
                let Some(path) = win::ask_save_path(owner, start.as_deref(), &format!("Snip {}", win::timestamp()), ("PNG image", "png")) else {
                    return;
                };
                let image = snip.flattened();
                std::thread::spawn(move || report.send(saved_note(&path, win::save_png(&path, &image))));
            }
            Content::Video(v) => {
                let start = win::videos_dir().map(|d| d.join("Screen Recordings"));
                let name = v.path.file_stem().map_or_else(|| "Screen Recording".into(), |n| n.to_string_lossy().into_owned());
                let Some(path) = win::ask_save_path(owner, start.as_deref(), &name, ("MP4 video", "mp4")) else {
                    return;
                };
                let from = v.path.clone();
                std::thread::spawn(move || report.send(saved_note(&path, std::fs::copy(&from, &path).map(|_| ()).map_err(|e| e.to_string()))));
            }
        }
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
}

/// All open editor windows, oldest first.
pub struct Editors {
    slots: Vec<Slot>,
    next: u64,
    icon: Option<Arc<egui::IconData>>,
    new_tip: String,
}

/// What the editor windows asked of the shell since the last `Editors::poll`.
#[derive(Debug, Default, PartialEq)]
pub struct Requests {
    pub new_snip: bool,
    pub closed: usize,
}

impl Editors {
    pub fn new(icon: Option<Arc<egui::IconData>>) -> Self {
        Self { slots: Vec::new(), next: 0, icon, new_tip: String::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    #[cfg(test)]
    pub fn ids(&self) -> Vec<ViewportId> {
        self.slots.iter().map(|s| s.id).collect()
    }

    /// Runs `f` on the editor in window `id`, if it is still open.
    pub fn with<R>(&self, id: ViewportId, f: impl FnOnce(&mut Editor) -> R) -> Option<R> {
        let slot = self.slots.iter().find(|s| s.id == id)?;
        let mut ed = slot.editor.lock().unwrap_or_else(PoisonError::into_inner);
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
        self.slots.push(Slot { id, title, builder, editor: Arc::new(Mutex::new(editor)) });
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

    /// Keeps every New button's tooltip in step with the shell's capture mode and hotkey.
    pub fn set_new_tip(&mut self, tip: String) {
        if tip == self.new_tip {
            return;
        }
        for slot in &self.slots {
            slot.editor.lock().unwrap_or_else(PoisonError::into_inner).new_tip = tip.clone();
        }
        self.new_tip = tip;
    }

    /// The toast of the newest editor window; `None` when none is open.
    pub fn newest_notifier(&self, ctx: &egui::Context) -> Option<Notifier> {
        let slot = self.slots.last()?;
        let ed = slot.editor.lock().unwrap_or_else(PoisonError::into_inner);
        Some(ed.notifier(ctx))
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

/// The snip as shown: the picture with committed marks burned in by the same rasteriser that saves
/// it. A mark still being drawn is painted over `shown` on the CPU, tile run by tile run, straight
/// into the texture, so the preview is pixel for pixel what the mark will burn (multiply included).
struct CanvasTex {
    tex: TextureHandle,
    /// The picture and its committed marks: exactly what copy and save write.
    shown: Arc<ColorImage>,
    /// `(image_rev, ink_rev)` of the doc that `shown` was built from.
    rev: (u64, u64),
    live: Live,
}

/// The texture tiles that show a mark still being drawn instead of `shown`.
struct Live {
    tiles: Tiles,
    /// Points of the stroke already painted into `tiles`.
    done: usize,
    /// Any other mark, as last painted.
    last: Option<Mark>,
}

impl Live {
    fn new(size: [usize; 2]) -> Self {
        Self { tiles: Tiles::new(size), done: 0, last: None }
    }
}

impl CanvasTex {
    fn new(tex: TextureHandle, shown: Arc<ColorImage>, rev: (u64, u64)) -> Self {
        let live = Live::new(shown.size);
        Self { tex, shown, rev, live }
    }

    /// Uploads the runs of `tiles` with `m` painted over the committed picture, or plain when `m` is `None`.
    fn upload(&mut self, tiles: &Tiles, m: Option<&Mark>) {
        for r in tiles.runs() {
            let mut patch = markup::sub_image(&self.shown, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
            if let Some(m) = m {
                m.paint(&mut patch, r.pos());
            }
            self.tex.set_partial(r.pos(), patch, TextureOptions::LINEAR);
        }
    }

    /// Adds a finished mark, burning just that mark into the picture and texture rather than
    /// re-flattening the whole snip.
    fn commit(&mut self, doc: &mut Doc, m: Mark) {
        let current = self.rev == (doc.image_rev, doc.ink_rev);
        if current {
            let mut tiles = Tiles::new(doc.size());
            m.footprint(&mut tiles);
            let shown = Arc::make_mut(&mut self.shown);
            for r in tiles.runs() {
                let mut patch = markup::sub_image(shown, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
                m.paint(&mut patch, r.pos());
                markup::paste(shown, &patch, r.pos());
                self.tex.set_partial(r.pos(), patch, TextureOptions::LINEAR);
            }
            // Live tiles the mark no longer reaches go back to the picture.
            let stale = self.live.tiles.minus(&tiles);
            self.upload(&stale, None);
        }
        doc.add_mark(m);
        if current {
            self.rev.1 = doc.ink_rev;
        }
        self.live = Live::new(self.shown.size);
    }

    /// Shows `m`, the mark being drawn. A stroke only grows, so only the tiles its newest segments
    /// reach are repainted; any other mark is repainted where it is now, and the tiles it has left
    /// go back to the picture. Nothing is redone while the mark stays the same.
    fn show_live(&mut self, m: &Mark) {
        let mut fresh = Tiles::new(self.shown.size);
        match m {
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
            }
            _ => {
                if self.live.last.as_ref() == Some(m) {
                    return;
                }
                m.footprint(&mut fresh);
                let left = self.live.tiles.minus(&fresh);
                self.upload(&left, None);
                self.live.tiles = fresh.clone();
                self.live.last = Some(m.clone());
            }
        }
        self.upload(&fresh, Some(m));
    }

    /// Puts back the committed picture wherever a cancelled mark was showing.
    fn clear_live(&mut self) {
        if !self.live.tiles.is_empty() {
            let tiles = std::mem::replace(&mut self.live.tiles, Tiles::new([0, 0]));
            self.upload(&tiles, None);
        }
        self.live = Live::new(self.shown.size);
    }
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
fn step_history(doc: &mut Doc, crop: &mut Option<(Pos2, Pos2)>, drawing: &mut Option<Mark>, redo: bool) {
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

    fn white(w: usize, h: usize) -> ColorImage {
        ColorImage::new([w, h], vec![Color32::WHITE; w * h])
    }

    #[test]
    fn undo_and_redo_drop_the_pending_crop_box() {
        let mut doc = Doc::new(white(100, 80));
        doc.crop(Rect::from_min_max(pos2(10.0, 10.0), pos2(60.0, 50.0)));
        assert_eq!(doc.size(), [50, 40]);
        let box_on_cropped = Some((pos2(5.0, 5.0), pos2(45.0, 35.0)));

        let mut crop = box_on_cropped;
        step_history(&mut doc, &mut crop, &mut None, false);
        assert_eq!(doc.size(), [100, 80], "undo restores the larger image");
        assert_eq!(crop, None, "the 50x40 box must not carry over to the 100x80 image");

        let mut crop = Some((pos2(80.0, 60.0), pos2(99.0, 79.0)));
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

    /// Group widths as laid out by `Editor::toolbar`: New button 80, output 133, markup with crop buttons 390.
    const NEW_END: f32 = 12.0 + 80.0;
    const OUT_W: f32 = 133.0;
    const MARKUP_W: f32 = 390.0;

    #[test]
    fn markup_width_matches_the_measured_toolbar() {
        let mut snip = Snip::new(white(4, 4), Tools::default());
        assert_eq!(snip.markup_width(), 7.0 * BTN.x + 2.0 * DIVIDER_W + 8.0 * GAP);
        snip.crop = Some((Pos2::ZERO, pos2(2.0, 2.0)));
        assert_eq!(snip.markup_width(), MARKUP_W);
        const { assert!(NEW_END + GROUP_GAP + MARKUP_W + GROUP_GAP + OUT_W + 12.0 <= MIN_EDIT_W, "MIN_EDIT_W fits all three groups") };
    }

    #[test]
    fn markup_group_never_overlaps_its_neighbours() {
        for window in [MIN_EDIT_W as i32, 900, 1200, 2000] {
            let row_end = window as f32 - 12.0;
            let right_start = row_end - OUT_W;
            let x = markup_x((12.0 + row_end) / 2.0, MARKUP_W, NEW_END, right_start);
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
            let row_end = window - 12.0;
            let x = markup_x((12.0 + row_end) / 2.0, MARKUP_W, NEW_END, row_end - OUT_W);
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
        let tex = ctx.load_texture("t", ColorImage::new([1, 1], vec![Color32::BLACK]), TextureOptions::LINEAR);
        let shown = Arc::new(snip.doc.flatten());
        snip.texture = Some(CanvasTex::new(tex, shown.clone(), (snip.doc.image_rev, snip.doc.ink_rev)));
        assert!(Arc::ptr_eq(&snip.flattened(), &shown), "a current canvas is reused, not re-burned");
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
        fn apply(&mut self, delta: &egui::TexturesDelta, id: Option<egui::TextureId>) {
            let Some(id) = id else { return };
            for d in delta.set.get(&id).into_iter().flatten() {
                let egui::ImageData::Color(img) = &d.image;
                match (d.pos, self.image.as_mut()) {
                    (None, _) => self.image = Some((**img).clone()),
                    (Some(at), Some(screen)) => markup::paste(screen, img, at),
                    (Some(_), None) => panic!("partial upload before the texture exists"),
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
        screen.apply(&out.textures_delta, snip.texture.as_ref().map(|c| c.tex.id()));
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
    fn shapes_show_exactly_what_is_saved_while_they_are_dragged() {
        for (kind, _, _) in SHAPES {
            let mut snip = Snip::new(gradient(300, 200), Tools { tool: Tool::Shape, shape_kind: kind, ..Tools::default() });
            drag_and_compare(&mut snip, &PATH);
            assert!(matches!(&snip.doc.marks[0], Mark::Shape(s) if s.kind == kind && s.to == pos2(600.0 - 640.0 + 150.0, 360.0 - 400.0 + 100.0)), "{kind:?} ends where the drag ended");
        }
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
    fn new_button_tooltips_follow_the_shell() {
        let ctx = egui::Context::default();
        let mut editors = Editors::new(None);
        let (a, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        editors.set_new_tip("New recording (Ctrl+N)".into());
        let (b, _, _) = editors.open_snip(&ctx, white(10, 10), 0, None);
        for id in [a, b] {
            assert_eq!(editors.with(id, |e| e.new_tip.clone()).unwrap(), "New recording (Ctrl+N)");
        }
    }
}
