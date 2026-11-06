#![windows_subsystem = "windows"]

mod audio;
mod editor;
mod icons;
mod markup;
mod notify;
mod recorder;
mod settings;
mod settings_window;
mod tray;
mod widgets;
mod win;

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use editor::{Editors, VideoResult};
use eframe::egui::{
    self, pos2, vec2, Align, Align2, Color32, ColorImage, CursorIcon, FontId, Key, KeyboardShortcut, Layout, Modifiers, Pos2, Rect, RichText, Sense, Shape,
    StrokeKind, TextureHandle, TextureOptions, Vec2, ViewportCommand, ViewportId, WindowLevel,
};
use icons::Icon;
use settings::{AfterCapture, Hotkey, Mode, Settings, Shape as Shape_, DELAYS};
use widgets::{
    dim_outside, divider, dropdown, icon_button, style, take_escape, Note, Notifier, PopupClickGuard, ACCENT, BTN, BUTTON_BG, BUTTON_HOVER, DIM, DIVIDER_W,
    GAP, GLYPH, ICON, ICON_OFF, MIN_WINDOW_H, MIN_WINDOW_W, OUTLINE, RADIUS, TOOLBAR_BG,
};
use windows_sys::Win32::Foundation::HWND;

/// The home window: a toolbar to start a capture over an empty canvas.
const HOME_SIZE: Vec2 = vec2(620.0, 330.0);
/// Time for the window's hide animation to finish before the screen is copied.
const HIDE_SETTLE: Duration = Duration::from_millis(250);

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

/// Whole seconds to show for a delay with `left` still to run, counting
/// 3, 2, 1; `None` once it has run out.
fn countdown_secs(left: Duration) -> Option<u64> {
    (!left.is_zero()).then(|| left.as_secs() + u64::from(left.subsec_nanos() > 0))
}

fn delay_label(s: u64) -> String {
    if s == 0 { "No delay".into() } else { format!("{s} seconds") }
}

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

enum Stage {
    Idle,
    /// Delay running with the window still visible, so it can show the countdown.
    Counting { end: Instant },
    /// Window hidden; the screen is about to be copied.
    Waiting,
    Picking(Box<Picker>),
    Recording(Box<RecBar>),
}

// Recording bar controls, left to right: Start, clock, mic, mic menu, system sound, divider, close, and the "!" warning (shown only when there are notes, but always budgeted for).
const REC_START_W: f32 = 104.0;
const REC_CLOCK_W: f32 = 84.0;
const REC_CHEVRON_W: f32 = 16.0;
const REC_WARN_W: f32 = 12.0;
const REC_ROW: [f32; 8] = [REC_START_W, REC_CLOCK_W, BTN.x, REC_CHEVRON_W, BTN.x, DIVIDER_W, BTN.x, REC_WARN_W];
const BAR_PAD: f32 = 8.0;
const BAR_BORDER: f32 = 1.0;
/// Spare room so rounding to whole pixels at fractional scales never clips a control.
const BAR_SLACK: f32 = 6.0;
const BAR: Vec2 = vec2(
    row_width(&REC_ROW) + 2.0 * (BAR_PAD + BAR_BORDER) + BAR_SLACK,
    BTN.y + 2.0 * (BAR_PAD + BAR_BORDER) + BAR_SLACK,
);

/// Width of `items` laid out in a row with `GAP` between them.
const fn row_width(items: &[f32]) -> f32 {
    let mut total = GAP * (items.len() - 1) as f32;
    let mut i = 0;
    while i < items.len() {
        total += items[i];
        i += 1;
    }
    total
}
/// Extra bar height while a message is showing under the controls.
const BAR_NOTE_H: f32 = 28.0;
const FRAME_TITLE: &str = "Snipscreen recording area";

/// A `--screenshot` run: render this window for a while, write it to `path`, exit.
struct ScreenshotJob {
    path: PathBuf,
    /// The home window, or the editor opened on the sample snip.
    target: ViewportId,
    frames: u32,
    rx: Option<Receiver<Arc<ColorImage>>>,
}

/// What to render headlessly instead of running normally.
enum Shot {
    Home(PathBuf),
    Editor(PathBuf),
    Settings(PathBuf),
}

/// The shell: the home window (which the picker and the recording bar take over while a capture
/// runs), the capture settings, the global hotkeys, and the open editor windows.
struct App {
    hwnd: isize,
    /// The home window's toolbar doubles as its title bar.
    bar: win::TitleBar,
    /// Everything the person chose, including the capture mode, shape, delay and sound choices.
    settings: Settings,
    /// `settings` as last handed to `saver`; any difference is saved at the end of the pass.
    saved: Settings,
    saver: settings::Saver,
    /// Keeps the sign-in entry in step with `settings.start_with_windows`. `None` in a screenshot run.
    startup: Option<StartupEntry>,
    /// Messages from background work that has no window of its own (settings saver, startup entry).
    bg: Notifier,
    bg_rx: Receiver<String>,
    stage: Stage,
    grab_tx: Sender<Result<win::Grab, String>>,
    grab_rx: Receiver<Result<win::Grab, String>>,
    /// The resident thread: global shortcuts and later launches. `None` in a screenshot run.
    agent: Option<tray::Agent>,
    agent_rx: Option<Receiver<tray::Event>>,
    /// The snip shortcut in force; `None` when it isn't registered (`hotkey_note` says why).
    hotkey: Option<Hotkey>,
    hotkey_note: Option<String>,
    /// Presses of the global Esc hotkey, which is registered only during a countdown.
    /// The global Esc fired since the countdown last looked.
    global_esc: bool,
    esc_armed: bool,
    /// Quit was asked for; the app closes once a running recording is saved.
    quitting: bool,
    /// The agent is running, so the app lives in the tray: closing the home window only hides it.
    resident: bool,
    /// Started out of sight (`--tray`): eframe shows the root window once its first frame is painted,
    /// so it stays cloaked until that show has been undone.
    hidden_start: HiddenStart,
    note: Option<Note>,
    /// Window corner before the picker took over the screen, in pixels.
    restore_corner: Option<[i32; 2]>,
    /// Frames left in which to put the returning window back on screen.
    settle: u8,
    /// Whether the home window belongs on screen when no capture is using it. Closing it while
    /// editors are open only hides it.
    home_visible: bool,
    popup_guard: PopupClickGuard,

    screenshot: Option<ScreenshotJob>,

    mics: Vec<audio::Mic>,
    rec_tx: Sender<recorder::Event>,
    rec_rx: Receiver<recorder::Event>,
    /// Snips announced by a notification, kept for an instant open when it is clicked.
    recent: notify::Recent,
    last_snip_id: u64,
    snip_tx: Sender<SnipEvent>,
    snip_rx: Receiver<SnipEvent>,
    /// Copy/save/notify workers still running; joined on exit so a snip is never half-written.
    workers: Vec<std::thread::JoinHandle<()>>,
    editors: Editors,
    settings_window: Option<settings_window::SettingsWindow>,
    icon: Option<Arc<egui::IconData>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, shot: Option<Shot>, icon: Option<Arc<egui::IconData>>, loaded: settings::Loaded, boot: Option<Boot>) -> Self {
        style(&cc.egui_ctx);
        let hwnd = window_hwnd(cc);
        let (grab_tx, grab_rx) = channel();
        let (bg_tx, bg_rx) = channel();
        let shot_run = shot.is_some();
        // A screenshot run renders the defaults and never writes the person's file.
        let save_to = if shot_run { None } else { loaded.path.clone() };
        let bg = Notifier::new(bg_tx, cc.egui_ctx.clone(), ViewportId::ROOT);
        let report = bg.clone();
        let saver = settings::Saver::new(save_to, move |e| report.send(e));
        let settings = if shot_run { Settings::default() } else { loaded.settings };
        if !loaded.existed {
            saver.save(&settings);
        }
        let saved = settings.clone();
        let (rec_tx, rec_rx) = channel();
        let (snip_tx, snip_rx) = channel();
        let hidden = boot.as_ref().is_some_and(|b| b.starts_hidden());
        if hidden {
            win::cloak(hwnd as HWND, true);
        }
        let (agent, agent_rx, hotkey, hotkey_note, intent) = match boot {
            Some(b) => {
                // Events sent before this point are waiting in the channel and handled on the first pass.
                let _ = b.waker.set(cc.egui_ctx.clone());
                let (hotkey, note) = match b.hotkey {
                    Ok(h) => (Some(h), None),
                    Err(e) => (None, Some(e)),
                };
                (b.agent, Some(b.events), hotkey, note, Some(b.intent))
            }
            None => (None, None, None, None, None),
        };
        let bar = win::TitleBar::default();
        let wake = cc.egui_ctx.clone();
        let bar_failed = bar.attach(hwnd as HWND, move || wake.request_repaint_of(ViewportId::ROOT)).err();
        let mut app = Self {
            hwnd,
            bar,
            settings,
            saved,
            saver,
            startup: None,
            bg,
            bg_rx,
            stage: Stage::Idle,
            grab_tx,
            grab_rx,
            agent,
            agent_rx,
            hotkey,
            hotkey_note,
            global_esc: false,
            esc_armed: false,
            quitting: false,
            resident: false,
            hidden_start: if hidden { HiddenStart::FirstFrame } else { HiddenStart::Done },
            note: None,
            restore_corner: None,
            settle: 0,
            home_visible: !hidden,
            popup_guard: PopupClickGuard::default(),
            screenshot: None,
            mics: Vec::new(),
            rec_tx,
            rec_rx,
            recent: notify::Recent::new(RECENT_SNIPS, RECENT_BYTES),
            last_snip_id: if shot_run { 0 } else { notify::recent_dir().map_or(0, |d| notify::newest_id(&d)) },
            snip_tx,
            snip_rx,
            workers: Vec::new(),
            editors: Editors::new(icon.clone()),
            settings_window: None,
            icon,
        };
        if let Some(e) = bar_failed {
            app.say(&cc.egui_ctx, e);
        }
        if !loaded.problems.is_empty() && !shot_run {
            app.say(&cc.egui_ctx, format!("Some settings couldn't be read and were reset: {}", loaded.problems.join(", ")));
        }
        app.resident = app.agent.is_some();
        if !shot_run {
            // Every start: the exe may have moved since the entry was written.
            let startup = StartupEntry::new(app.bg.clone(), tray::sync_run_at_startup);
            startup.set(app.settings.start_with_windows);
            app.startup = Some(startup);
            app.register_notifications();
        }
        if let Some(intent) = intent {
            app.on_intent(&cc.egui_ctx, intent);
        }
        app.screenshot = shot.map(|shot| match shot {
            Shot::Home(path) => ScreenshotJob { path, target: ViewportId::ROOT, frames: 0, rx: None },
            Shot::Settings(path) => {
                app.open_settings(&cc.egui_ctx);
                let target = app.settings_window.as_ref().map_or(ViewportId::ROOT, |w| w.id());
                ScreenshotJob { path, target, frames: 0, rx: None }
            }
            Shot::Editor(path) => {
                let target = app.open_sample_editor(&cc.egui_ctx);
                ScreenshotJob { path, target, frames: 0, rx: None }
            }
        });
        app
    }

    fn start_snip(&mut self, ctx: &egui::Context) {
        if !matches!(self.stage, Stage::Idle | Stage::Counting { .. }) {
            return;
        }
        let delay = if self.settings.mode == Mode::Photo { self.settings.delay } else { 0 };
        if delay == 0 {
            self.hide_and_grab(ctx);
            return;
        }
        // Pressing New / Ctrl+N / the hotkey during the countdown starts it over.
        self.stage = Stage::Counting { end: Instant::now() + Duration::from_secs(delay) };
        // The countdown shows in the home window, even when it had been closed.
        if !self.home_visible {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
        }
        // On top and un-minimized without taking focus, so a hotkey snip from
        // another app still shows the countdown without disturbing what is on screen.
        win::restore_inactive(self.hwnd as HWND);
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
        ctx.request_repaint();
    }

    /// Esc is a global hotkey only while a countdown runs: a hotkey-started
    /// countdown never takes focus, so the window cannot see Esc itself. Called
    /// on every stage exit and once per frame, so it cannot stay registered.
    fn sync_escape_hotkey(&mut self) {
        let want = wants_escape_hotkey(&self.stage);
        if want != self.esc_armed {
            self.esc_armed = want;
            if let Some(agent) = &self.agent {
                agent.set_escape(want);
            }
        }
    }

    /// Hides the window, waits for its hide animation, then copies the screen.
    fn hide_and_grab(&mut self, ctx: &egui::Context) {
        self.restore_corner = win::window_corner(self.hwnd as HWND);
        // A snip from the tray has no window to wait for.
        let settle = if win::is_visible(self.hwnd as HWND) { HIDE_SETTLE } else { Duration::ZERO };
        ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        self.stage = Stage::Waiting;
        self.sync_escape_hotkey();
        let (tx, ctx) = (self.grab_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            std::thread::sleep(settle);
            let _ = tx.send(win::grab_screen());
            wake_root(&ctx);
        });
    }

    /// Advances the delay countdown: Esc cancels, the last second hides the window.
    fn tick_countdown(&mut self, ctx: &egui::Context) {
        // Always taken, so a press that arrives just as the countdown ends is not kept for the next one.
        let global_esc = std::mem::take(&mut self.global_esc);
        let Stage::Counting { end } = self.stage else { return };
        if take_escape(ctx) || global_esc {
            self.cancel_countdown(ctx);
            return;
        }
        let left = end.saturating_duration_since(Instant::now());
        match countdown_secs(left) {
            None => self.hide_and_grab(ctx),
            // Wake when the displayed number is due to change.
            Some(s) => ctx.request_repaint_after(left - Duration::from_secs(s - 1) + Duration::from_millis(1)),
        }
    }

    fn cancel_countdown(&mut self, ctx: &egui::Context) {
        self.stage = Stage::Idle;
        self.sync_escape_hotkey();
        self.return_home(ctx);
    }

    fn on_grab(&mut self, ctx: &egui::Context, grab: Result<win::Grab, String>) {
        let grab = match grab {
            Ok(g) => g,
            Err(e) => {
                self.stage = Stage::Idle;
                self.return_home(ctx);
                self.say(ctx, format!("Couldn't take the snip: {e}"));
                return;
            }
        };
        if self.settings.shape == Shape_::FullScreen {
            match self.settings.mode {
                Mode::Photo => self.finish(ctx, grab.image),
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
        self.stage = Stage::Picking(Box::new(Picker { home, grab, texture, start: None, outline: Vec::new(), target, shown: false, tries: 0 }));
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

    /// Back from the countdown, the picker or the recording bar to the home window, or out of
    /// sight again if the person had closed it.
    fn return_home(&mut self, ctx: &egui::Context) {
        for cmd in [ViewportCommand::WindowLevel(WindowLevel::Normal), ViewportCommand::Decorations(true), ViewportCommand::Resizable(true)] {
            ctx.send_viewport_cmd(cmd);
        }
        if self.home_visible {
            for cmd in [ViewportCommand::Minimized(false), ViewportCommand::Visible(true), ViewportCommand::Focus] {
                ctx.send_viewport_cmd(cmd);
            }
        } else {
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    /// The home window's close button. In the tray, or with editors open, it only hides the home
    /// window; otherwise the app ends when the last window is gone (see `close_when_no_window_is_left`).
    fn on_home_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) || self.quitting || self.screenshot.is_some() || (!self.resident && self.editors.is_empty()) {
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::CancelClose);
        match self.stage {
            Stage::Idle => {
                self.home_visible = false;
                ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            }
            Stage::Counting { .. } => {
                self.home_visible = false;
                self.cancel_countdown(ctx);
            }
            // The picker and the recording bar have no close button; they end on their own first.
            Stage::Waiting | Stage::Picking(_) | Stage::Recording(_) => {}
        }
    }

    fn close_when_no_window_is_left(&self, ctx: &egui::Context) {
        if no_window_left(self.resident, self.home_visible, self.editors.is_empty(), matches!(self.stage, Stage::Idle)) {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    /// Puts the home window back to its size; `settle_window` then returns it to its old corner.
    fn place_window(&mut self, ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(self.bar.inner_size_for(HOME_SIZE, ctx.pixels_per_point())));
        self.settle = 8;
    }

    /// Runs for a few frames after the picker: back to the old corner, then
    /// kept inside the monitor once Windows has finished any rescaling.
    fn settle_window(&mut self, ctx: &egui::Context) {
        if self.settle == 0 || !matches!(self.stage, Stage::Idle) {
            return;
        }
        self.settle -= 1;
        // Normally a no-op: corrects the size if Windows rescaled the window on its way back.
        let px = HOME_SIZE * ctx.pixels_per_point();
        win::size_client(self.hwnd as HWND, px.x.round() as i32, px.y.round() as i32);
        win::place_on_screen(self.hwnd as HWND, self.restore_corner.take());
        ctx.request_repaint();
    }

    fn cancel_pick(&mut self, ctx: &egui::Context) {
        self.stage = Stage::Idle;
        self.return_home(ctx);
        self.place_window(ctx);
    }

    fn finish(&mut self, ctx: &egui::Context, image: ColorImage) {
        self.stage = Stage::Idle;
        self.return_home(ctx);
        self.place_window(ctx);
        self.on_captured(ctx, image);
    }

    /// Every new snip arrives here: it is copied and saved as the settings ask, then announced by a
    /// notification or opened in an editor.
    fn on_captured(&mut self, ctx: &egui::Context, image: ColorImage) {
        match self.settings.after_capture {
            AfterCapture::Notify => self.notify_snip(ctx, image),
            AfterCapture::Editor => {
                let (_, report, image) = self.open_editor(ctx, image);
                self.auto_copy_save(image, report);
            }
        }
    }

    /// Runs `job` on a thread that `on_exit` waits for.
    fn spawn_worker(&mut self, job: impl FnOnce() + Send + 'static) {
        self.workers.retain(|w| !w.is_finished());
        self.workers.push(std::thread::spawn(job));
    }

    /// Where an auto-saved snip goes, when auto-save is on.
    fn save_path(&self) -> Option<Result<PathBuf, String>> {
        self.settings.auto_save.then(|| {
            win::pictures_dir()
                .map(|dir| dir.join("Screenshots").join(format!("Snip {}.png", win::timestamp())))
                .ok_or_else(|| "couldn't find your Pictures folder".to_string())
        })
    }

    /// Copies and saves the snip as asked, then shows a Windows notification with its preview; a click
    /// on it opens the snip (`open_link`). All on a worker. If the notification can't be shown the snip
    /// opens in an editor instead, saying why.
    fn notify_snip(&mut self, ctx: &egui::Context, image: ColorImage) {
        let id = next_snip_id(self.last_snip_id, std::time::SystemTime::now());
        self.last_snip_id = id;
        let image = Arc::new(image);
        self.recent.insert(id, image.clone());
        let (save_to, copy, hwnd) = (self.save_path(), self.settings.auto_copy, self.hwnd);
        let (tx, ctx) = (self.snip_tx.clone(), ctx.clone());
        self.spawn_worker(move || {
            if let Err(msg) = deliver_by_notification(id, &image, hwnd, copy, save_to) {
                let _ = tx.send(SnipEvent::NotShown { id, msg });
                wake_root(&ctx);
            }
        });
    }

    /// A click on a snip's notification: the snip opens in a new editor, from memory or its file.
    fn open_link(&mut self, ctx: &egui::Context, link: &str) {
        let Some((id, path)) = notify::parse_open_link(link) else {
            self.show_home(ctx);
            self.say(ctx, "That link doesn't name a snip".into());
            return;
        };
        if let Some(image) = self.recent.take(id) {
            self.open_editor(ctx, Arc::unwrap_or_clone(image));
            return;
        }
        let folders: Vec<PathBuf> = [win::pictures_dir().map(|d| d.join("Screenshots")), notify::recent_dir()].into_iter().flatten().collect();
        if !notify::may_open(&path, &folders) {
            self.show_home(ctx);
            self.say(ctx, format!("SnipScreen only opens its own snips, not {}", path.display()));
            return;
        }
        let (tx, ctx) = (self.snip_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(SnipEvent::Loaded(notify::load_png(&path)));
            wake_root(&ctx);
        });
    }

    fn on_snip_event(&mut self, ctx: &egui::Context, event: SnipEvent) {
        match event {
            SnipEvent::Loaded(Ok(image)) => {
                self.open_editor(ctx, image);
            }
            SnipEvent::Loaded(Err(e)) => {
                self.show_home(ctx);
                self.say(ctx, format!("Couldn't open the snip: {e}"));
            }
            SnipEvent::NotShown { id, msg } => match self.recent.take(id) {
                Some(image) => {
                    let (_, report, _) = self.open_editor(ctx, Arc::unwrap_or_clone(image));
                    report.send(msg);
                }
                None => self.say(ctx, msg),
            },
        }
    }

    /// Opens `image` in a new editor window near where the home window sits. Returns the window,
    /// its toast, and the picture as the editor holds it.
    fn open_editor(&mut self, ctx: &egui::Context, image: ColorImage) -> (ViewportId, Notifier, Arc<ColorImage>) {
        self.editors.open_snip(ctx, image, self.hwnd, self.restore_corner)
    }

    /// Copies and/or saves a new snip on a worker, reporting through `report` what each job really did.
    fn auto_copy_save(&mut self, image: Arc<ColorImage>, report: Notifier) {
        if !self.settings.auto_copy && !self.settings.auto_save {
            return;
        }
        let (save_to, copy, hwnd) = (self.save_path(), self.settings.auto_copy, self.hwnd);
        self.spawn_worker(move || {
            let Delivered { copied, saved, .. } = copy_and_save(hwnd, &image, copy, save_to);
            if let Some(msg) = auto_note(copied, saved.map(|r| r.map(|_| ()))) {
                report.send(msg);
            }
        });
    }

    /// The New buttons' tooltip, naming the shortcut only while it is registered.
    fn new_tip(&self) -> String {
        new_tooltip(self.settings.mode, self.hotkey.map(Hotkey::label).as_deref(), self.hotkey_note.as_deref())
    }

    /// A launch of the program: this one, or a later one forwarded by `tray::forward`.
    fn on_intent(&mut self, ctx: &egui::Context, intent: tray::Intent) {
        match intent {
            tray::Intent::Show => self.show_home(ctx),
            tray::Intent::Open(link) => self.open_link(ctx, &link),
            tray::Intent::Tray => {}
            tray::Intent::Quit => self.quit(ctx),
        }
    }

    /// Brings the home window up, or, mid-capture, has it come back when the capture ends.
    fn show_home(&mut self, ctx: &egui::Context) {
        self.home_visible = true;
        if matches!(self.stage, Stage::Idle | Stage::Counting { .. }) {
            self.return_home(ctx);
        }
    }

    /// Ends the app. A recording in progress is stopped and saved first; editors close with it,
    /// their snips already copied and saved as the settings asked.
    fn quit(&mut self, ctx: &egui::Context) {
        self.quitting = true;
        if let Stage::Recording(bar) = &mut self.stage {
            if let Some(rec) = &mut bar.rec {
                rec.stop();
                bar.saving = true;
                return;
            }
        }
        ctx.send_viewport_cmd(ViewportCommand::Close);
    }

    /// Opens the Settings window, or brings it forward if it is open.
    fn open_settings(&mut self, ctx: &egui::Context) {
        match &self.settings_window {
            Some(w) => w.focus(ctx),
            None => self.settings_window = Some(settings_window::SettingsWindow::new(ctx, &self.settings, self.hotkey, self.hotkey_note.as_deref(), self.icon.clone())),
        }
    }

    /// Takes what the Settings window changed: the after-snip and startup choices apply at once, the
    /// recorder is started or stopped on the agent.
    fn poll_settings_window(&mut self, ctx: &egui::Context) {
        let Some(w) = &self.settings_window else { return };
        let req = w.poll();
        if let Some(s) = req.settings {
            (self.settings.after_capture, self.settings.auto_copy, self.settings.auto_save) = (s.after_capture, s.auto_copy, s.auto_save);
            self.settings.start_with_windows = s.start_with_windows;
        }
        match (req.record, &self.agent) {
            (Some(on), Some(agent)) => agent.record(on),
            (Some(true), None) => w.recorded(ctx, Some(Err("Global shortcuts are off, so there is nothing to change".into()))),
            _ => {}
        }
        if req.closed {
            self.settings_window = None;
        }
    }

    /// The recorder caught a combination: a usable one goes to the agent to register.
    fn on_recorded(&mut self, ctx: &egui::Context, caught: Option<Hotkey>) {
        let verdict = caught.map(Hotkey::check);
        if let (Some(Ok(h)), Some(agent)) = (&verdict, &self.agent) {
            agent.set_hotkey(*h);
        }
        if let Some(w) = &self.settings_window {
            w.recorded(ctx, verdict);
        }
    }

    /// The agent registered a new shortcut, or refused it and kept the old one.
    fn on_hotkey_set(&mut self, ctx: &egui::Context, result: Result<Hotkey, String>) {
        if let Ok(h) = result {
            (self.hotkey, self.hotkey_note, self.settings.shortcut) = (Some(h), None, h);
        }
        if let Some(w) = &self.settings_window {
            w.hotkey_result(ctx, &result, self.hotkey);
        }
    }

    /// Registers SnipScreen for notifications and its `snipscreen:` links, off the UI thread.
    fn register_notifications(&self) {
        let report = self.bg.clone();
        std::thread::spawn(move || {
            if let Err(e) = win::exe_path().and_then(|exe| notify::register(&exe)) {
                report.send(format!("Notifications may not work: {e}"));
            }
        });
    }

    /// Hands changed settings to the saver; a few field compares per pass when nothing changed.
    fn persist(&mut self) {
        if self.settings != self.saved {
            if let (true, Some(startup)) = (self.settings.start_with_windows != self.saved.start_with_windows, &self.startup) {
                startup.set(self.settings.start_with_windows);
            }
            self.saved = self.settings.clone();
            self.saver.save(&self.settings);
        }
    }

    /// Shows `msg` in the home window (or the recording bar), or in the newest editor while the home window is closed.
    fn say(&mut self, ctx: &egui::Context, msg: String) {
        if !note_goes_home(self.home_visible, matches!(self.stage, Stage::Recording(_))) {
            if let Some(editor) = self.editors.newest_notifier(ctx) {
                editor.send(msg);
                return;
            }
        }
        self.note = Some(Note::new(msg));
        ctx.request_repaint();
    }

    /// The message to draw right now, if any; the first draw starts its clock.
    fn live_note(&mut self, ctx: &egui::Context) -> Option<String> {
        widgets::live_note(&mut self.note, ctx)
    }

    /// The sample snip behind `--screenshot-editor`, with a pen and a highlighter stroke on it.
    fn open_sample_editor(&mut self, ctx: &egui::Context) -> ViewportId {
        let (id, _, _) = self.open_editor(ctx, sample_image(960, 600));
        self.editors.with(id, |e| {
            if let editor::Content::Snip(snip) = &mut e.content {
                for s in sample_strokes() {
                    snip.doc.add_mark(s);
                }
            }
        });
        id
    }
}

impl eframe::App for App {
    /// Lets go of the shortcuts and writes pending settings before the process ends.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
        self.persist();
        self.saver = settings::Saver::new(None, |_| {});
        self.startup = None;
        self.agent = None;
    }

    /// Transparent so the recording-area outline window shows only its dashes.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }

    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.hidden_start == HiddenStart::Painted {
            win::cloak(self.hwnd as HWND, false);
            self.hidden_start = HiddenStart::Done;
        }
        self.on_home_close(ctx);
        while let Ok(msg) = self.bg_rx.try_recv() {
            self.say(ctx, msg);
        }
        while let Ok(event) = self.rec_rx.try_recv() {
            self.on_record_event(ctx, event);
        }
        let mut hotkey = false;
        while let Some(event) = self.agent_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            match event {
                tray::Event::Snip => hotkey = true,
                tray::Event::Escape => self.global_esc = true,
                tray::Event::Intent(intent) => self.on_intent(ctx, intent),
                tray::Event::HotkeySet(result) => self.on_hotkey_set(ctx, result),
                tray::Event::Recorded(caught) => self.on_recorded(ctx, caught),
                tray::Event::Settings => self.open_settings(ctx),
            }
        }
        self.poll_settings_window(ctx);
        while let Ok(event) = self.snip_rx.try_recv() {
            self.on_snip_event(ctx, event);
        }
        if self.editors.poll().new_snip || hotkey {
            self.start_snip(ctx);
        }
        if let Ok(grab) = self.grab_rx.try_recv() {
            self.on_grab(ctx, grab);
        }
        self.tick_countdown(ctx);
        self.reveal_picker(ctx);
        self.settle_window(ctx);
        self.sync_escape_hotkey();
        self.editors.set_new_tip(self.new_tip());
        self.close_when_no_window_is_left(ctx);
        self.persist();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.hidden_start == HiddenStart::FirstFrame {
            // Processed right after eframe's first-frame show, which the cloak keeps off the screen
            // (and `with_active(false)` keeps from taking focus).
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.hidden_start = HiddenStart::Painted;
            ctx.request_repaint();
        }
        if matches!(self.stage, Stage::Picking(_)) {
            self.picker_ui(ui);
        } else if matches!(self.stage, Stage::Recording(_)) {
            self.rec_bar_ui(ui);
        } else {
            self.home_ui(ui);
        }
        // Every pass, or egui closes the editor and settings windows.
        self.editors.show(&ctx);
        if let Some(w) = &self.settings_window {
            w.show(&ctx);
        }
        self.drive_screenshot(&ctx);
        self.persist();
        // After this pass's stage changes, before eframe applies its window commands: the picker and
        // the recording bar are frameless, and winit lays those out.
        self.bar.set_active(!matches!(self.stage, Stage::Picking(_) | Stage::Recording(_)));
    }
}

impl App {
    fn home_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::N))) {
            self.start_snip(&ctx);
        }
        let bar = self.bar.clone();
        widgets::title_bar(ui, "home_toolbar", &bar, |ui, row| {
            let layout = |l| egui::UiBuilder::new().max_rect(row).layout(l);
            let mut left = ui.new_child(layout(Layout::left_to_right(Align::Center)));
            self.capture_controls(&mut left);
            let mut right = ui.new_child(layout(Layout::right_to_left(Align::Center)));
            self.home_menu(&mut right);
            vec![left.min_rect(), right.min_rect()]
        });
        egui::CentralPanel::default().frame(egui::Frame::new().fill(ui.style().visuals.extreme_bg_color)).show(ui, |ui| {
            if let Stage::Counting { end } = self.stage {
                self.countdown_view(ui, end);
            } else {
                self.empty_state(ui);
            }
        });
        // The window is hidden while waiting; a message shown now would be gone before anyone saw it.
        if !matches!(self.stage, Stage::Waiting) {
            if let Some(msg) = self.live_note(&ctx) {
                widgets::toast(&ctx, &msg, None);
            }
        }
    }

    fn home_menu(&mut self, ui: &mut egui::Ui) {
        let more = icon_button(ui, Icon::More, None, false, true, "More");
        egui::Popup::menu(&more).show(|ui| {
            if ui.button("Settings…").clicked() {
                self.open_settings(ui.ctx());
            }
            if ui.button("Open the Screenshots folder").clicked() {
                if let Some(dir) = win::pictures_dir().map(|d| d.join("Screenshots")) {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::process::Command::new("explorer").arg(dir).spawn();
                }
            }
        });
    }

    fn capture_controls(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let tip = self.new_tip();
        if widgets::new_button(ui, &tip).clicked() {
            self.start_snip(&ctx);
        }
        ui.add_space(8.0);
        self.mode_switch(ui);
        ui.add_space(4.0);
        self.shape_menu(ui);
        if self.settings.mode == Mode::Video {
            return;
        }

        let (timer, badge) = if self.settings.delay == 0 { (Icon::TimerOff, None) } else { (Icon::Timer, Some(self.settings.delay)) };
        let resp = dropdown(ui, timer, badge).on_hover_text(format!("Delay: {}", delay_label(self.settings.delay)));
        egui::Popup::menu(&resp).show(|ui| {
            for d in DELAYS {
                ui.selectable_value(&mut self.settings.delay, d, delay_label(d));
            }
        });
    }

    fn mode_switch(&mut self, ui: &mut egui::Ui) {
        const INSET: f32 = 2.0;
        let (group, _) = ui.allocate_exact_size(vec2(BTN.x * 2.0 + 2.0 * INSET, BTN.y), Sense::hover());
        // Concentric corners: the cells' radius plus the inset between them and the outline.
        ui.painter().rect_stroke(group, RADIUS as f32 + INSET, (1.0, OUTLINE), StrokeKind::Inside);
        for (i, (mode, icon, tip)) in
            [(Mode::Photo, Icon::Camera, "Snip a picture"), (Mode::Video, Icon::Video, "Record a video")]
                .into_iter()
                .enumerate()
        {
            let cell = Rect::from_min_size(group.min + vec2(INSET + i as f32 * BTN.x, INSET), BTN - vec2(0.0, 2.0 * INSET));
            let resp = ui.interact(cell, ui.scope_id().with(("mode", i)), Sense::click()).on_hover_text(tip);
            let p = ui.painter();
            if self.settings.mode == mode {
                p.rect_filled(cell, RADIUS, BUTTON_HOVER);
                let bar = Rect::from_center_size(cell.center_bottom() - vec2(0.0, 2.5), vec2(12.0, 3.0));
                p.rect_filled(bar, 1.5, ACCENT);
            } else if resp.hovered() {
                p.rect_filled(cell, RADIUS, BUTTON_BG);
            }
            icons::paint(p, Rect::from_center_size(cell.center() - vec2(0.0, 1.0), Vec2::splat(GLYPH)), icon, ICON, None);
            if resp.clicked() {
                self.set_mode(mode);
            }
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.settings.mode = mode;
        if !Shape_::choices(mode).contains(&self.settings.shape) {
            self.settings.shape = Shape_::Rectangle;
        }
    }

    fn shape_menu(&mut self, ui: &mut egui::Ui) {
        let resp = dropdown(ui, self.settings.shape.icon(), None).on_hover_text(format!("Shape: {}", self.settings.shape.label()));
        egui::Popup::menu(&resp).show(|ui| {
            for &s in Shape_::choices(self.settings.mode) {
                let (rect, item) = ui.allocate_exact_size(vec2(150.0, 32.0), Sense::click());
                let p = ui.painter();
                if item.hovered() {
                    p.rect_filled(rect, RADIUS, BUTTON_HOVER);
                }
                if self.settings.shape == s {
                    let pill = Rect::from_center_size(rect.left_center() + vec2(3.0, 0.0), vec2(3.0, 16.0));
                    p.rect_filled(pill, 1.5, ACCENT);
                }
                icons::paint(p, Rect::from_center_size(rect.left_center() + vec2(22.0, 0.0), Vec2::splat(GLYPH)), s.icon(), ICON, None);
                p.text(rect.left_center() + vec2(42.0, 0.0), Align2::LEFT_CENTER, s.label(), FontId::proportional(14.0), ICON);
                if item.clicked() {
                    self.settings.shape = s;
                    ui.close();
                }
            }
        });
    }

    fn countdown_view(&mut self, ui: &mut egui::Ui, end: Instant) {
        let secs = countdown_secs(end.saturating_duration_since(Instant::now())).unwrap_or(0);
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.add_space((ui.available_height() * 0.5 - 60.0).max(8.0));
            ui.label(RichText::new(format!("Snipping in {secs}…")).size(28.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new("The window hides when the countdown ends.").weak());
            ui.add_space(12.0);
            if ui.button("Cancel (Esc)").clicked() {
                self.cancel_countdown(ui.ctx());
            }
        });
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        ui.with_layout(Layout::top_down(Align::Center), |ui| {
            ui.add_space((ui.available_height() * 0.5 - 50.0).max(8.0));
            let title = match self.settings.mode {
                Mode::Photo => "Snip anything on your screen",
                Mode::Video => "Record anything on your screen",
            };
            ui.label(RichText::new(title).size(20.0).strong());
            ui.add_space(6.0);
            let hint = match (self.settings.mode, self.hotkey) {
                (Mode::Photo, Some(h)) => format!("Press {} anywhere, or pick a shape and click New.", h.label()),
                (Mode::Photo, None) => "Pick a shape and click New.".into(),
                (Mode::Video, _) => "Click New, choose an area, then press Start to record.".into(),
            };
            ui.label(RichText::new(hint).weak());
            if let Some(n) = &self.hotkey_note {
                ui.add_space(4.0);
                ui.label(RichText::new(n).small().weak());
            }
        });
    }

    fn picker_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        self.popup_guard.begin_frame(&ctx);
        let rect = ui.max_rect();
        let video = self.settings.mode == Mode::Video;
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
        let mut window_click = false;
        let mut cancel = take_escape(&ctx);

        match self.settings.shape {
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
                        widgets::dashed_outline(painter, shown);
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
                if released_on(&ctx, &resp) && target.width() >= 2.0 && target.height() >= 2.0 {
                    picked = Some(target);
                    window_click = true;
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

        // The shape menu closes while the bar above is drawn, after the pick was read, so the guard can only be asked now.
        if window_click && self.popup_guard.swallows(&ctx, true) {
            (record_area, result) = (None, None);
        }

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
            system_sound: self.settings.record_system,
            microphone: self.settings.record_mic.then(|| self.settings.mic.clone()),
            path: dir.join("Screen Recordings").join(format!("Screen Recording {}.mp4", win::timestamp())),
        };
        let (tx, c) = (self.rec_tx.clone(), ctx.clone());
        bar.rec = Some(recorder::start(settings, move |event| {
            let _ = tx.send(event);
            wake_root(&c);
        }));
    }

    fn on_record_event(&mut self, ctx: &egui::Context, event: recorder::Event) {
        match event {
            recorder::Event::Started { area, notes } => {
                if let Stage::Recording(bar) = &mut self.stage {
                    // The outline and bar follow what is actually recorded.
                    bar.area = area;
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
                if self.quitting {
                    // The file is complete once `Done` arrives; nobody is left to show it to.
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                    return;
                }
                match result {
                    Ok(f) => {
                        let thumb = f.first_frame.map(|img| ctx.load_texture("recording", img, TextureOptions::LINEAR));
                        let video = VideoResult { path: f.path, seconds: f.seconds, size: f.size, thumb };
                        let mut msg = match f.stopped_early {
                            Some(why) => format!("Recording stopped early ({why}) · Saved to Videos\\Screen Recordings"),
                            None => "Saved to Videos\\Screen Recordings".to_string(),
                        };
                        for n in notes {
                            msg.push_str(" · ");
                            msg.push_str(&n);
                        }
                        self.editors.open_video(ctx, video, msg, self.hwnd, self.restore_corner);
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
        self.return_home(ctx);
        self.place_window(ctx);
    }

    fn rec_bar_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let hwnd = self.hwnd as HWND;
        let message = self.live_note(&ctx);
        let Stage::Recording(bar) = &mut self.stage else { return };

        // Sit just above the area, or inside its top edge when there's no room.
        let ppp = ctx.input(|i| i.viewport().native_pixels_per_point).unwrap_or(1.0);
        let bar_h = if message.is_some() { BAR.y + BAR_NOTE_H } else { BAR.y };
        let (bw, bh) = ((BAR.x * ppp).round() as i32, (bar_h * ppp).round() as i32);
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
        if !recording && take_escape(&ctx) {
            close = true;
        }

        if let Some(m) = &message {
            egui::Panel::bottom("rec_note")
                .frame(egui::Frame::new().fill(TOOLBAR_BG).inner_margin(egui::Margin::symmetric(12, 6)))
                .show_separator_line(false)
                .show(ui, |ui| {
                    ui.add(egui::Label::new(RichText::new(m).color(ICON)).truncate());
                });
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(TOOLBAR_BG).stroke((BAR_BORDER, OUTLINE)).inner_margin(egui::Margin::symmetric(BAR_PAD as i8, BAR_PAD as i8)))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = vec2(GAP, 0.0);
                ui.horizontal_centered(|ui| {
                    let (rect, resp) = ui.allocate_exact_size(vec2(REC_START_W, BTN.y), Sense::click());
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
                    let (t, _) = ui.allocate_exact_size(vec2(REC_CLOCK_W, BTN.y), Sense::hover());
                    ui.painter().text(t.center(), Align2::CENTER_CENTER, clock, FontId::monospace(14.0), ICON);

                    let mic_icon = if self.settings.record_mic { Icon::Mic } else { Icon::MicOff };
                    let mic_tip = if self.settings.record_mic { "Mute microphone" } else { "Unmute microphone" };
                    if icon_button(ui, mic_icon, None, false, !recording, mic_tip).clicked() {
                        self.settings.record_mic = !self.settings.record_mic;
                    }
                    let (chev, chev_resp) = ui.allocate_exact_size(vec2(REC_CHEVRON_W, BTN.y), if recording { Sense::hover() } else { Sense::click() });
                    icons::paint(ui.painter(), Rect::from_center_size(chev.center(), vec2(12.0, 12.0)), Icon::Chevron, if recording { ICON_OFF } else { ICON }, None);
                    if chev_resp.clicked() {
                        self.mics = audio::microphones();
                    }
                    egui::Popup::menu(&chev_resp).show(|ui| {
                        if ui.selectable_label(!self.settings.record_mic, "Mute").clicked() {
                            self.settings.record_mic = false;
                        }
                        ui.separator();
                        for (i, m) in self.mics.iter().enumerate() {
                            let chosen = self.settings.record_mic
                                && match &self.settings.mic {
                                    Some(id) => *id == m.id,
                                    None => i == 0,
                                };
                            if ui.selectable_label(chosen, &m.name).clicked() {
                                self.settings.record_mic = true;
                                self.settings.mic = Some(m.id.clone());
                            }
                        }
                        if self.mics.is_empty() {
                            ui.weak("No microphones found");
                        }
                    });

                    let sys_icon = if self.settings.record_system { Icon::SystemSound } else { Icon::SystemSoundOff };
                    let sys_tip = if self.settings.record_system { "Mute system audio" } else { "Unmute system audio" };
                    if icon_button(ui, sys_icon, None, false, !recording, sys_tip).clicked() {
                        self.settings.record_system = !self.settings.record_system;
                    }
                    divider(ui);
                    let close_tip = if recording { "Stop and save" } else { "Close (Esc)" };
                    if icon_button(ui, Icon::Close, None, false, !saving, close_tip).clicked() {
                        if recording { stop = true } else { close = true }
                    }
                    if !notes.is_empty() {
                        let (warn, resp) = ui.allocate_exact_size(vec2(REC_WARN_W, BTN.y), Sense::hover());
                        ui.painter().text(warn.center(), Align2::CENTER_CENTER, "!", FontId::proportional(14.0), ACCENT);
                        resp.on_hover_text(&notes);
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

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.screenshot else { return };
        ctx.request_repaint();
        if job.target != ViewportId::ROOT {
            ctx.request_repaint_of(job.target);
        }
        job.frames += 1;
        if job.frames == 120 {
            let (tx, rx) = channel();
            job.rx = Some(rx);
            let c = ctx.clone();
            let shot = egui::ScreenshotCallback::new(move |img| {
                let _ = tx.send(img);
                c.request_repaint_of(ViewportId::ROOT);
            });
            ctx.send_viewport_cmd_to(job.target, ViewportCommand::Screenshot(shot));
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

/// Brings the sign-in entry in line with "Start SnipScreen with Windows" on one worker, so quick
/// toggles are applied in order (and only the newest of a burst at all) and the entry ends up as the
/// setting says. Dropping it applies what is still queued, then stops.
struct StartupEntry {
    tx: Option<Sender<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl StartupEntry {
    fn new(report: Notifier, apply: impl Fn(bool) -> Result<(), String> + Send + 'static) -> Self {
        let (tx, rx) = channel::<bool>();
        let thread = std::thread::spawn(move || {
            while let Ok(mut on) = rx.recv() {
                while let Ok(newer) = rx.try_recv() {
                    on = newer;
                }
                if let Err(e) = apply(on) {
                    report.send(format!("Couldn't update starting with Windows: {e}"));
                }
            }
        });
        Self { tx: Some(tx), thread: Some(thread) }
    }

    fn set(&self, on: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(on);
        }
    }
}

impl Drop for StartupEntry {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Wakes the shell from another thread. `request_repaint` would target whichever viewport is in a
/// pass right now (an editor or Settings, whose passes share the context), leaving the event unread
/// until something else repaints the root.
fn wake_root(ctx: &egui::Context) {
    ctx.request_repaint_of(ViewportId::ROOT);
}

/// Messages show in the home window while it is on screen, and always in the recording bar, which
/// draws them itself; otherwise they go to the newest editor.
fn note_goes_home(home_visible: bool, recording: bool) -> bool {
    home_visible || recording
}

/// Without a tray icon the app ends once the home window was closed and the last editor has gone,
/// unless a capture is still running. In the tray it only ends on Quit.
fn no_window_left(resident: bool, home_visible: bool, no_editors: bool, idle: bool) -> bool {
    !resident && !home_visible && no_editors && idle
}

/// Where a start out of sight stands; see `App::hidden_start`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HiddenStart {
    /// The first frame hasn't been painted yet; eframe shows the window right after it.
    FirstFrame,
    /// Painted, shown by eframe while cloaked, and hidden again. Uncloak on the next pass.
    Painted,
    Done,
}

/// A made-up snip for `--screenshot-editor`: a deterministic gradient with a title bar and a few
/// light cards, so scaling, ink and the toolbar can be checked in a render without a real screen.
fn sample_image(w: usize, h: usize) -> ColorImage {
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let card = (x % 240) >= 24 && (x % 240) < 216 && y >= 96 && (y - 96) % 168 < 132;
            px.push(if y < 48 {
                Color32::from_rgb(0x24, 0x29, 0x33)
            } else if card {
                Color32::from_rgb(0xf4, 0xf6, 0xfa)
            } else {
                Color32::from_rgb((70.0 + 90.0 * fx) as u8, (110.0 + 60.0 * fy) as u8, (170.0 + 60.0 * (1.0 - fx)) as u8)
            });
        }
    }
    ColorImage::new([w, h], px)
}

fn sample_strokes() -> Vec<markup::Stroke> {
    let line = |a: Pos2, b: Pos2| (0..=20).map(|i| a + (b - a) * (i as f32 / 20.0)).collect::<Vec<_>>();
    vec![
        markup::Stroke { ink: markup::Ink::Highlighter, color: Color32::from_rgb(0xff, 0xe6, 0x00), width: 18.0, points: line(pos2(270.0, 140.0), pos2(450.0, 140.0)) },
        markup::Stroke { ink: markup::Ink::Pen, color: Color32::from_rgb(0xe8, 0x1c, 0x24), width: 4.0, points: line(pos2(520.0, 300.0), pos2(700.0, 420.0)) },
    ]
}

fn crop_px(img: &ColorImage, r: Rect) -> ColorImage {
    let x0 = (r.min.x.round().max(0.0) as usize).min(img.size[0] - 1);
    let y0 = (r.min.y.round().max(0.0) as usize).min(img.size[1] - 1);
    let x1 = (r.max.x.round() as usize).clamp(x0 + 1, img.size[0]);
    let y1 = (r.max.y.round() as usize).clamp(y0 + 1, img.size[1]);
    markup::sub_image(img, x0, y0, x1 - x0, y1 - y0)
}

fn size_tag(painter: &egui::Painter, shown: Rect, px: Vec2) {
    let text = format!("{:.0} × {:.0}", px.x, px.y);
    let galley = painter.layout_no_wrap(text, FontId::proportional(12.0), Color32::WHITE);
    let at = shown.left_bottom() + vec2(0.0, 6.0);
    let bg = Rect::from_min_size(at, galley.size() + vec2(10.0, 4.0));
    painter.rect_filled(bg, 4.0, Color32::from_black_alpha(190));
    painter.galley(at + vec2(5.0, 2.0), galley, Color32::WHITE);
}

fn window_hwnd(cc: &eframe::CreationContext<'_>) -> isize {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match cc.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => h.hwnd.get(),
        _ => 0,
    }
}

/// What the command line asks for. `--open-link <link>` (what Windows runs for a `snipscreen:` link,
/// see `notify::register`) carries text from outside, so after it nothing is read as a flag: a crafted
/// link can't smuggle in `--screenshot <path>` or anything else.
fn parse_args(args: &[String]) -> Result<(Option<Shot>, tray::Intent), String> {
    if args.first().map(String::as_str) == Some(tray::OPEN_LINK_FLAG) {
        return Ok((None, tray::Intent::Open(args.get(1).cloned().unwrap_or_default())));
    }
    let mut shot = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let make = match a.as_str() {
            "--screenshot" => Shot::Home,
            "--screenshot-editor" => Shot::Editor,
            "--screenshot-settings" => Shot::Settings,
            _ => continue,
        };
        match it.next() {
            Some(p) => shot = Some(make(PathBuf::from(p))),
            None => return Err(format!("{a} needs a file name, e.g. {a} shot.png")),
        }
    }
    Ok((shot, tray::Intent::from_args(args)))
}

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (shot, intent) = match parse_args(&args) {
        Ok(parsed) => parsed,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    win::become_dpi_aware();
    let loaded = settings::load();
    // A screenshot run must not take the shortcut or the single-instance slot from a running copy.
    // This comes first so a launch that only forwards to the running copy exits without more work.
    let boot = if shot.is_none() {
        match start_resident(intent, loaded.settings.shortcut) {
            Some(b) => Some(b),
            None => return Ok(()),
        }
    } else {
        None
    };
    if boot.is_some() {
        notify::set_process_id();
    }
    let mut viewport = egui::ViewportBuilder::default().with_title("Snipscreen").with_inner_size(HOME_SIZE).with_min_inner_size([MIN_WINDOW_W, MIN_WINDOW_H]);
    // The icon is built into the binary and checked by `app_icon_decodes`, so a failure here
    // means a broken build; the app still runs with the default icon rather than refusing to start.
    let icon = match app_icon() {
        Ok(icon) => Some(Arc::new(icon)),
        Err(e) => {
            eprintln!("couldn't load the app icon: {e}");
            None
        }
    };
    if let Some(icon) = &icon {
        viewport = viewport.with_icon(icon.clone());
    }
    if boot.as_ref().is_some_and(Boot::starts_hidden) {
        // eframe's show after the first frame must not take focus from what the person is doing.
        viewport = viewport.with_active(false);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Snipscreen", options, Box::new(move |cc| Ok(Box::new(App::new(cc, shot, icon, loaded, boot)))))
}

/// The single running copy's resident side, set up before the window so a second launch can reach
/// it as early as possible.
struct Boot {
    agent: Option<tray::Agent>,
    hotkey: Result<Hotkey, String>,
    events: Receiver<tray::Event>,
    /// Filled with the egui context once it exists, so agent events can wake the UI.
    waker: Arc<OnceLock<egui::Context>>,
    /// What this launch was started for.
    intent: tray::Intent,
}

impl Boot {
    /// Started with Windows, or to open a notification's snip: the home window stays out of sight.
    fn starts_hidden(&self) -> bool {
        matches!(self.intent, tray::Intent::Tray | tray::Intent::Open(_))
    }
}

/// Claims the single-instance slot and starts the agent. `None` means this launch is done: its intent
/// went to the copy already running, or it was `--quit` with nothing to quit.
fn start_resident(intent: tray::Intent, hotkey: Hotkey) -> Option<Boot> {
    if !tray::claim_single_instance() {
        if let Err(e) = tray::forward(&intent) {
            if intent != tray::Intent::Quit {
                win::error_box(&e);
            }
        }
        return None;
    }
    if intent == tray::Intent::Quit {
        return None;
    }
    let waker: Arc<OnceLock<egui::Context>> = Arc::new(OnceLock::new());
    let (tx, events) = channel();
    let w = waker.clone();
    // ceiling: eframe holds back every repaint of a hidden window by >= 100 ms
    // (INVISIBLE_WINDOW_REPAINT_INTERVAL), so a shortcut, tray click or notification click reaches
    // `logic` ~100 ms late while the home window is closed. Keeping the root window shown but
    // cloaked (and off the taskbar) while "hidden" would remove that, if the delay ever matters.
    let wake = move |e| {
        let _ = tx.send(e);
        if let Some(ctx) = w.get() {
            wake_root(ctx);
        }
    };
    let (agent, hotkey) = match tray::Agent::start(hotkey, true, wake) {
        Ok(started) => (Some(started.agent), started.hotkey),
        Err(e) => (None, Err(format!("Global shortcuts are off: {e}"))),
    };
    Some(Boot { agent, hotkey, events, waker, intent })
}

/// The window and taskbar icon: the 256 px render of `assets/snipscreen-icon.svg`, built into the binary.
fn app_icon() -> Result<egui::IconData, String> {
    let bytes: &[u8] = include_bytes!("../assets/snipscreen-icon-256.png");
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().map_err(|e| e.to_string())?;
    let mut rgba = vec![0; reader.output_buffer_size().ok_or("the icon is too large to decode")?];
    let info = reader.next_frame(&mut rgba).map_err(|e| e.to_string())?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return Err(format!("the icon is {:?} at {:?}, not 8-bit RGBA", info.color_type, info.bit_depth));
    }
    rgba.truncate(info.buffer_size());
    Ok(egui::IconData { rgba, width: info.width, height: info.height })
}

/// Snips kept in memory for a click on their notification, by count and by bytes (a 1080p
/// full-screen snip is ~8 MB).
const RECENT_SNIPS: usize = 10;
const RECENT_BYTES: usize = 64 << 20;

/// What background snip work reports to the shell.
enum SnipEvent {
    /// A snip read back from its file for a click on an older notification.
    Loaded(Result<ColorImage, String>),
    /// The notification for snip `id` couldn't be shown; `msg` says why and what copy/save did.
    NotShown { id: u64, msg: String },
}

/// A new snip's id: milliseconds since 1970, nudged up so two snips never share one, even across restarts.
fn next_snip_id(last: u64, now: std::time::SystemTime) -> u64 {
    let ms = now.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
    ms.max(last + 1)
}

/// What the automatic copy and save did with a new snip; `None` for a job that wasn't asked for.
struct Delivered {
    png: Result<Vec<u8>, String>,
    copied: Option<Result<(), String>>,
    /// Where it was saved.
    saved: Option<Result<PathBuf, String>>,
}

/// The snip as PNG, encoded once for both jobs, then copied and/or saved (to a new file) as asked.
fn copy_and_save(hwnd: isize, image: &ColorImage, copy: bool, save_to: Option<Result<PathBuf, String>>) -> Delivered {
    let png = win::encode_png(image);
    let bytes = || png.as_ref().map_err(|e| format!("couldn't encode the snip: {e}"));
    let copied = copy.then(|| bytes().and_then(|p| win::copy_image_with_png(hwnd as HWND, image, p)));
    let saved = save_to.map(|to| to.and_then(|path| bytes().and_then(|p| win::save_new(&path, p))));
    Delivered { png, copied, saved }
}

/// The worker behind `App::notify_snip`: copy and save (while the preview is made alongside), then
/// the notification. A snip that wasn't auto-saved is kept in `recent` after the notification is up,
/// so a click from the notification centre can still open it after a restart. `Err` is the message
/// for the editor the snip then opens in.
fn deliver_by_notification(id: u64, image: &ColorImage, hwnd: isize, copy: bool, save_to: Option<Result<PathBuf, String>>) -> Result<(), String> {
    let recent = notify::recent_dir();
    let preview_path = recent.as_ref().map(|d| d.join(format!("preview-{id}.png")));
    let (preview, Delivered { png, copied, saved }) = std::thread::scope(|s| {
        let preview = s.spawn(|| preview_path.as_ref().map(|p| win::save_png(p, &notify::preview_of(image)).map(|_| p.clone())));
        let delivered = copy_and_save(hwnd, image, copy, save_to);
        (preview.join().ok().flatten().and_then(Result::ok), delivered)
    });
    let (title, lines) = toast_text(&copied, &saved);
    let unsaved = match &saved {
        Some(Ok(_)) => None,
        _ => recent.as_ref().map(|d| d.join(format!("snip-{id}.png"))),
    };
    let file = match (&saved, &unsaved) {
        (Some(Ok(p)), _) => p.clone(),
        (_, Some(p)) => p.clone(),
        _ => return Err(not_shown("couldn't find your local AppData folder", copied, saved)),
    };
    let toast = notify::Toast { title, lines, image: preview, launch: notify::open_link(id, &file) };
    if let Err(e) = toast.show() {
        return Err(not_shown(&e, copied, saved));
    }
    if let (Some(path), Ok(png)) = (&unsaved, &png) {
        let _ = std::fs::write(path, png);
    }
    if let Some(dir) = &recent {
        notify::prune_recent(dir);
    }
    Ok(())
}

/// The editor's message when the notification failed: why, plus what copy and save did.
fn not_shown(why: &str, copied: Option<Result<(), String>>, saved: Option<Result<PathBuf, String>>) -> String {
    let mut msg = format!("Couldn't show the notification ({why}), so the snip opened here");
    if let Some(note) = auto_note(copied, saved.map(|r| r.map(|_| ()))) {
        msg.push_str(" · ");
        msg.push_str(&note);
    }
    msg
}

/// A new snip's notification text: the title says what happened to it (copied, saved, or neither),
/// the first line where it was saved and what failed, the last what a click does.
fn toast_text(copied: &Option<Result<(), String>>, saved: &Option<Result<PathBuf, String>>) -> (String, Vec<String>) {
    let title = match (copied, saved) {
        (Some(Ok(())), _) => "Snip copied to clipboard",
        (_, Some(Ok(_))) => "Snip saved",
        _ => "Snip taken",
    };
    let mut status = Vec::new();
    if let Some(Ok(p)) = saved {
        status.push(format!("Saved to Pictures\\Screenshots as {}", p.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned())));
    }
    if let Some(Err(e)) = copied {
        status.push(format!("Couldn't copy: {e}"));
    }
    if let Some(Err(e)) = saved {
        status.push(format!("Couldn't auto-save: {e}"));
    }
    let mut lines: Vec<String> = (!status.is_empty()).then(|| status.join(" · ")).into_iter().collect();
    lines.push("Select to mark it up".into());
    (title.into(), lines)
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

/// True on the frame the primary button comes up over `resp`. egui reports a press held longer than
/// ~0.8 s or moved more than ~6 pt as a drag, not a click, and a still, slow press produces no frame
/// that marks it a drag, so `clicked` and `drag_stopped` alone miss some releases.
fn released_on(ctx: &egui::Context, resp: &egui::Response) -> bool {
    resp.clicked() || resp.drag_stopped() || (ctx.input(|i| i.pointer.primary_released()) && resp.contains_pointer())
}

/// Esc is grabbed system-wide only while a countdown is running.
fn wants_escape_hotkey(stage: &Stage) -> bool {
    matches!(stage, Stage::Counting { .. })
}


/// The New button's tooltip; the global shortcut is only advertised while it is registered.
/// `note` is why it isn't (none in a screenshot run, which never tries).
fn new_tooltip(mode: Mode, hotkey: Option<&str>, note: Option<&str>) -> String {
    let what = match mode {
        Mode::Photo => "New snip",
        Mode::Video => "New recording",
    };
    match (hotkey, note) {
        (Some(key), _) => format!("{what} (Ctrl+N, or {key} anywhere)"),
        (None, Some(why)) => format!("{what} (Ctrl+N). {why}"),
        (None, None) => format!("{what} (Ctrl+N)"),
    }
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
        // The window reaches PAD pixels past the area on every side; the dashes go just outside it.
        widgets::dashed_outline(ui.painter(), ui.max_rect().shrink(PAD as f32 / ui.ctx().pixels_per_point()));
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
    use eframe::egui::Event;

    #[test]
    fn recording_bar_fits_every_control_at_fractional_scales() {
        let need = row_width(&REC_ROW) + 2.0 * (BAR_PAD + BAR_BORDER);
        let need_h = BTN.y + 2.0 * (BAR_PAD + BAR_BORDER);
        for ppp in [1.0_f32, 1.25, 1.5, 1.75, 2.0, 2.25, 3.0] {
            // What the window really gets: whole pixels, back in points.
            let (w, h) = ((BAR.x * ppp).round() / ppp, (BAR.y * ppp).round() / ppp);
            assert!(w >= need + 2.0 && h >= need_h + 2.0, "bar {w}x{h} at {ppp}x is too small for {need}x{need_h}");
        }
    }

    #[test]
    fn new_tooltip_only_promises_a_registered_hotkey() {
        assert_eq!(new_tooltip(Mode::Photo, Some("Alt+Shift+S"), None), "New snip (Ctrl+N, or Alt+Shift+S anywhere)");
        assert_eq!(new_tooltip(Mode::Video, Some("Ctrl+F12"), None), "New recording (Ctrl+N, or Ctrl+F12 anywhere)");
        let taken = new_tooltip(Mode::Photo, None, Some("Alt+Shift+S is already used by another app"));
        assert_eq!(taken, "New snip (Ctrl+N). Alt+Shift+S is already used by another app");
        assert!(!new_tooltip(Mode::Video, None, None).contains("Alt+Shift+S"));
    }

    /// A worker that finishes while an editor's pass runs must still wake the root, which reads the
    /// channels. The pass below is an editor's own, as eframe runs it for a deferred viewport.
    #[test]
    fn workers_wake_the_root_even_during_another_windows_pass() {
        let ctx = egui::Context::default();
        let woken = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = woken.clone();
        ctx.set_repaint_observer(move |info| log.lock().unwrap().push(info.viewport_id));
        let editor = ViewportId::from_hash_of("an editor");
        let mut plain_went_to = Vec::new();
        let mut input = egui::RawInput { viewport_id: editor, ..Default::default() };
        input.viewports.insert(editor, egui::ViewportInfo::default());
        let mut out = ctx.run_ui(input, |ui| {
            woken.lock().unwrap().clear();
            let (plain, root) = (ui.ctx().clone(), ui.ctx().clone());
            std::thread::spawn(move || plain.request_repaint()).join().unwrap();
            plain_went_to = woken.lock().unwrap().split_off(0);
            std::thread::spawn(move || wake_root(&root)).join().unwrap();
        });
        out.textures_delta.clear();
        assert_eq!(plain_went_to, [editor], "the trap: a bare request_repaint goes to the window in its pass");
        assert!(woken.lock().unwrap().starts_with(&[ViewportId::ROOT]), "{:?}", woken.lock().unwrap());
    }

    #[test]
    fn quick_startup_toggles_end_as_the_setting_says() {
        let applied = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = applied.clone();
        let (tx, rx) = channel();
        let report = Notifier::new(tx, egui::Context::default(), ViewportId::ROOT);
        let entry = StartupEntry::new(report, move |on| {
            std::thread::sleep(Duration::from_millis(20));
            log.lock().unwrap().push(on);
            if on { Ok(()) } else { Err("access denied".into()) }
        });
        for on in [true, false, true, false, true] {
            entry.set(on);
        }
        drop(entry);
        let applied = applied.lock().unwrap();
        assert_eq!(applied.last(), Some(&true), "the newest choice is applied last: {applied:?}");
        assert!(applied.len() < 5, "a burst collapses: {applied:?}");
        assert!(rx.try_iter().all(|m| m == "Couldn't update starting with Windows: access denied"));
    }

    #[test]
    fn countdown_shows_whole_seconds_rounded_up() {
        let ms = Duration::from_millis;
        assert_eq!(countdown_secs(ms(3000)), Some(3));
        assert_eq!(countdown_secs(ms(2999)), Some(3));
        assert_eq!(countdown_secs(ms(2001)), Some(3));
        assert_eq!(countdown_secs(ms(2000)), Some(2));
        assert_eq!(countdown_secs(ms(1)), Some(1));
        assert_eq!(countdown_secs(Duration::ZERO), None);
    }

    /// Frames of a press-move-release on a fresh widget; returns whether `released_on` fired on any of them.
    fn release_detected(hold_secs: f64, wobble: f32) -> bool {
        let ctx = egui::Context::default();
        let (from, to) = (pos2(100.0, 100.0), pos2(100.0 + wobble, 100.0));
        let btn = |pos, pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let frames = [
            (0.0, vec![Event::PointerMoved(from)]),
            (0.1, vec![btn(from, true)]),
            (0.1 + hold_secs / 2.0, vec![Event::PointerMoved(to)]),
            (0.1 + hold_secs, vec![btn(to, false)]),
            (0.2 + hold_secs, vec![]),
        ];
        let mut seen = false;
        for (time, events) in frames {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
                time: Some(time),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let resp = ui.interact(ui.max_rect(), ui.make_persistent_id("picker"), Sense::click_and_drag());
                seen |= released_on(ui.ctx(), &resp);
            });
            out.textures_delta.clear();
        }
        seen
    }

    #[test]
    fn window_pick_registers_on_a_quick_click() {
        assert!(release_detected(0.1, 0.0));
    }

    #[test]
    fn window_pick_registers_on_a_slow_press() {
        assert!(release_detected(1.5, 0.0), "held past the click duration");
    }

    #[test]
    fn window_pick_registers_on_a_wobbly_press() {
        assert!(release_detected(0.2, 20.0), "moved past the click distance");
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
    fn notification_text_is_truthful_about_copy_and_save() {
        let ok = || Some(Ok(()));
        let saved = || Some(Ok(PathBuf::from(r"C:\Users\a\Pictures\Screenshots\Snip 2026-11-05 141530.png")));
        let hint = "Select to mark it up".to_string();
        let as_saved = "Saved to Pictures\\Screenshots as Snip 2026-11-05 141530.png".to_string();
        assert_eq!(toast_text(&ok(), &saved()), ("Snip copied to clipboard".into(), vec![as_saved.clone(), hint.clone()]));
        assert_eq!(toast_text(&ok(), &None), ("Snip copied to clipboard".into(), vec![hint.clone()]));
        assert_eq!(toast_text(&None, &saved()), ("Snip saved".into(), vec![as_saved.clone(), hint.clone()]));
        assert_eq!(toast_text(&None, &None), ("Snip taken".into(), vec![hint.clone()]));
        assert_eq!(toast_text(&Some(Err("busy".into())), &saved()), ("Snip saved".into(), vec![format!("{as_saved} · Couldn't copy: busy"), hint.clone()]));
        assert_eq!(
            toast_text(&ok(), &Some(Err("disk full".into()))),
            ("Snip copied to clipboard".into(), vec!["Couldn't auto-save: disk full".into(), hint.clone()])
        );
        assert_eq!(toast_text(&Some(Err("busy".into())), &Some(Err("disk full".into()))).0, "Snip taken");
    }

    #[test]
    fn a_link_never_turns_into_flags() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (shot, intent) = parse_args(&args(&["--open-link", "snipscreen:open?id=1&path=x", "--screenshot", "C:\\evil.png", "--quit"])).unwrap();
        assert!(shot.is_none(), "--screenshot after a link is part of nothing");
        assert_eq!(intent, tray::Intent::Open("snipscreen:open?id=1&path=x".into()));
        let (shot, intent) = parse_args(&args(&["--open-link"])).unwrap();
        assert!(shot.is_none());
        assert_eq!(intent, tray::Intent::Open(String::new()), "an empty link opens nothing later");
        // Only in first place does it mean anything; a bare link is never opened.
        let (_, intent) = parse_args(&args(&["--tray", "--open-link", "snipscreen:open?id=1&path=x"])).unwrap();
        assert_eq!(intent, tray::Intent::Tray);
        assert_eq!(parse_args(&args(&["snipscreen:open?id=1&path=x"])).unwrap().1, tray::Intent::Show);
        assert!(matches!(parse_args(&args(&["--screenshot", "a.png"])), Ok((Some(Shot::Home(_)), tray::Intent::Show))));
        assert!(parse_args(&args(&["--screenshot"])).is_err());
    }

    #[test]
    fn snip_ids_never_repeat() {
        let t = std::time::UNIX_EPOCH + Duration::from_millis(1_000);
        assert_eq!(next_snip_id(0, t), 1_000);
        assert_eq!(next_snip_id(1_000, t), 1_001, "two snips in one millisecond");
        assert_eq!(next_snip_id(5_000, t), 5_001, "the clock went back");
    }

    #[test]
    fn one_png_serves_both_the_clipboard_and_the_file() {
        let dir = std::env::temp_dir().join(format!("snipscreen-deliver-{}", std::process::id()));
        let img = ColorImage::new([4, 3], vec![Color32::from_rgb(1, 2, 3); 12]);
        let Delivered { png, copied, saved } = copy_and_save(0, &img, false, Some(Ok(dir.join("Snip.png"))));
        let on_disk = saved.as_ref().and_then(|r| r.as_ref().ok()).and_then(|p| std::fs::read(p).ok());
        let failed = copy_and_save(0, &img, false, Some(Err("couldn't find your Pictures folder".into()))).saved;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(copied.is_none(), "copy wasn't asked for");
        assert_eq!(on_disk, png.ok(), "the saved file is the encoded PNG");
        assert_eq!(failed, Some(Err("couldn't find your Pictures folder".into())));
    }

    #[test]
    fn global_esc_is_registered_only_while_counting() {
        let end = Instant::now();
        assert!(wants_escape_hotkey(&Stage::Counting { end }));
        for stage in [Stage::Idle, Stage::Waiting] {
            assert!(!wants_escape_hotkey(&stage), "Esc must stay with the focused app outside a countdown");
        }
    }


    /// One frame in the picker's order: the full-screen response is read first, the shape menu shown after it.
    /// Returns whether a Window-mode pick survives the guard.
    fn picker_frame(ctx: &egui::Context, guard: &mut PopupClickGuard, events: Vec<Event>, open: bool) -> bool {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))),
            events,
            ..Default::default()
        };
        let mut picks = false;
        let mut out = ctx.run_ui(input, |ui| {
            guard.begin_frame(ui.ctx());
            let rect = ui.max_rect();
            let resp = ui.interact(rect, ui.make_persistent_id(egui::IdSalt::new("picker")), Sense::click_and_drag());
            let wants = released_on(ui.ctx(), &resp);
            egui::Area::new(ui.make_persistent_id(egui::IdSalt::new("bar"))).fixed_pos(pos2(10.0, 10.0)).show(ui.ctx(), |ui| {
                let menu = ui.button("shape");
                egui::Popup::from_response(&menu)
                    .open_memory(open.then_some(egui::SetOpenCommand::Bool(true)))
                    .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
                    .show(|ui| ui.label("options"));
            });
            picks = wants && !guard.swallows(ui.ctx(), true);
        });
        out.textures_delta.clear();
        picks
    }

    #[test]
    fn click_that_closes_the_shape_menu_does_not_pick_a_window() {
        let (ctx, mut guard) = (egui::Context::default(), PopupClickGuard::default());
        for _ in 0..3 {
            picker_frame(&ctx, &mut guard, vec![], false);
        }
        picker_frame(&ctx, &mut guard, vec![], true);
        picker_frame(&ctx, &mut guard, vec![], false);
        assert!(egui::Popup::is_any_open(&ctx), "menu should be open before the click");
        let at = pos2(400.0, 400.0);
        let btn = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        picker_frame(&ctx, &mut guard, vec![Event::PointerMoved(at)], false);
        picker_frame(&ctx, &mut guard, vec![btn(true)], false);
        assert!(!picker_frame(&ctx, &mut guard, vec![btn(false)], false), "the dismissing click must not pick");
        assert!(!egui::Popup::is_any_open(&ctx), "click outside should have closed the menu");
        picker_frame(&ctx, &mut guard, vec![btn(true)], false);
        assert!(picker_frame(&ctx, &mut guard, vec![btn(false)], false), "the next click picks as usual");
    }

    #[test]
    fn app_icon_decodes() {
        let icon = app_icon().expect("the built-in icon decodes");
        assert_eq!((icon.width, icon.height), (256, 256));
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        // Transparent outside the rounded tile, opaque at its centre.
        assert_eq!(icon.rgba[3], 0, "top-left corner is outside the tile");
        assert_eq!(icon.rgba[(128 * 256 + 128) * 4 + 3], 255, "the tile's centre is opaque");
    }

    #[test]
    fn messages_follow_the_window_that_is_on_screen() {
        assert!(note_goes_home(true, false), "home window open: it shows its own messages");
        assert!(note_goes_home(false, true), "the recording bar always shows its own");
        assert!(!note_goes_home(false, false), "home window closed: the newest editor shows them");
    }

    #[test]
    fn the_app_ends_only_when_no_window_is_left() {
        assert!(no_window_left(false, false, true, true));
        assert!(!no_window_left(false, true, true, true), "the home window is still open");
        assert!(!no_window_left(false, false, false, true), "an editor is still open");
        assert!(!no_window_left(false, false, true, false), "a capture is still running and will open an editor");
        assert!(!no_window_left(true, false, true, true), "in the tray the app stays until Quit");
    }

    #[test]
    fn sample_snip_is_deterministic_and_fits_its_strokes() {
        let (a, b) = (sample_image(960, 600), sample_image(960, 600));
        assert_eq!(a.size, [960, 600]);
        assert!(a.pixels == b.pixels, "the same picture every run, so renders can be compared");
        for s in sample_strokes() {
            assert!(s.points.iter().all(|p| p.x < 960.0 && p.y < 600.0));
        }
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
