#![windows_subsystem = "windows"]

mod audio;
mod icons;
mod markup;
mod recorder;
mod win;

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CornerRadius, CursorIcon, Event, FontId,
    Key, KeyboardShortcut, Layout, Modifiers, Pos2, Rect, RichText, Sense, Shape, StrokeKind,
    TextureHandle, TextureOptions, Vec2, ViewportCommand, WindowLevel,
};
use icons::Icon;
use markup::{Doc, Ink, PxRect, Stroke};
use windows_sys::Win32::Foundation::HWND;

const ACCENT: Color32 = Color32::from_rgb(0xff, 0xb6, 0x34);
const TOOLBAR_BG: Color32 = Color32::from_rgb(0x20, 0x20, 0x20);
const CANVAS_BG: Color32 = Color32::from_rgb(0x2c, 0x2c, 0x2c);
const BUTTON_BG: Color32 = Color32::from_rgb(0x2d, 0x2d, 0x2d);
const BUTTON_HOVER: Color32 = Color32::from_rgb(0x34, 0x34, 0x34);
const OUTLINE: Color32 = Color32::from_rgb(0x45, 0x45, 0x45);
const DIVIDER: Color32 = Color32::from_rgb(0x31, 0x31, 0x31);
const ICON: Color32 = Color32::from_rgb(0xe8, 0xe8, 0xe8);
const ICON_OFF: Color32 = Color32::from_rgb(0x6b, 0x6b, 0x6b);
const BTN: Vec2 = vec2(36.0, 34.0);
const GAP: f32 = 4.0;
const DIVIDER_W: f32 = 13.0;
/// Narrowest window that fits the capture, markup (with the crop buttons) and output groups side by side.
const MIN_WINDOW_W: f32 = 520.0;
const MIN_WINDOW_H: f32 = 240.0;
/// Measured: capture group ~298 + markup with crop buttons 350 + output group 133 + 2 gaps + 24 toolbar margin = 837.
const MIN_EDIT_W: f32 = 850.0;
/// Clear space kept between the toolbar groups.
const GROUP_GAP: f32 = 16.0;
/// Snip zoom change per Ctrl+Plus / Ctrl+Minus press.
const ZOOM_STEP: f32 = 1.25;
const DIM: Color32 = Color32::from_black_alpha(120);
/// Time for the window's hide animation to finish before the screen is copied.
const HIDE_SETTLE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Photo,
    Video,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape_ {
    Rectangle,
    Window,
    FullScreen,
    Freeform,
}

impl Shape_ {
    const ALL: [Shape_; 4] = [Shape_::Rectangle, Shape_::Window, Shape_::FullScreen, Shape_::Freeform];

    /// Shapes on offer in each mode; a video can't be freeform.
    fn choices(mode: Mode) -> &'static [Shape_] {
        match mode {
            Mode::Photo => &Self::ALL,
            Mode::Video => &Self::ALL[..3],
        }
    }

    fn icon(self) -> Icon {
        match self {
            Shape_::Rectangle => Icon::Rectangle,
            Shape_::Window => Icon::Window,
            Shape_::FullScreen => Icon::FullScreen,
            Shape_::Freeform => Icon::Freeform,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Shape_::Rectangle => "Rectangle",
            Shape_::Window => "Window",
            Shape_::FullScreen => "Full screen",
            Shape_::Freeform => "Freeform",
        }
    }
}

const DELAYS: [u64; 4] = [0, 3, 5, 10];

fn delay_label(s: u64) -> String {
    if s == 0 { "No delay".into() } else { format!("{s} seconds") }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Pen,
    Highlighter,
    Eraser,
    Crop,
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

struct Picker {
    grab: win::Grab,
    texture: TextureHandle,
    /// Drag start in image pixels.
    start: Option<Pos2>,
    /// Freeform outline in image pixels.
    outline: Vec<Pos2>,
    /// Screen pixels the window's drawing area must cover: x, y, w, h.
    target: [i32; 4],
    shown: bool,
    tries: u8,
    /// Monitor under the mouse when picking began, in image pixels; the
    /// helper bar sits at its top.
    home: [i32; 4],
}

/// The window shrunk to the recording bar above the chosen area.
struct RecBar {
    /// Screen pixels being recorded: x, y, w, h.
    area: [i32; 4],
    rec: Option<recorder::Recording>,
    saving: bool,
    notes: Vec<String>,
}

struct VideoResult {
    path: PathBuf,
    seconds: f64,
    size: [u32; 2],
    thumb: Option<TextureHandle>,
}

enum Stage {
    Idle,
    /// Window hidden; the screen is about to be copied.
    Waiting,
    Picking(Box<Picker>),
    Recording(Box<RecBar>),
}

const BAR: Vec2 = vec2(372.0, 52.0);
const FRAME_TITLE: &str = "Snipscreen recording area";

struct ScreenshotJob {
    path: PathBuf,
    frames: u32,
    rx: Option<Receiver<Arc<ColorImage>>>,
}

struct App {
    hwnd: isize,
    mode: Mode,
    shape: Shape_,
    delay: u64,
    stage: Stage,
    grab_tx: Sender<Result<win::Grab, String>>,
    grab_rx: Receiver<Result<win::Grab, String>>,
    hotkey_rx: Option<Receiver<()>>,
    hotkey_note: Option<String>,
    note_tx: Sender<String>,
    note_rx: Receiver<String>,
    note: Option<(String, f64)>,
    /// Window corner before the picker took over the screen, in pixels.
    restore_corner: Option<[i32; 2]>,
    /// Frames left in which to put the returning window back on screen.
    settle: u8,

    doc: Option<Doc>,
    texture: Option<CanvasTex>,
    tool: Tool,
    pen: (Color32, f32),
    highlighter: (Color32, f32),
    drawing: Option<Stroke>,
    erase_marked: bool,
    crop: Option<(Pos2, Pos2)>,
    /// `None` fits the snip to the window; otherwise 1.0 is one screen pixel per pixel.
    zoom: Option<f32>,
    /// Minimum window width last sent to the OS, so the command goes out only on change.
    min_width: f32,
    popup_guard: PopupClickGuard,
    /// Zoom factor requested from the keyboard this frame; `canvas` applies it.
    key_zoom: f32,

    auto_copy: bool,
    auto_save: bool,
    screenshot: Option<ScreenshotJob>,
    /// Open straight into picking, like the original does.
    pick_on_launch: bool,

    record_system: bool,
    record_mic: bool,
    /// `None` is the Windows default microphone.
    mic_choice: Option<String>,
    mics: Vec<audio::Mic>,
    rec_tx: Sender<recorder::Event>,
    rec_rx: Receiver<recorder::Event>,
    video: Option<VideoResult>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, screenshot: Option<PathBuf>) -> Self {
        style(&cc.egui_ctx);
        let hwnd = window_hwnd(cc);
        let (grab_tx, grab_rx) = channel();
        let (note_tx, note_rx) = channel();
        let (rec_tx, rec_rx) = channel();
        let (mut hotkey_rx, mut hotkey_note) = (None, None);
        // A screenshot run must not steal the shortcut from a running copy.
        if screenshot.is_none() {
            let (tx, rx) = channel();
            let ctx = cc.egui_ctx.clone();
            match win::listen_hotkey(move || {
                let _ = tx.send(());
                ctx.request_repaint();
            }) {
                Ok(()) => hotkey_rx = Some(rx),
                Err(e) => hotkey_note = Some(e),
            }
        }
        Self {
            hwnd,
            mode: Mode::Photo,
            shape: Shape_::Rectangle,
            delay: 0,
            stage: Stage::Idle,
            grab_tx,
            grab_rx,
            hotkey_rx,
            hotkey_note,
            note_tx,
            note_rx,
            note: None,
            restore_corner: None,
            settle: 0,
            doc: None,
            texture: None,
            tool: Tool::Pen,
            pen: (PEN_COLORS[2], 4.0),
            highlighter: (HIGHLIGHT_COLORS[0], 18.0),
            drawing: None,
            erase_marked: false,
            crop: None,
            zoom: None,
            min_width: MIN_WINDOW_W,
            popup_guard: PopupClickGuard::default(),
            key_zoom: 1.0,
            auto_copy: true,
            auto_save: true,
            pick_on_launch: screenshot.is_none(),
            screenshot: screenshot.map(|path| ScreenshotJob { path, frames: 0, rx: None }),
            record_system: true,
            record_mic: false,
            mic_choice: None,
            mics: Vec::new(),
            rec_tx,
            rec_rx,
            video: None,
        }
    }

    fn start_snip(&mut self, ctx: &egui::Context) {
        if !matches!(self.stage, Stage::Idle) {
            return;
        }
        self.restore_corner = win::window_corner(self.hwnd as HWND);
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.stage = Stage::Waiting;
        let delay = if self.mode == Mode::Photo { self.delay } else { 0 };
        let (tx, ctx, wait) = (
            self.grab_tx.clone(),
            ctx.clone(),
            Duration::from_secs(delay) + HIDE_SETTLE,
        );
        std::thread::spawn(move || {
            std::thread::sleep(wait);
            let _ = tx.send(win::grab_screen());
            ctx.request_repaint();
        });
    }

    fn on_grab(&mut self, ctx: &egui::Context, grab: Result<win::Grab, String>) {
        let grab = match grab {
            Ok(g) => g,
            Err(e) => {
                self.stage = Stage::Idle;
                self.show_window(ctx);
                self.say(ctx, format!("Couldn't take the snip: {e}"));
                return;
            }
        };
        if self.shape == Shape_::FullScreen {
            match self.mode {
                Mode::Photo => {
                    self.stage = Stage::Idle;
                    self.show_window(ctx);
                    self.finish(ctx, grab.image);
                }
                Mode::Video => self.open_rec_bar(ctx, win::monitor_at_cursor()),
            }
            return;
        }
        let texture = ctx.load_texture("screen", grab.image.clone(), TextureOptions::LINEAR);
        for cmd in [
            ViewportCommand::Decorations(false),
            ViewportCommand::Resizable(false),
            ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop),
        ] {
            ctx.send_viewport_cmd(cmd);
        }
        // ceiling: one window spans every monitor and draws at a single scale,
        // so the frozen picture is resampled on monitors with other scaling;
        // one picker window per monitor if that ever looks soft.
        let target = [grab.origin[0], grab.origin[1], grab.image.size[0] as i32, grab.image.size[1] as i32];
        let [mx, my, mw, mh] = win::monitor_at_cursor();
        let home = [mx - grab.origin[0], my - grab.origin[1], mw, mh];
        self.stage = Stage::Picking(Box::new(Picker {
            home,
            grab,
            texture,
            start: None,
            outline: Vec::new(),
            target,
            shown: false,
            tries: 0,
        }));
        ctx.request_repaint();
    }

    /// Lines the hidden picker window up with the screen pixels, then shows it.
    fn reveal_picker(&mut self, ctx: &egui::Context) {
        let Stage::Picking(pk) = &mut self.stage else { return };
        if pk.shown {
            return;
        }
        let [x, y, w, h] = pk.target;
        if win::fit_client(self.hwnd as HWND, x, y, w, h) || pk.tries > 30 {
            pk.shown = true;
            for cmd in [ViewportCommand::Minimized(false), ViewportCommand::Visible(true), ViewportCommand::Focus] {
                ctx.send_viewport_cmd(cmd);
            }
        } else {
            pk.tries += 1;
            ctx.request_repaint_after(Duration::from_millis(10));
        }
    }

    /// Back from the picker (or from hiding) to a normal window.
    fn show_window(&mut self, ctx: &egui::Context) {
        for cmd in [
            ViewportCommand::WindowLevel(WindowLevel::Normal),
            ViewportCommand::Decorations(true),
            ViewportCommand::Resizable(true),
            ViewportCommand::Minimized(false),
            ViewportCommand::Visible(true),
            ViewportCommand::Focus,
        ] {
            ctx.send_viewport_cmd(cmd);
        }
    }

    fn place_window(&mut self, ctx: &egui::Context, size: Vec2) {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        self.settle = 8;
    }

    /// Runs for a few frames after the picker: back to the old corner, then
    /// kept inside the monitor once Windows has finished any rescaling.
    fn settle_window(&mut self, ctx: &egui::Context) {
        if self.settle == 0 || !matches!(self.stage, Stage::Idle) {
            return;
        }
        self.settle -= 1;
        win::place_on_screen(self.hwnd as HWND, self.restore_corner.take());
        ctx.request_repaint();
    }

    fn cancel_pick(&mut self, ctx: &egui::Context) {
        self.stage = Stage::Idle;
        self.show_window(ctx);
        self.place_window(ctx, self.window_size_for(ctx));
    }

    fn window_size_for(&self, ctx: &egui::Context) -> Vec2 {
        let size = match (&self.doc, &self.video) {
            (Some(doc), _) => [doc.size()[0] as f32, doc.size()[1] as f32],
            (None, Some(v)) => [v.size[0] as f32, v.size[1] as f32 + 90.0],
            (None, None) => return vec2(620.0, 330.0),
        };
        let ppp = ctx.pixels_per_point();
        let monitor = ctx.input(|i| i.viewport().monitor_size).unwrap_or(vec2(1920.0, 1080.0));
        let snip = vec2(size[0], size[1]) / ppp;
        (snip + vec2(48.0, 120.0)).clamp(vec2(MIN_EDIT_W, 440.0), (monitor * 0.85).max(vec2(MIN_EDIT_W, 440.0)))
    }

    fn finish(&mut self, ctx: &egui::Context, image: ColorImage) {
        self.stage = Stage::Idle;
        self.show_window(ctx);
        self.video = None;
        self.doc = Some(Doc::new(image));
        self.texture = None;
        self.crop = None;
        self.drawing = None;
        self.zoom = None;
        self.place_window(ctx, self.window_size_for(ctx));
        let Some(doc) = &self.doc else { return };
        let image = doc.image.clone();
        if !self.auto_copy && !self.auto_save {
            return;
        }
        // One worker does both jobs so the toast can report what each really did.
        let save_path = self.auto_save.then(|| {
            win::pictures_dir()
                .map(|dir| dir.join("Screenshots").join(format!("Snip {}.png", win::timestamp())))
                .ok_or_else(|| "couldn't find your Pictures folder".to_string())
        });
        let (copy, tx, ctx, hwnd) = (self.auto_copy, self.note_tx.clone(), ctx.clone(), self.hwnd);
        std::thread::spawn(move || {
            let copied = copy.then(|| win::copy_image(hwnd as HWND, &image));
            let saved = save_path.map(|path| path.and_then(|p| win::save_png(&p, &image)));
            if let Some(msg) = auto_note(copied, saved) {
                let _ = tx.send(msg);
                ctx.request_repaint();
            }
        });
    }

    fn copy_in_background(&self, ctx: &egui::Context, image: Arc<ColorImage>, announce: bool) {
        let (tx, ctx, hwnd) = (self.note_tx.clone(), ctx.clone(), self.hwnd);
        std::thread::spawn(move || {
            let msg = match win::copy_image(hwnd as HWND, &image) {
                Ok(()) if announce => Some("Copied to clipboard".to_string()),
                Ok(()) => None,
                Err(e) => Some(format!("Couldn't copy: {e}")),
            };
            if let Some(m) = msg {
                let _ = tx.send(m);
                ctx.request_repaint();
            }
        });
    }

    fn copy(&self, ctx: &egui::Context) {
        if let Some(doc) = &self.doc {
            self.copy_in_background(ctx, Arc::new(doc.flatten()), true);
        }
    }

    fn save_as(&mut self, ctx: &egui::Context) {
        let Some(doc) = &self.doc else { return };
        let start = win::pictures_dir().map(|d| d.join("Screenshots"));
        let Some(path) = win::ask_save_path(self.hwnd as HWND, start.as_deref(), &format!("Snip {}", win::timestamp()), ("PNG image", "png"))
        else {
            return;
        };
        let msg = match win::save_png(&path, &doc.flatten()) {
            Ok(()) => format!("Saved {}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into())),
            Err(e) => format!("Couldn't save: {e}"),
        };
        self.say(ctx, msg);
    }

    fn say(&mut self, ctx: &egui::Context, msg: String) {
        self.note = Some((msg, ctx.input(|i| i.time) + 4.0));
        ctx.request_repaint_after(Duration::from_secs_f32(4.1));
    }
}

impl eframe::App for App {
    /// Transparent so the recording-area outline window shows only its dashes.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if std::mem::take(&mut self.pick_on_launch) {
            self.start_snip(ctx);
        }
        while let Ok(msg) = self.note_rx.try_recv() {
            self.say(ctx, msg);
        }
        while let Ok(event) = self.rec_rx.try_recv() {
            self.on_record_event(ctx, event);
        }
        if let Some(rx) = &self.hotkey_rx {
            if rx.try_iter().count() > 0 {
                self.start_snip(ctx);
            }
        }
        if let Ok(grab) = self.grab_rx.try_recv() {
            self.on_grab(ctx, grab);
        }
        self.reveal_picker(ctx);
        self.settle_window(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if matches!(self.stage, Stage::Picking(_)) {
            self.picker_ui(ui);
        } else if matches!(self.stage, Stage::Recording(_)) {
            self.rec_bar_ui(ui);
        } else {
            self.popup_guard.begin_frame(&ctx);
            self.shortcuts(&ctx);
            self.toolbar(ui);
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(ui.style().visuals.extreme_bg_color))
                .show(ui, |ui| {
                    if self.doc.is_some() {
                        self.canvas(ui);
                    } else if self.video.is_some() {
                        self.video_view(ui);
                    } else {
                        self.empty_state(ui);
                    }
                });
            self.toast(&ctx);
        }
        self.drive_screenshot(&ctx);
    }
}

impl App {
    fn shortcuts(&mut self, ctx: &egui::Context) {
        let Keys { new, save, undo, redo, copy, enter, esc, zoom_in, zoom_out, zoom_fit } = ctx.input_mut(read_keys);
        if new {
            self.start_snip(ctx);
        }
        if self.video.is_some() && save {
            self.save_video_as(ctx);
        }
        if self.doc.is_none() {
            return;
        }
        if save {
            self.save_as(ctx);
        }
        if copy {
            self.copy(ctx);
        }
        if let Some(doc) = &mut self.doc {
            if undo {
                doc.undo();
            }
            if redo {
                doc.redo();
            }
        }
        if enter {
            self.apply_crop();
        }
        if esc {
            self.crop = None;
        }
        if zoom_fit {
            self.zoom = None;
        } else if zoom_in != zoom_out {
            self.key_zoom = if zoom_in { ZOOM_STEP } else { 1.0 / ZOOM_STEP };
        }
    }

    fn apply_crop(&mut self) {
        if let (Some((a, b)), Some(doc)) = (self.crop.take(), &mut self.doc) {
            doc.crop(Rect::from_two_pos(a, b));
            self.zoom = None;
        }
    }

    /// With a snip open the toolbar needs room for all three groups; without one the window may shrink further.
    fn sync_min_width(&mut self, ctx: &egui::Context) {
        let want = if self.doc.is_some() { MIN_EDIT_W } else { MIN_WINDOW_W };
        if want != self.min_width {
            self.min_width = want;
            ctx.send_viewport_cmd(ViewportCommand::MinInnerSize(vec2(want, MIN_WINDOW_H)));
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        self.sync_min_width(&ui.ctx().clone());
        egui::Panel::top("toolbar")
            .frame(egui::Frame::new().fill(TOOLBAR_BG).inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
                let row = Rect::from_min_size(ui.available_rect_before_wrap().min, vec2(ui.available_width(), BTN.y));
                ui.allocate_rect(row, Sense::hover());
                let layout = |l| egui::UiBuilder::new().max_rect(row).layout(l);

                let mut left = ui.new_child(layout(Layout::left_to_right(Align::Center)));
                self.capture_controls(&mut left);
                let left_end = left.min_rect().max.x;

                let mut right = ui.new_child(layout(Layout::right_to_left(Align::Center)));
                self.output_controls(&mut right);
                let right_start = right.min_rect().min.x;

                if self.doc.is_some() {
                    let width = self.markup_width();
                    let x = markup_x(row.center().x, width, left_end, right_start);
                    let mid_rect = Rect::from_min_size(pos2(x, row.min.y), vec2(width + 1.0, BTN.y));
                    let mut mid = ui.new_child(egui::UiBuilder::new().max_rect(mid_rect).layout(Layout::left_to_right(Align::Center)));
                    self.markup_controls(&mut mid);
                }
            });
    }

    fn capture_controls(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let (rect, resp) = ui.allocate_exact_size(vec2(80.0, BTN.y), Sense::click());
        let p = ui.painter();
        p.rect_filled(rect, 4.0, if resp.hovered() { BUTTON_HOVER } else { BUTTON_BG });
        let icon = Rect::from_center_size(rect.left_center() + vec2(20.0, 0.0), vec2(18.0, 18.0));
        icons::paint(p, icon, Icon::Plus, ACCENT, None);
        p.text(rect.left_center() + vec2(36.0, 0.0), Align2::LEFT_CENTER, "New", FontId::proportional(14.0), ICON);
        let tip = match self.mode {
            Mode::Photo => "New snip (Ctrl+N, or Alt+Shift+S anywhere)",
            Mode::Video => "New recording (Ctrl+N, or Alt+Shift+S anywhere)",
        };
        if resp.on_hover_text(tip).clicked() {
            self.start_snip(&ctx);
        }
        ui.add_space(8.0);
        self.mode_switch(ui);
        ui.add_space(4.0);
        self.shape_menu(ui);
        if self.mode == Mode::Video {
            return;
        }

        let (timer, badge) = if self.delay == 0 { (Icon::TimerOff, None) } else { (Icon::Timer, Some(self.delay)) };
        let resp = dropdown(ui, timer, badge).on_hover_text(format!("Delay: {}", delay_label(self.delay)));
        egui::Popup::menu(&resp).show(|ui| {
            for d in DELAYS {
                ui.selectable_value(&mut self.delay, d, delay_label(d));
            }
        });
    }

    fn mode_switch(&mut self, ui: &mut egui::Ui) {
        let (group, _) = ui.allocate_exact_size(vec2(BTN.x * 2.0 + 6.0, BTN.y), Sense::hover());
        ui.painter().rect_stroke(group, 5.0, (1.0, OUTLINE), StrokeKind::Inside);
        for (i, (mode, icon, tip)) in
            [(Mode::Photo, Icon::Camera, "Snip a picture"), (Mode::Video, Icon::Video, "Record a video")]
                .into_iter()
                .enumerate()
        {
            let cell = Rect::from_min_size(group.min + vec2(3.0 + i as f32 * BTN.x, 3.0), BTN - vec2(0.0, 6.0));
            let resp = ui.interact(cell, ui.scope_id().with(("mode", i)), Sense::click()).on_hover_text(tip);
            let p = ui.painter();
            if self.mode == mode {
                p.rect_filled(cell, 4.0, BUTTON_HOVER);
                let bar = Rect::from_center_size(cell.center_bottom() - vec2(0.0, 3.0), vec2(14.0, 3.0));
                p.rect_filled(bar, 1.5, ACCENT);
            } else if resp.hovered() {
                p.rect_filled(cell, 4.0, BUTTON_BG);
            }
            icons::paint(p, Rect::from_center_size(cell.center() - vec2(0.0, 1.0), vec2(18.0, 18.0)), icon, ICON, None);
            if resp.clicked() {
                self.set_mode(mode);
            }
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if !Shape_::choices(mode).contains(&self.shape) {
            self.shape = Shape_::Rectangle;
        }
    }

    fn shape_menu(&mut self, ui: &mut egui::Ui) {
        let resp = dropdown(ui, self.shape.icon(), None).on_hover_text(format!("Shape: {}", self.shape.label()));
        egui::Popup::menu(&resp).show(|ui| {
            for &s in Shape_::choices(self.mode) {
                let (rect, item) = ui.allocate_exact_size(vec2(150.0, 32.0), Sense::click());
                let p = ui.painter();
                if item.hovered() {
                    p.rect_filled(rect, 4.0, BUTTON_HOVER);
                }
                if self.shape == s {
                    let pill = Rect::from_center_size(rect.left_center() + vec2(3.0, 0.0), vec2(3.0, 16.0));
                    p.rect_filled(pill, 1.5, ACCENT);
                }
                icons::paint(p, Rect::from_center_size(rect.left_center() + vec2(22.0, 0.0), vec2(18.0, 18.0)), s.icon(), ICON, None);
                p.text(rect.left_center() + vec2(42.0, 0.0), Align2::LEFT_CENTER, s.label(), FontId::proportional(14.0), ICON);
                if item.clicked() {
                    self.shape = s;
                    ui.close();
                }
            }
        });
    }

    fn output_controls(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let more = icon_button(ui, Icon::More, None, false, true, "More");
        egui::Popup::menu(&more).show(|ui| {
            ui.checkbox(&mut self.auto_copy, "Copy snips to the clipboard automatically");
            ui.checkbox(&mut self.auto_save, "Save snips to Pictures\\Screenshots automatically");
            if ui.button("Open the Screenshots folder").clicked() {
                if let Some(dir) = win::pictures_dir().map(|d| d.join("Screenshots")) {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            }
            if self.doc.is_some() {
                ui.separator();
                if ui.button("Zoom to fit").clicked() {
                    self.zoom = None;
                }
                if ui.button("Actual size (100%)").clicked() {
                    self.zoom = Some(1.0);
                }
            }
        });
        if self.video.is_some() {
            divider(ui);
            if icon_button(ui, Icon::Save, None, false, true, "Save a copy (Ctrl+S)").clicked() {
                self.save_video_as(&ctx);
            }
        }
        if self.doc.is_some() {
            divider(ui);
            if icon_button(ui, Icon::Copy, None, false, true, "Copy with markup (Ctrl+C)").clicked() {
                self.copy(&ctx);
            }
            if icon_button(ui, Icon::Save, None, false, true, "Save as (Ctrl+S)").clicked() {
                self.save_as(&ctx);
            }
        }
    }

    fn markup_width(&self) -> f32 {
        let buttons = if self.crop.is_some() { 8.0 } else { 6.0 };
        let dividers = 2.0;
        buttons * BTN.x + dividers * DIVIDER_W + (buttons + dividers - 1.0) * GAP
    }

    fn markup_controls(&mut self, ui: &mut egui::Ui) {
        for (tool, icon, tip) in [
            (Tool::Pen, Icon::Pen, "Pen (click again for colour and size)"),
            (Tool::Highlighter, Icon::Highlighter, "Highlighter (click again for colour and size)"),
            (Tool::Eraser, Icon::Eraser, "Eraser: rubs out whole strokes"),
        ] {
            let ink = match tool {
                Tool::Pen => Some(self.pen.0),
                Tool::Highlighter => Some(self.highlighter.0),
                _ => None,
            };
            let resp = icon_button(ui, icon, ink, self.tool == tool, true, tip);
            let again = resp.clicked() && self.tool == tool;
            if resp.clicked() {
                self.tool = tool;
                self.crop = None;
            }
            if ink.is_some() {
                egui::Popup::from_response(&resp)
                    .open_memory(again.then_some(egui::SetOpenCommand::Toggle))
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| self.ink_options(ui, tool));
            }
        }
        divider(ui);
        if icon_button(ui, Icon::Crop, None, self.tool == Tool::Crop, true, "Crop: drag a box, then Enter").clicked() {
            self.tool = Tool::Crop;
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
        if let Some(doc) = &mut self.doc {
            if icon_button(ui, Icon::Undo, None, false, doc.can_undo(), "Undo (Ctrl+Z)").clicked() {
                doc.undo();
            }
            if icon_button(ui, Icon::Redo, None, false, doc.can_redo(), "Redo (Ctrl+Y)").clicked() {
                doc.redo();
            }
        }
    }

    fn ink_options(&mut self, ui: &mut egui::Ui, tool: Tool) {
        let ((color, width), palette, range) = match tool {
            Tool::Highlighter => (&mut self.highlighter, &HIGHLIGHT_COLORS[..], 6.0..=48.0),
            _ => (&mut self.pen, &PEN_COLORS[..], 1.0..=24.0),
        };
        ui.spacing_mut().item_spacing = vec2(6.0, 8.0);
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

    fn toast(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.note.as_ref().is_some_and(|(_, until)| now > *until) {
            self.note = None;
        }
        let Some((msg, _)) = &self.note else { return };
        egui::Area::new(egui::Id::unique("toast"))
            .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -20.0))
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(BUTTON_BG)
                    .stroke((1.0, OUTLINE))
                    .corner_radius(6)
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .show(ui, |ui| ui.label(RichText::new(msg.as_str()).color(ICON)));
            });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.add_space((ui.available_height() * 0.5 - 50.0).max(8.0));
            let title = match self.mode {
                Mode::Photo => "Snip anything on your screen",
                Mode::Video => "Record anything on your screen",
            };
            ui.label(RichText::new(title).size(20.0).strong());
            ui.add_space(6.0);
            let hint = match (self.mode, self.hotkey_rx.is_some()) {
                (Mode::Photo, true) => "Press Alt+Shift+S anywhere, or pick a shape and click New.",
                (Mode::Photo, false) => "Pick a shape and click New.",
                (Mode::Video, _) => "Click New, choose an area, then press Start to record.",
            };
            ui.label(RichText::new(hint).weak());
            if let Some(n) = &self.hotkey_note {
                ui.add_space(4.0);
                ui.label(RichText::new(n).small().weak());
            }
        });
    }

    fn canvas(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let Some(doc) = &mut self.doc else { return };
        let rev = (doc.image_rev, doc.ink_rev);
        if self.texture.as_ref().is_none_or(|c| c.rev != rev) {
            // ceiling: erase/undo re-flatten the whole snip (~150 ms for 20 long strokes on 4K); burn per-stroke tiles if erasing feels sticky.
            let shown = if doc.strokes.is_empty() { doc.image.clone() } else { Arc::new(doc.flatten()) };
            match &mut self.texture {
                Some(c) => {
                    c.tex.set(shown.clone(), TextureOptions::LINEAR);
                    if c.rev.0 != rev.0 {
                        c.live = None;
                    }
                    c.shown = shown;
                    c.rev = rev;
                }
                None => {
                    let tex = ctx.load_texture("snip", shown.clone(), TextureOptions::LINEAR);
                    self.texture = Some(CanvasTex { tex, shown, rev, live: None });
                }
            }
        }
        let Some(canvas) = &mut self.texture else { return };

        let ppp = ctx.pixels_per_point();
        let px_size = vec2(doc.size()[0] as f32, doc.size()[1] as f32);
        let natural = px_size / ppp;
        let view = ui.available_size();
        let fit = ((view - vec2(32.0, 32.0)) / natural).min_elem().clamp(0.02, 1.0);
        let wheel_zoom = if ui.rect_contains_pointer(ui.max_rect()) { ui.input(|i| i.zoom_delta()) } else { 1.0 };
        let zoom_delta = wheel_zoom * std::mem::replace(&mut self.key_zoom, 1.0);
        if zoom_delta != 1.0 {
            self.zoom = Some((self.zoom.unwrap_or(fit) * zoom_delta).clamp(0.05, 8.0));
        }
        let zoom = self.zoom.unwrap_or(fit);
        let shown = natural * zoom;

        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            let area = shown + vec2(32.0, 32.0);
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
            let swallowed = self.popup_guard.swallows(&ctx, resp.clicked());
            match self.tool {
                _ if swallowed => {}
                Tool::Pen | Tool::Highlighter => {
                    let (ink, (color, width)) = if self.tool == Tool::Pen {
                        (Ink::Pen, self.pen)
                    } else {
                        (Ink::Highlighter, self.highlighter)
                    };
                    if resp.hovered() {
                        ctx.set_cursor_icon(CursorIcon::Crosshair);
                    }
                    // Thickness is chosen in screen points, so ink looks the same at any zoom.
                    let width_px = width * k;
                    if resp.drag_started() || resp.clicked() {
                        if let Some(p) = origin {
                            self.drawing = Some(Stroke { ink, color, width: width_px, points: vec![to_px(p)] });
                        }
                    }
                    if let (Some(s), Some(p)) = (&mut self.drawing, pointer) {
                        let p = to_px(p);
                        if s.points.last().is_none_or(|l| l.distance(p) > 0.75 * k) {
                            s.points.push(p);
                        }
                    }
                    if resp.drag_stopped() || resp.clicked() {
                        if let Some(s) = self.drawing.take() {
                            canvas.commit(doc, s);
                        }
                    }
                    if let Some(s) = &self.drawing {
                        canvas.show_live(&ctx, painter, img_rect, s, doc.size());
                    }
                }
                Tool::Eraser => {
                    let radius_pt = 10.0;
                    if let Some(p) = resp.hover_pos().or(pointer) {
                        painter.circle_stroke(p, radius_pt, (1.5, Color32::WHITE));
                        painter.circle_stroke(p, radius_pt + 1.5, (1.0, Color32::BLACK));
                    }
                    if resp.drag_started() || resp.clicked() {
                        self.erase_marked = false;
                    }
                    if resp.dragged() || resp.clicked() {
                        if let Some(p) = pointer {
                            let (p, r) = (to_px(p), radius_pt * k);
                            if doc.hits(p, r) {
                                if !self.erase_marked {
                                    doc.checkpoint();
                                    self.erase_marked = true;
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
                            self.crop = Some((clamp(to_px(p)), clamp(to_px(p))));
                        }
                    }
                    if resp.dragged() {
                        if let (Some((_, b)), Some(p)) = (&mut self.crop, pointer) {
                            *b = clamp(to_px(p));
                        }
                    }
                    if let Some((a, b)) = self.crop {
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
        });
    }

    fn picker_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let video = self.mode == Mode::Video;
        let Stage::Picking(pk) = &mut self.stage else { return };
        let img = vec2(pk.grab.image.size[0] as f32, pk.grab.image.size[1] as f32);
        let k = img / rect.size();
        let to_px = |p: Pos2| ((p - rect.min) * k).to_pos2();
        let to_pt = |p: Pos2| rect.min + p.to_vec2() / k;

        // Windows may rescale the window after it lands on another monitor.
        let [x, y, w, h] = pk.target;
        win::fit_client(self.hwnd as HWND, x, y, w, h);
        let resp = ui.interact(rect, egui::Id::unique("picker"), Sense::click_and_drag());
        let painter = ui.painter();
        painter.image(pk.texture.id(), rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
        ctx.set_cursor_icon(CursorIcon::Crosshair);
        let pointer = resp.interact_pointer_pos().or(resp.hover_pos());
        let origin = ctx.input(|i| i.pointer.press_origin()).or(pointer);

        let mut result: Option<ColorImage> = None;
        // An area in image pixels: cropped for a photo, recorded for a video.
        let mut picked: Option<Rect> = None;
        let mut whole_monitor = false;
        let mut cancel = ctx.input(|i| i.key_pressed(Key::Escape));

        match self.shape {
            Shape_::Rectangle => {
                if resp.drag_started() {
                    pk.start = origin.map(to_px);
                }
                let sel = match (pk.start, pointer) {
                    (Some(a), Some(b)) if resp.dragged() || resp.drag_stopped() => {
                        Some(Rect::from_two_pos(a, to_px(b)).intersect(Rect::from_min_size(Pos2::ZERO, img)))
                    }
                    _ => None,
                };
                match sel {
                    Some(s) => {
                        let shown = Rect::from_min_max(to_pt(s.min), to_pt(s.max));
                        dim_outside(painter, rect, shown);
                        painter.rect_stroke(shown, 0.0, (1.0, Color32::WHITE), StrokeKind::Outside);
                        size_tag(painter, shown, s.size());
                        if resp.drag_stopped() {
                            pk.start = None;
                            if s.width() >= 2.0 && s.height() >= 2.0 {
                                picked = Some(s);
                            }
                        }
                    }
                    None => {
                        painter.rect_filled(rect, 0.0, DIM);
                    }
                }
            }
            Shape_::Window => {
                let hit = pointer.map(to_px).and_then(|p| {
                    pk.grab
                        .windows
                        .iter()
                        .map(|w| Rect::from_min_max(pos2(w[0] as f32, w[1] as f32), pos2(w[2] as f32, w[3] as f32)))
                        .find(|r| r.contains(p))
                        .map(|r| r.intersect(Rect::from_min_size(Pos2::ZERO, img)))
                });
                let target = hit.unwrap_or(Rect::from_min_size(Pos2::ZERO, img));
                let shown = Rect::from_min_max(to_pt(target.min), to_pt(target.max));
                dim_outside(painter, rect, shown);
                painter.rect_stroke(shown.shrink(1.0), 0.0, (3.0, ACCENT), StrokeKind::Inside);
                if resp.clicked() && target.width() >= 2.0 && target.height() >= 2.0 {
                    picked = Some(target);
                }
            }
            Shape_::Freeform => {
                painter.rect_filled(rect, 0.0, DIM);
                if resp.drag_started() {
                    pk.outline = origin.map(to_px).into_iter().collect();
                }
                if resp.dragged() {
                    if let Some(p) = pointer.map(to_px) {
                        if pk.outline.last().is_none_or(|l| l.distance(p) > 1.5) {
                            pk.outline.push(p);
                        }
                    }
                }
                if pk.outline.len() > 1 {
                    let pts: Vec<Pos2> = pk.outline.iter().map(|&p| to_pt(p)).collect();
                    painter.add(Shape::line(pts, (2.0, Color32::WHITE)));
                }
                if resp.drag_stopped() {
                    result = markup::cut_freeform(&pk.grab.image, &pk.outline);
                    pk.outline.clear();
                }
            }
            Shape_::FullScreen if video => whole_monitor = true,
            Shape_::FullScreen => {
                result = Some(pk.grab.image.clone());
            }
        }
        let origin = pk.grab.origin;
        let mut record_area = None;
        if let Some(r) = picked {
            if video {
                let (x, y) = (r.min.x.round() as i32, r.min.y.round() as i32);
                record_area = Some([origin[0] + x, origin[1] + y, r.width().round() as i32, r.height().round() as i32]);
            } else {
                result = Some(crop_px(&pk.grab.image, r));
            }
        }
        if whole_monitor {
            record_area = Some(win::monitor_at_cursor());
        }
        let home = pk.home;

        // The original's helper bar: photo/video, shape, close; at the top
        // of the monitor the mouse was on.
        let top = to_pt(pos2(home[0] as f32 + home[2] as f32 / 2.0, home[1] as f32));
        egui::Area::new(egui::Id::unique("picker_bar"))
            .pivot(Align2::CENTER_TOP)
            .fixed_pos(top + vec2(0.0, 14.0))
            .order(egui::Order::Foreground)
            .show(&ctx, |ui| {
                egui::Frame::new()
                    .fill(TOOLBAR_BG)
                    .stroke((1.0, OUTLINE))
                    .corner_radius(8)
                    .inner_margin(6)
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
                        ui.horizontal(|ui| {
                            self.mode_switch(ui);
                            ui.add_space(4.0);
                            self.shape_menu(ui);
                            divider(ui);
                            if icon_button(ui, Icon::Close, None, false, true, "Close (Esc)").clicked() {
                                cancel = true;
                            }
                        });
                    });
            });

        if cancel {
            self.cancel_pick(&ctx);
        } else if let Some(area) = record_area {
            self.open_rec_bar(&ctx, area);
        } else if let Some(image) = result {
            self.finish(&ctx, image);
        }
    }

    /// Turns the window into the recording bar for `area` (screen pixels).
    fn open_rec_bar(&mut self, ctx: &egui::Context, area: [i32; 4]) {
        let area = recorder::fit_area(area);
        if area[2] < 32 || area[3] < 32 {
            self.cancel_pick(ctx);
            self.say(ctx, "That area is too small to record".into());
            return;
        }
        self.stage = Stage::Recording(Box::new(RecBar { area, rec: None, saving: false, notes: Vec::new() }));
        win::exclude_from_capture(self.hwnd as HWND, true);
        for cmd in [
            ViewportCommand::Decorations(false),
            ViewportCommand::Resizable(false),
            ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop),
            ViewportCommand::Minimized(false),
            ViewportCommand::Visible(true),
            ViewportCommand::Focus,
        ] {
            ctx.send_viewport_cmd(cmd);
        }
        ctx.request_repaint();
    }

    fn start_recording(&mut self, ctx: &egui::Context) {
        let Stage::Recording(bar) = &mut self.stage else { return };
        let Some(dir) = win::videos_dir() else {
            self.say(ctx, "Couldn't find your Videos folder".into());
            return;
        };
        let settings = recorder::Settings {
            area: bar.area,
            system_sound: self.record_system,
            microphone: self.record_mic.then(|| self.mic_choice.clone()),
            path: dir.join("Screen Recordings").join(format!("Screen Recording {}.mp4", win::timestamp())),
        };
        let (tx, c) = (self.rec_tx.clone(), ctx.clone());
        bar.rec = Some(recorder::start(settings, move |event| {
            let _ = tx.send(event);
            c.request_repaint();
        }));
    }

    fn on_record_event(&mut self, ctx: &egui::Context, event: recorder::Event) {
        match event {
            recorder::Event::Started { notes } => {
                if let Stage::Recording(bar) = &mut self.stage {
                    bar.notes = notes;
                }
            }
            recorder::Event::Note(note) => {
                if let Stage::Recording(bar) = &mut self.stage {
                    bar.notes.push(note);
                }
            }
            recorder::Event::Done(result) => {
                let notes = match std::mem::replace(&mut self.stage, Stage::Idle) {
                    Stage::Recording(bar) => bar.notes,
                    _ => Vec::new(),
                };
                self.close_rec_bar(ctx);
                match result {
                    Ok(f) => {
                        let thumb = f.first_frame.map(|img| ctx.load_texture("recording", img, TextureOptions::LINEAR));
                        self.video = Some(VideoResult { path: f.path, seconds: f.seconds, size: f.size, thumb });
                        self.doc = None;
                        self.texture = None;
                        self.place_window(ctx, self.window_size_for(ctx));
                        let mut msg = match f.stopped_early {
                            Some(why) => format!("Recording stopped early ({why}) · Saved to Videos\\Screen Recordings"),
                            None => "Saved to Videos\\Screen Recordings".to_string(),
                        };
                        for n in notes {
                            msg.push_str(" · ");
                            msg.push_str(&n);
                        }
                        self.say(ctx, msg);
                    }
                    Err(e) => self.say(ctx, format!("Recording failed: {e}")),
                }
            }
        }
    }

    /// Back from the bar to the normal window.
    fn close_rec_bar(&mut self, ctx: &egui::Context) {
        self.stage = Stage::Idle;
        win::exclude_from_capture(self.hwnd as HWND, false);
        self.show_window(ctx);
        self.place_window(ctx, self.window_size_for(ctx));
    }

    fn rec_bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let hwnd = self.hwnd as HWND;
        let Stage::Recording(bar) = &mut self.stage else { return };

        // Sit just above the area, or inside its top edge when there's no room.
        let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
        let (bw, bh) = ((BAR.x * ppp).round() as i32, (BAR.y * ppp).round() as i32);
        let [x, y, w, h] = bar.area;
        let work = win::work_area_at(x + w / 2, y + h / 2);
        let (bx, by) = bar_position(bar.area, work, bw, bh);
        win::fit_client(hwnd, bx, by, bw, bh);
        area_outline(&ctx, bar.area);

        let recording = bar.rec.is_some();
        let elapsed = bar.rec.as_ref().map_or(0, |r| r.started.elapsed().as_secs());
        if recording {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let (mut start, mut stop, mut close) = (false, false, false);
        let saving = bar.saving;
        let notes = bar.notes.join("\n");
        if !recording && ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(TOOLBAR_BG).stroke((1.0, OUTLINE)).inner_margin(egui::Margin::symmetric(8, 8)))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
                ui.horizontal_centered(|ui| {
                    let (rect, resp) = ui.allocate_exact_size(vec2(104.0, BTN.y), Sense::click());
                    let p = ui.painter();
                    let fill = if resp.hovered() && !saving { ACCENT.gamma_multiply(0.9) } else { ACCENT };
                    p.rect_filled(rect, 4.0, if saving { ICON_OFF } else { fill });
                    let (icon, label) = match (recording, saving) {
                        (_, true) => (Icon::Stop, "Saving"),
                        (true, _) => (Icon::Stop, "Stop"),
                        _ => (Icon::Record, "Start"),
                    };
                    let dark = Color32::from_rgb(0x1a, 0x1a, 0x1a);
                    icons::paint(p, Rect::from_center_size(rect.left_center() + vec2(24.0, 0.0), vec2(18.0, 18.0)), icon, dark, None);
                    p.text(rect.left_center() + vec2(42.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(14.0), dark);
                    if resp.clicked() && !saving {
                        if recording { stop = true } else { start = true }
                    }

                    let clock = format!("{:02}:{:02}:{:02}", elapsed / 3600, elapsed / 60 % 60, elapsed % 60);
                    let (t, _) = ui.allocate_exact_size(vec2(84.0, BTN.y), Sense::hover());
                    ui.painter().text(t.center(), Align2::CENTER_CENTER, clock, FontId::monospace(14.0), ICON);

                    let mic_icon = if self.record_mic { Icon::Mic } else { Icon::MicOff };
                    let mic_tip = if self.record_mic { "Mute microphone" } else { "Unmute microphone" };
                    if icon_button(ui, mic_icon, None, false, !recording, mic_tip).clicked() {
                        self.record_mic = !self.record_mic;
                    }
                    let (chev, chev_resp) = ui.allocate_exact_size(vec2(16.0, BTN.y), if recording { Sense::hover() } else { Sense::click() });
                    icons::paint(ui.painter(), Rect::from_center_size(chev.center(), vec2(12.0, 12.0)), Icon::Chevron, if recording { ICON_OFF } else { ICON }, None);
                    if chev_resp.clicked() {
                        self.mics = audio::microphones();
                    }
                    egui::Popup::menu(&chev_resp).show(|ui| {
                        if ui.selectable_label(!self.record_mic, "Mute").clicked() {
                            self.record_mic = false;
                        }
                        ui.separator();
                        for (i, m) in self.mics.iter().enumerate() {
                            let chosen = self.record_mic
                                && match &self.mic_choice {
                                    Some(id) => *id == m.id,
                                    None => i == 0,
                                };
                            if ui.selectable_label(chosen, &m.name).clicked() {
                                self.record_mic = true;
                                self.mic_choice = Some(m.id.clone());
                            }
                        }
                        if self.mics.is_empty() {
                            ui.weak("No microphones found");
                        }
                    });

                    let sys_icon = if self.record_system { Icon::SystemSound } else { Icon::SystemSoundOff };
                    let sys_tip = if self.record_system { "Mute system audio" } else { "Unmute system audio" };
                    if icon_button(ui, sys_icon, None, false, !recording, sys_tip).clicked() {
                        self.record_system = !self.record_system;
                    }
                    divider(ui);
                    let close_tip = if recording { "Stop and save" } else { "Close (Esc)" };
                    if icon_button(ui, Icon::Close, None, false, !saving, close_tip).clicked() {
                        if recording { stop = true } else { close = true }
                    }
                    if !notes.is_empty() {
                        ui.label(RichText::new("!").strong().color(ACCENT)).on_hover_text(&notes);
                    }
                });
            });

        if start {
            self.start_recording(&ctx);
        }
        if let Stage::Recording(bar) = &mut self.stage {
            if stop {
                if let Some(rec) = &mut bar.rec {
                    rec.stop();
                    bar.saving = true;
                }
            }
        }
        if close {
            self.close_rec_bar(&ctx);
        }
    }

    fn video_view(&mut self, ui: &mut egui::Ui) {
        let Some(v) = &self.video else { return };
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
        let button = |ui: &mut egui::Ui, text: &str| {
            ui.add(egui::Button::new(text).fill(BUTTON_HOVER).min_size(vec2(100.0, 32.0))).clicked()
        };
        let (play, folder) = (button(&mut row, "Play"), button(&mut row, "Show in folder"));
        if resp.clicked() || play {
            if let Err(e) = win::open_path(&path) {
                self.say(ui.ctx(), format!("Couldn't open the video: {e}"));
            }
        }
        if folder {
            if let Err(e) = win::show_in_folder(&path) {
                self.say(ui.ctx(), format!("Couldn't open the folder: {e}"));
            }
        }
    }

    fn save_video_as(&mut self, ctx: &egui::Context) {
        let Some(v) = &self.video else { return };
        let start = win::videos_dir().map(|d| d.join("Screen Recordings"));
        let name = v.path.file_stem().map_or_else(|| "Screen Recording".into(), |n| n.to_string_lossy().into_owned());
        let Some(path) = win::ask_save_path(self.hwnd as HWND, start.as_deref(), &name, ("MP4 video", "mp4")) else {
            return;
        };
        let msg = match std::fs::copy(&v.path, &path) {
            Ok(_) => format!("Saved {}", path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into())),
            Err(e) => format!("Couldn't save: {e}"),
        };
        self.say(ctx, msg);
    }

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.screenshot else { return };
        ctx.request_repaint();
        job.frames += 1;
        if job.frames == 120 {
            let (tx, rx) = channel();
            job.rx = Some(rx);
            let c = ctx.clone();
            ctx.request_screenshot(move |img| {
                let _ = tx.send(img);
                c.request_repaint();
            });
        }
        let Some(img) = job.rx.as_ref().and_then(|rx| rx.try_recv().ok()) else { return };
        match win::save_png(&job.path, &img) {
            Ok(()) => ctx.send_viewport_cmd(ViewportCommand::Close),
            Err(e) => {
                eprintln!("screenshot failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// The snip as shown: the picture with committed ink burned in by the same
/// rasteriser that saves it, plus a layer for the stroke being drawn.
struct CanvasTex {
    tex: TextureHandle,
    shown: Arc<ColorImage>,
    /// `(image_rev, ink_rev)` of the doc that `shown` was built from.
    rev: (u64, u64),
    live: Option<LiveInk>,
}

/// Image-sized, see-through texture holding the stroke being drawn. Only the
/// pixels its newest segments touch are re-rasterised and uploaded each frame.
struct LiveInk {
    tex: TextureHandle,
    /// Points of the stroke already rasterised into `tex`.
    done: usize,
    /// Pixels written so far, cleared when the stroke ends.
    dirty: Option<PxRect>,
}

impl LiveInk {
    fn clear(&mut self) {
        if let Some(r) = self.dirty.take() {
            let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
            self.tex.set_partial(r.pos(), ColorImage::new([w, h], vec![Color32::TRANSPARENT; w * h]), TextureOptions::LINEAR);
        }
        self.done = 0;
    }
}

impl CanvasTex {
    /// Adds a finished stroke, burning just that stroke into the texture
    /// rather than re-flattening the whole snip.
    fn commit(&mut self, doc: &mut Doc, s: Stroke) {
        let current = self.rev == (doc.image_rev, doc.ink_rev);
        if current {
            if let Some(r) = s.bounds_from(0, doc.size()) {
                let shown = Arc::make_mut(&mut self.shown);
                markup::burn_stroke(shown, &s);
                let patch = markup::sub_image(shown, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
                self.tex.set_partial(r.pos(), patch, TextureOptions::LINEAR);
            }
        }
        doc.add_stroke(s);
        if current {
            self.rev.1 = doc.ink_rev;
        }
        if let Some(live) = &mut self.live {
            live.clear();
        }
    }

    fn show_live(&mut self, ctx: &egui::Context, painter: &egui::Painter, img_rect: Rect, s: &Stroke, size: [usize; 2]) {
        let live = self.live.get_or_insert_with(|| LiveInk {
            tex: ctx.load_texture("live ink", ColorImage::new(size, vec![Color32::TRANSPARENT; size[0] * size[1]]), TextureOptions::LINEAR),
            done: 0,
            dirty: None,
        });
        if s.points.len() < live.done {
            live.clear();
        }
        if live.done < s.points.len() {
            if let Some(r) = s.bounds_from(live.done, size) {
                live.tex.set_partial(r.pos(), s.ink_tile(r), TextureOptions::LINEAR);
                live.dirty = Some(live.dirty.map_or(r, |d| d.union(r)));
            }
            live.done = s.points.len();
        }
        painter.image(live.tex.id(), img_rect, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE);
    }
}

fn dim_outside(painter: &egui::Painter, all: Rect, keep: Rect) {
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

fn size_tag(painter: &egui::Painter, shown: Rect, px: Vec2) {
    let text = format!("{:.0} × {:.0}", px.x, px.y);
    let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
    let at = shown.left_bottom() + vec2(0.0, 6.0);
    let bg = Rect::from_min_size(at, galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(bg, 4.0, Color32::from_black_alpha(190));
    painter.galley(at + vec2(5.0, 2.0), galley, Color32::WHITE);
}

fn crop_px(img: &ColorImage, r: Rect) -> ColorImage {
    let x0 = (r.min.x.round().max(0.0) as usize).min(img.size[0] - 1);
    let y0 = (r.min.y.round().max(0.0) as usize).min(img.size[1] - 1);
    let x1 = (r.max.x.round() as usize).clamp(x0 + 1, img.size[0]);
    let y1 = (r.max.y.round() as usize).clamp(y0 + 1, img.size[1]);
    markup::sub_image(img, x0, y0, x1 - x0, y1 - y0)
}

fn window_hwnd(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    // Ctrl+Plus/Minus/0 zoom the snip (see `read_keys`), not egui's whole UI.
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

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let mut screenshot = None;
    while let Some(a) = args.next() {
        if a == "--screenshot" {
            match args.next() {
                Some(p) => screenshot = Some(PathBuf::from(p)),
                None => {
                    eprintln!("--screenshot needs a file name, e.g. --screenshot shot.png");
                    std::process::exit(2);
                }
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Snipscreen")
            .with_inner_size([620.0, 330.0])
            .with_min_inner_size([MIN_WINDOW_W, MIN_WINDOW_H]),
        ..Default::default()
    };
    eframe::run_native(
        "Snipscreen",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, screenshot)))),
    )
}

/// Toast for a new snip's automatic copy and save; `None` when each job that ran succeeded and
/// only the copy was asked for, since the clipboard needs no announcement.
fn auto_note(copied: Option<Result<(), String>>, saved: Option<Result<(), String>>) -> Option<String> {
    let copy_err = copied.as_ref().and_then(|r| r.as_ref().err());
    let save_err = saved.as_ref().and_then(|r| r.as_ref().err());
    let mut parts = Vec::new();
    match (&copied, &saved) {
        (Some(Ok(())), Some(Ok(()))) => parts.push("Copied, and saved to Pictures\\Screenshots".to_string()),
        (_, Some(Ok(()))) => parts.push("Saved to Pictures\\Screenshots".to_string()),
        (Some(Ok(())), Some(Err(_))) => parts.push("Copied to clipboard".to_string()),
        _ => {}
    }
    if let Some(e) = copy_err {
        parts.push(format!("Couldn't copy: {e}"));
    }
    if let Some(e) = save_err {
        parts.push(format!("Couldn't auto-save: {e}"));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// A popup that closes on a click outside lets that click through to whatever lies beneath it.
/// This notices when a canvas click only dismissed a popup, so it doesn't also draw.
#[derive(Default)]
struct PopupClickGuard {
    open_at_press: bool,
}

impl PopupClickGuard {
    /// Call once per frame before any popup is shown: the press that starts a click sees the popups as they were.
    fn begin_frame(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.pointer.any_pressed()) {
            self.open_at_press = egui::Popup::is_any_open(ctx);
        }
    }

    /// True for a click that was open at the press and is closed now. Drags leave the popup open, so they still draw.
    fn swallows(&self, ctx: &egui::Context, clicked: bool) -> bool {
        clicked && self.open_at_press && !egui::Popup::is_any_open(ctx)
    }
}

/// Left edge of the markup group: centred in the toolbar when it fits clear of the capture and
/// output groups, otherwise pushed right up against the capture group.
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

fn read_keys(i: &mut egui::InputState) -> Keys {
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
        enter: i.key_pressed(Key::Enter),
        esc: i.key_pressed(Key::Escape),
        zoom_in: zoom_in_plus | zoom_in_equals,
        zoom_out: i.consume_shortcut(&cmd(Key::Minus)),
        zoom_fit: i.consume_shortcut(&cmd(Key::Num0)),
    }
}

fn icon_button(ui: &mut egui::Ui, icon: Icon, ink: Option<Color32>, selected: bool, enabled: bool, tip: &str) -> egui::Response {
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
fn dropdown(ui: &mut egui::Ui, icon: Icon, badge: Option<u64>) -> egui::Response {
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

fn divider(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(DIVIDER_W, BTN.y), Sense::hover());
    ui.painter().line_segment([rect.center_top() + vec2(0.0, 7.0), rect.center_bottom() - vec2(0.0, 7.0)], (1.0, DIVIDER));
}

/// A see-through, click-through window with a dashed outline just outside
/// the area being recorded. It stays out of the recording itself.
fn area_outline(ctx: &egui::Context, [x, y, w, h]: [i32; 4]) {
    const PAD: i32 = 4;
    let id = egui::ViewportId::from_hash_of(FRAME_TITLE);
    let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
    let builder = egui::ViewportBuilder::default()
        .with_title(FRAME_TITLE)
        .with_decorations(false)
        .with_transparent(true)
        .with_mouse_passthrough(true)
        .with_always_on_top()
        .with_taskbar(false)
        .with_resizable(false)
        .with_active(false)
        .with_position(pos2((x - PAD) as f32, (y - PAD) as f32) / ppp)
        .with_inner_size(vec2((w + 2 * PAD) as f32, (h + 2 * PAD) as f32) / ppp);
    ctx.show_viewport_immediate(id, builder, |ui, _| {
        if let Some(hwnd) = win::find_window(FRAME_TITLE) {
            win::exclude_from_capture(hwnd, true);
            win::fit_client(hwnd, x - PAD, y - PAD, w + 2 * PAD, h + 2 * PAD);
        }
        let r = ui.max_rect().shrink(1.5);
        let p = ui.painter();
        let dash = |a: Pos2, b: Pos2| {
            p.add(Shape::dashed_line(&[a, b], (2.0, Color32::from_white_alpha(230)), 7.0, 5.0));
        };
        dash(r.left_top(), r.right_top());
        dash(r.right_top(), r.right_bottom());
        dash(r.right_bottom(), r.left_bottom());
        dash(r.left_bottom(), r.left_top());
    });
}

/// Top-left of the recording bar (`bw`x`bh` px) for a capture `area` on a monitor `work` area, both `[x, y, w, h]`.
/// Centred over the area, just above it or inside its top edge, and kept on the work area; never panics.
fn bar_position(area: [i32; 4], work: [i32; 4], bw: i32, bh: i32) -> (i32, i32) {
    let [x, y, w, _] = area;
    // `i32::clamp` panics when min > max, so a work area narrower than the bar aligns to its left edge instead.
    let bx = (x + w / 2 - bw / 2).clamp(work[0], (work[0] + work[2] - bw).max(work[0]));
    let by = if y - bh - 12 >= work[1] { y - bh - 12 } else { y + 12 };
    let by = by.clamp(work[1], (work[1] + work[3] - bh).max(work[1]));
    (bx, by)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys seen by `read_keys` in one frame after pressing `key` with `mods`.
    fn press(key: Key, mods: Modifiers) -> Keys {
        let ctx = egui::Context::default();
        let input = egui::RawInput {
            events: vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: mods }],
            ..Default::default()
        };
        let mut keys = Keys::default();
        let mut out = ctx.run_ui(input, |ui| keys = ui.ctx().input_mut(read_keys));
        out.textures_delta.clear();
        keys
    }

    #[test]
    fn markup_group_never_overlaps_its_neighbours() {
        // Group widths as laid out by `toolbar`: capture 298, output 133, markup with crop buttons 350.
        let (left_end, out_w, width) = (12.0 + 298.0, 133.0, 350.0);
        for window in [MIN_EDIT_W as i32, 900, 1200, 2000] {
            let row_end = window as f32 - 12.0;
            let right_start = row_end - out_w;
            let x = markup_x((12.0 + row_end) / 2.0, width, left_end, right_start);
            assert!(x >= left_end + GROUP_GAP, "overlaps capture group at {window}");
            assert!(x + width <= right_start - GROUP_GAP, "overlaps output group at {window}");
        }
    }

    #[test]
    fn markup_group_centres_when_room_and_hugs_the_capture_group_otherwise() {
        assert_eq!(markup_x(500.0, 200.0, 100.0, 900.0), 400.0);
        assert_eq!(markup_x(500.0, 300.0, 100.0, 600.0), 284.0, "pushed left by the output group");
        assert_eq!(markup_x(500.0, 800.0, 100.0, 1000.0), 116.0, "pulled right by the capture group");
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
    fn auto_note_reports_what_each_job_did() {
        let ok = || Some(Ok(()));
        let err = |m: &str| Some(Err(m.to_string()));
        assert_eq!(auto_note(ok(), ok()).as_deref(), Some("Copied, and saved to Pictures\\Screenshots"));
        assert_eq!(auto_note(None, ok()).as_deref(), Some("Saved to Pictures\\Screenshots"));
        assert_eq!(auto_note(ok(), None), None);
        assert_eq!(auto_note(None, None), None);
        assert_eq!(auto_note(err("busy"), ok()).as_deref(), Some("Saved to Pictures\\Screenshots · Couldn't copy: busy"));
        assert_eq!(auto_note(ok(), err("disk full")).as_deref(), Some("Copied to clipboard · Couldn't auto-save: disk full"));
        assert_eq!(auto_note(err("busy"), None).as_deref(), Some("Couldn't copy: busy"));
        assert_eq!(auto_note(None, err("disk full")).as_deref(), Some("Couldn't auto-save: disk full"));
        assert_eq!(auto_note(err("busy"), err("disk full")).as_deref(), Some("Couldn't copy: busy · Couldn't auto-save: disk full"));
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

    const WORK: [i32; 4] = [0, 0, 1920, 1080];

    #[test]
    fn bar_centres_above_the_area() {
        assert_eq!(bar_position([500, 400, 400, 300], WORK, 300, 60), (550, 328));
    }

    #[test]
    fn bar_clamps_to_left_and_right_edges() {
        assert_eq!(bar_position([0, 400, 100, 300], WORK, 300, 60).0, 0);
        assert_eq!(bar_position([1800, 400, 120, 300], WORK, 300, 60).0, 1620);
    }

    #[test]
    fn bar_wider_than_work_area_aligns_left_without_panicking() {
        assert_eq!(bar_position([100, 400, 500, 300], [50, 0, 200, 1080], 300, 60).0, 50);
    }

    #[test]
    fn bar_drops_inside_the_top_edge_when_there_is_no_room_above() {
        assert_eq!(bar_position([500, 20, 400, 300], WORK, 300, 60), (550, 32));
    }

    #[test]
    fn bar_stays_on_a_work_area_too_short_for_it() {
        // no room above, and y + 12 would put the bar's bottom below the work area
        assert_eq!(bar_position([0, 50, 400, 30], [0, 0, 1920, 100], 300, 60), (50, 40));
        // area at the top of a work area shorter than the bar: pinned to its top
        assert_eq!(bar_position([0, 0, 400, 30], [0, 0, 1920, 40], 300, 60), (50, 0));
    }
}
