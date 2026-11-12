//! Toolbar icons from Bootstrap Icons 1.13.2 (MIT, `assets/icons/LICENSE`).
//!
//! Each icon is the vendored SVG's path data, parsed once and rasterised with
//! vello_cpu to an alpha mask at the exact device-pixel size it is drawn at.
//! The mask is a texture painted tinted with the icon's colour, so strokes are
//! as sharp as the 16-unit grid allows at any display scale, and the texture
//! is uploaded once per (icon, size), not per frame.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use eframe::egui::{pos2, vec2, Color32, ColorImage, Id, Painter, Rect, Stroke, TextureHandle, TextureOptions, Vec2};
use vello_cpu::kurbo::{self, Affine, Circle, Shape as _};
use vello_cpu::peniko::{BlendMode, Compose, Fill, Mix};
use vello_cpu::{color::palette::css, Pixmap, RenderContext, Resources};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Plus,
    Rectangle,
    Window,
    FullScreen,
    Freeform,
    Timer,
    TimerOff,
    Chevron,
    /// A pointer, with a dot of the selected mark's ink.
    Select,
    Pen,
    Highlighter,
    Eraser,
    /// Shape tools: the shape, with a dot of the ink they draw in.
    Arrow,
    Line,
    RectOutline,
    Ellipse,
    /// A "T", with a dot of the text's ink.
    Text,
    Crop,
    /// Lines of text between corner brackets: pick out the text in a snip.
    ScanText,
    Check,
    Close,
    Undo,
    Redo,
    Save,
    Copy,
    /// A painter's palette: open the snip in Paint.
    Paint,
    More,
    Camera,
    Video,
    Mic,
    MicOff,
    SystemSound,
    SystemSoundOff,
    /// Ring with a dot, as on a Start-recording button.
    Record,
    Stop,
    Play,
}

/// Every icon, in declaration order (`ALL[i] as usize == i`), which indexes `ART`.
const ALL: [Icon; 36] = [
    Icon::Plus,
    Icon::Rectangle,
    Icon::Window,
    Icon::FullScreen,
    Icon::Freeform,
    Icon::Timer,
    Icon::TimerOff,
    Icon::Chevron,
    Icon::Select,
    Icon::Pen,
    Icon::Highlighter,
    Icon::Eraser,
    Icon::Arrow,
    Icon::Line,
    Icon::RectOutline,
    Icon::Ellipse,
    Icon::Text,
    Icon::Crop,
    Icon::ScanText,
    Icon::Check,
    Icon::Close,
    Icon::Undo,
    Icon::Redo,
    Icon::Save,
    Icon::Copy,
    Icon::Paint,
    Icon::More,
    Icon::Camera,
    Icon::Video,
    Icon::Mic,
    Icon::MicOff,
    Icon::SystemSound,
    Icon::SystemSoundOff,
    Icon::Record,
    Icon::Stop,
    Icon::Play,
];

/// The Bootstrap icon each variant is drawn from.
fn svg(icon: Icon) -> &'static str {
    macro_rules! bi {
        ($name:literal) => {
            include_str!(concat!("../assets/icons/", $name, ".svg"))
        };
    }
    match icon {
        Icon::Plus => bi!("plus-lg"),
        Icon::Rectangle => bi!("bounding-box"),
        Icon::Window => bi!("window"),
        Icon::FullScreen => bi!("fullscreen"),
        Icon::Freeform => bi!("bezier2"),
        Icon::Timer | Icon::TimerOff => bi!("stopwatch"),
        Icon::Chevron => bi!("chevron-down"),
        Icon::Select => bi!("cursor"),
        Icon::Pen => bi!("pencil"),
        Icon::Highlighter => bi!("highlighter"),
        Icon::Eraser => bi!("eraser"),
        Icon::Arrow => bi!("arrow-up-right"),
        Icon::Line => bi!("slash-lg"),
        Icon::RectOutline => bi!("square"),
        Icon::Ellipse => bi!("circle"),
        Icon::Text => bi!("fonts"),
        Icon::Crop => bi!("crop"),
        Icon::ScanText => bi!("scan-text"),
        Icon::Check => bi!("check-lg"),
        Icon::Close => bi!("x-lg"),
        Icon::Undo => bi!("arrow-counterclockwise"),
        Icon::Redo => bi!("arrow-clockwise"),
        Icon::Save => bi!("floppy"),
        Icon::Copy => bi!("copy"),
        Icon::Paint => bi!("palette"),
        Icon::More => bi!("three-dots"),
        Icon::Camera => bi!("camera"),
        Icon::Video => bi!("camera-video"),
        Icon::Mic => bi!("mic"),
        Icon::MicOff => bi!("mic-mute"),
        Icon::SystemSound => bi!("volume-up"),
        Icon::SystemSoundOff => bi!("volume-mute"),
        Icon::Record => bi!("record-circle"),
        Icon::Stop => bi!("stop-fill"),
        Icon::Play => bi!("play-fill"),
    }
}

/// Bootstrap Icons are drawn on a 16×16 grid.
const GRID: f64 = 16.0;
/// The ink dot's centre and radius, and the clear gap around it, on that grid.
const DOT: (f64, f64) = (12.7, 12.7);
const DOT_R: f64 = 2.3;
const DOT_GAP: f64 = 1.3;
/// A diagonal bar across the stopwatch (no delay), and the clear gap beside it.
const SLASH: ((f64, f64), (f64, f64)) = ((1.5, 1.5), (14.5, 14.5));
const SLASH_W: f64 = 1.1;
const SLASH_GAP: f64 = 1.1;

struct Layer {
    path: kurbo::BezPath,
    rule: Fill,
}

/// The `<path>` elements of a Bootstrap icon: every one is a filled `d` with an optional `fill-rule`.
fn parse(svg: &str) -> Result<Vec<Layer>, String> {
    svg.split("<path")
        .skip(1)
        .map(|tag| {
            let tag = tag.split('>').next().unwrap_or_default();
            let d = tag.split(" d=\"").nth(1).and_then(|s| s.split('"').next()).ok_or("path without d")?;
            let path = kurbo::BezPath::from_svg(d).map_err(|e| format!("bad path data: {e}"))?;
            let rule = if tag.contains("fill-rule=\"evenodd\"") { Fill::EvenOdd } else { Fill::NonZero };
            Ok(Layer { path, rule })
        })
        .collect()
}

static ART: LazyLock<Vec<Vec<Layer>>> = LazyLock::new(|| {
    ALL.iter()
        .map(|&icon| {
            parse(svg(icon)).unwrap_or_else(|e| {
                eprintln!("icon {icon:?}: {e}");
                Vec::new()
            })
        })
        .collect()
});

/// The icon's alpha at `px`×`px` device pixels. `dot` clears a gap for the ink dot.
fn coverage(icon: Icon, px: u16, dot: bool) -> Vec<u8> {
    let mut cx = RenderContext::new(px, px);
    let s = f64::from(px) / GRID;
    // Bootstrap's cursor points up and to the right; mirrored, it reads as the usual pointer.
    cx.set_transform(if icon == Icon::Select { Affine::new([-s, 0.0, 0.0, s, f64::from(px), 0.0]) } else { Affine::scale(s) });
    cx.set_paint(css::WHITE);
    for layer in &ART[icon as usize] {
        cx.set_fill_rule(layer.rule);
        cx.fill_path(&layer.path);
    }
    let clear = BlendMode::new(Mix::Normal, Compose::Clear);
    let over = BlendMode::default();
    cx.set_fill_rule(Fill::NonZero);
    if icon == Icon::TimerOff {
        let mut bar = kurbo::BezPath::new();
        bar.move_to(SLASH.0);
        bar.line_to(SLASH.1);
        cx.set_blend_mode(clear);
        cx.set_stroke(kurbo::Stroke::new(SLASH_W + 2.0 * SLASH_GAP).with_caps(kurbo::Cap::Butt));
        cx.stroke_path(&bar);
        cx.set_blend_mode(over);
        cx.set_stroke(kurbo::Stroke::new(SLASH_W).with_caps(kurbo::Cap::Butt));
        cx.stroke_path(&bar);
    }
    if dot {
        cx.set_blend_mode(clear);
        cx.fill_path(&Circle::new(DOT, DOT_R + DOT_GAP).to_path(0.05));
    }
    cx.flush();
    let mut pixmap = Pixmap::new(px, px);
    cx.render(&mut pixmap, &mut Resources::new());
    pixmap.data().iter().map(|p| p.a).collect()
}

/// Uploaded masks, per egui context (a texture belongs to the context that made it).
// ceiling: never evicted; sizes only change with the display scale or a window's DPI, so a handful per icon. Evict if icons are ever drawn at animated sizes.
#[derive(Clone, Default)]
struct Textures(Arc<Mutex<HashMap<Mask, TextureHandle>>>);

/// What a mask depends on: the icon, its size in device pixels, and whether it has an ink dot's gap.
type Mask = (Icon, u16, bool);

/// The square of whole device pixels, centred on `rect` and as wide as its shorter side, and its size in pixels.
fn snap(rect: Rect, ppp: f32) -> (Rect, u16) {
    let px = (rect.width().min(rect.height()) * ppp).round().clamp(1.0, 1024.0);
    let min = ((rect.center().to_vec2() * ppp - Vec2::splat(px) / 2.0).round() / ppp).to_pos2();
    (Rect::from_min_size(min, Vec2::splat(px / ppp)), px as u16)
}

/// Paints `icon` centred in `rect`, snapped to whole device pixels so it stays crisp at any scale.
/// `ink` adds a dot of the colour the tool draws in at the bottom-right.
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32, ink: Option<Color32>) {
    let ctx = p.ctx();
    let (at, px) = snap(rect, ctx.pixels_per_point());
    let key = (icon, px, ink.is_some());
    let textures = ctx.data_mut(|d| d.get_temp_mut_or_insert_with(Id::NULL, Textures::default).clone());
    let texture = textures
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_insert_with(|| {
            let alpha = coverage(icon, px, key.2);
            let image = ColorImage { size: [usize::from(px); 2], source_size: Vec2::splat(f32::from(px)), pixels: alpha.into_iter().map(Color32::from_white_alpha).collect() };
            ctx.load_texture(format!("icon-{icon:?}-{px}"), image, TextureOptions::LINEAR)
        })
        .id();
    p.image(texture, at, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), color);
    if let Some(ink) = ink {
        let unit = at.width() / GRID as f32;
        p.circle(at.min + vec2(DOT.0 as f32, DOT.1 as f32) * unit, DOT_R as f32 * unit, ink, Stroke::new(1.0, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_land_on_whole_device_pixels_at_any_scale() {
        for ppp in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for (x, y, side) in [(10.0, 10.0, 16.0), (10.3, 7.7, 18.0), (33.5, 2.25, 12.0), (5.0, 5.0, 28.0)] {
                let (at, px) = snap(Rect::from_min_size(pos2(x, y), Vec2::splat(side)), ppp);
                let device = |v: f32| (v * ppp, (v * ppp).round());
                for v in [at.min.x, at.min.y, at.width(), at.height()] {
                    let (exact, whole) = device(v);
                    assert!((exact - whole).abs() < 1e-3, "{v} not on the pixel grid at {ppp}x");
                }
                assert!((f32::from(px) - at.width() * ppp).abs() < 1e-3);
                assert!((at.center() - Rect::from_min_size(pos2(x, y), Vec2::splat(side)).center()).length() <= 0.5 / ppp * 1.5);
            }
        }
    }

    #[test]
    fn all_lists_every_icon_in_order() {
        assert_eq!(ALL.len(), Icon::Play as usize + 1);
        for (i, icon) in ALL.iter().enumerate() {
            assert_eq!(*icon as usize, i, "{icon:?} out of order");
        }
    }

    #[test]
    fn every_icon_parses_and_rasterises() {
        for icon in ALL {
            let layers = parse(svg(icon)).unwrap_or_else(|e| panic!("{icon:?}: {e}"));
            assert!(!layers.is_empty(), "{icon:?} has no paths");
            for px in [16, 18, 20, 23, 27, 36] {
                let mask = coverage(icon, px, false);
                assert_eq!(mask.len(), usize::from(px) * usize::from(px));
                let ink: u32 = mask.iter().map(|&a| u32::from(a)).sum();
                assert!(ink > 255 * u32::from(px) / 2, "{icon:?} at {px}px is blank (alpha sum {ink})");
                assert!(mask.iter().any(|&a| a > 127), "{icon:?} at {px}px has no solid pixel");
            }
        }
    }

    #[test]
    fn ink_dot_clears_a_gap() {
        let px = 32u16;
        let at = |m: &[u8], x: f64, y: f64| m[(y * f64::from(px) / GRID) as usize * usize::from(px) + (x * f64::from(px) / GRID) as usize];
        for icon in [Icon::Pen, Icon::Highlighter, Icon::Arrow, Icon::Line, Icon::RectOutline, Icon::Ellipse, Icon::Text] {
            let plain = coverage(icon, px, false);
            let gapped = coverage(icon, px, true);
            assert_eq!(at(&gapped, DOT.0, DOT.1), 0, "{icon:?}: dot centre not cleared");
            assert!(gapped.iter().map(|&a| u32::from(a)).sum::<u32>() <= plain.iter().map(|&a| u32::from(a)).sum::<u32>());
            assert!(gapped.iter().any(|&a| a > 127), "{icon:?}: the dot gap erased the whole icon");
        }
    }

    #[test]
    fn no_delay_stopwatch_differs_from_the_plain_one() {
        assert_ne!(coverage(Icon::Timer, 32, false), coverage(Icon::TimerOff, 32, false));
    }

    #[test]
    fn a_texture_is_uploaded_once_per_icon_and_size() {
        let ctx = eframe::egui::Context::default();
        let mut uploads = 0;
        for _ in 0..3 {
            let mut out = ctx.run_ui(Default::default(), |ui| {
                paint(ui.painter(), Rect::from_min_size(pos2(10.3, 10.7), Vec2::splat(18.0)), Icon::Camera, Color32::WHITE, None);
            });
            uploads += out.textures_delta.set.iter().filter(|(id, _)| **id != eframe::egui::TextureId::default()).count();
            out.textures_delta.clear();
        }
        assert_eq!(uploads, 1);
    }
}
