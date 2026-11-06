//! Snip document: picture, marks (ink strokes), crop and undo history.
//!
//! Every mark is rasterised on the CPU by `Mark::paint`, tile run by tile run. The editor shows a
//! mark being drawn by painting the same runs over the picture it will be burned into, so what is
//! on screen is exactly what gets copied or saved, blend modes included.

use std::sync::Arc;

use eframe::egui::{Color32, ColorImage, Pos2, Rect};
use skrifa::MetadataProvider as _;
use vello_cpu::kurbo::{self, Shape as _};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ink {
    Pen,
    /// Multiplies with the picture like a real highlighter: white turns the ink's colour and dark
    /// text stays dark, instead of a see-through coat laid on top.
    Highlighter,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Stroke {
    pub ink: Ink,
    pub color: Color32,
    /// Thickness in image pixels.
    pub width: f32,
    /// Points in image pixels.
    pub points: Vec<Pos2>,
}

impl Stroke {
    fn radius(&self) -> f32 {
        (self.width * 0.5).max(0.5)
    }

    /// Segments from point `from - 1` onwards; a lone point is a segment of length zero.
    fn segments_from(&self, from: usize) -> impl Iterator<Item = (Pos2, Pos2)> + '_ {
        let pts = self.points.get(from.saturating_sub(1)..).unwrap_or(&[]);
        let single = (pts.len() == 1).then(|| (pts[0], pts[0]));
        single.into_iter().chain(pts.windows(2).map(|p| (p[0], p[1])))
    }

    /// Tiles the segments from point `from - 1` onwards can touch; `from = 0` is the whole stroke.
    pub fn footprint_from(&self, from: usize, tiles: &mut Tiles) {
        let reach = self.radius() + 1.0;
        for (a, b) in self.segments_from(from) {
            tiles.add_segment(a, b, reach);
        }
    }

    fn paint(&self, patch: &mut ColorImage, at: [usize; 2]) {
        let mut cover = vec![0f32; patch.pixels.len()];
        cover_segments(&mut cover, patch_rect(patch, at), self.segments_from(0), self.radius());
        let blend = match self.ink {
            Ink::Pen => over,
            Ink::Highlighter => multiply,
        };
        for (px, &k) in patch.pixels.iter_mut().zip(&cover) {
            if k > 0.0 {
                *px = blend(*px, self.color, k);
            }
        }
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        let reach = self.width * 0.5 + radius;
        self.segments_from(0).any(|(a, b)| dist_to_segment(p, a, b) <= reach)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeKind {
    Arrow,
    Line,
    Rectangle,
    Ellipse,
}

/// A shape dragged out from `from` to `to`: a line or arrow between them, or the outline of the
/// rectangle or ellipse they span. Drawn opaque in `color`.
#[derive(Clone, PartialEq, Debug)]
pub struct Shape {
    pub kind: ShapeKind,
    pub color: Color32,
    /// Line thickness in image pixels.
    pub width: f32,
    pub from: Pos2,
    pub to: Pos2,
}

impl Shape {
    fn radius(&self) -> f32 {
        (self.width * 0.5).max(0.5)
    }

    /// End of the shaft and the head's corners (tip first). The head grows with the line width and
    /// shrinks to fit a short arrow; the shaft ends inside the head so its round cap never shows.
    fn arrow(&self) -> (Pos2, [Pos2; 3]) {
        let d = self.to - self.from;
        let len = d.length();
        if len < 1e-3 {
            return (self.to, [self.to; 3]);
        }
        let dir = d / len;
        let length = (self.width * 3.0 + 6.0).min(len);
        let half = length * 0.6;
        let base = self.to - dir * length;
        let side = dir.rot90() * half;
        (base + dir * length * 0.3, [self.to, base + side, base - side])
    }

    /// The thick centre-line segments of a line, arrow shaft or ellipse.
    fn segments(&self) -> Vec<(Pos2, Pos2)> {
        match self.kind {
            ShapeKind::Line => vec![(self.from, self.to)],
            ShapeKind::Arrow => vec![(self.from, self.arrow().0)],
            ShapeKind::Rectangle => {
                let r = Rect::from_two_pos(self.from, self.to);
                let c = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom()];
                (0..4).map(|i| (c[i], c[(i + 1) % 4])).collect()
            }
            ShapeKind::Ellipse => {
                let r = Rect::from_two_pos(self.from, self.to);
                let (a, b) = (r.width() / 2.0, r.height() / 2.0);
                // Equal steps of the angle stray from the true curve by at most `a * step² / 8`; this
                // many keeps that under 0.05 px at any size, with short chords where it bends most.
                let n = (10.0 * a.max(b).sqrt()).clamp(24.0, 4096.0) as usize;
                let at = |i: usize| {
                    let t = std::f32::consts::TAU * i as f32 / n as f32;
                    r.center() + eframe::egui::vec2(a * t.cos(), b * t.sin())
                };
                (0..n).map(|i| (at(i), at(i + 1))).collect()
            }
        }
    }

    fn footprint(&self, tiles: &mut Tiles) {
        let reach = self.radius() + 1.0;
        for (a, b) in self.segments() {
            tiles.add_segment(a, b, reach);
        }
        if self.kind == ShapeKind::Arrow {
            tiles.add(Rect::from_points(&self.arrow().1).expand(1.0));
        }
    }

    /// Paints with `segments` and `head` worked out beforehand by `Mark::raster`.
    fn paint(&self, segments: &[(Pos2, Pos2)], head: Option<[Pos2; 3]>, patch: &mut ColorImage, at: [usize; 2]) {
        let r = patch_rect(patch, at);
        let mut cover = vec![0f32; patch.pixels.len()];
        match self.kind {
            ShapeKind::Rectangle => cover_box_outline(&mut cover, r, Rect::from_two_pos(self.from, self.to), self.radius()),
            _ => cover_segments(&mut cover, r, segments.iter().copied(), self.radius()),
        }
        if let Some(head) = head {
            cover_triangle(&mut cover, r, head);
        }
        for (px, &k) in patch.pixels.iter_mut().zip(&cover) {
            if k > 0.0 {
                *px = over(*px, self.color, k);
            }
        }
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        let reach = self.width * 0.5 + radius;
        self.segments().into_iter().any(|(a, b)| dist_to_segment(p, a, b) <= reach)
            || (self.kind == ShapeKind::Arrow && triangle_distance(p, self.arrow().1) <= radius)
    }
}

/// Where a shape dragged from `from` to `to` ends with Shift held: lines and arrows snap to the
/// nearest 45°, keeping their length; rectangles and ellipses become squares and circles.
pub fn constrain(kind: ShapeKind, from: Pos2, to: Pos2) -> Pos2 {
    let d = to - from;
    match kind {
        ShapeKind::Line | ShapeKind::Arrow => {
            let step = std::f32::consts::FRAC_PI_4;
            let angle = (d.y.atan2(d.x) / step).round() * step;
            from + eframe::egui::vec2(angle.cos(), angle.sin()) * d.length()
        }
        ShapeKind::Rectangle | ShapeKind::Ellipse => {
            let side = d.x.abs().max(d.y.abs());
            let sign = |v: f32| if v < 0.0 { -1.0 } else { 1.0 };
            from + eframe::egui::vec2(sign(d.x), sign(d.y)) * side
        }
    }
}

/// Geist SemiBold 1.8.2 (SIL Open Font License 1.1, `assets/fonts/OFL.txt`), the face all text is set in.
static GEIST: &[u8] = include_bytes!("../assets/fonts/Geist-SemiBold.ttf");

/// Typed text, set in Geist SemiBold. `pos` is the top-left corner of the first line.
#[derive(Clone, PartialEq, Debug)]
pub struct Text {
    pub text: String,
    pub color: Color32,
    /// Font size in image pixels.
    pub size: f32,
    /// A halo in a contrasting colour under the letters, so they read on any background.
    pub outline: bool,
    pub pos: Pos2,
}

/// The font's vertical measures at one size, in pixels.
struct LineMetrics {
    ascent: f32,
    height: f32,
}

impl Text {
    fn font() -> Option<skrifa::FontRef<'static>> {
        skrifa::FontRef::new(GEIST).ok()
    }

    fn line_metrics(&self, font: &skrifa::FontRef) -> LineMetrics {
        let m = font.metrics(skrifa::instance::Size::new(self.size), skrifa::instance::LocationRef::default());
        LineMetrics { ascent: m.ascent, height: m.ascent - m.descent + m.leading }
    }

    /// Height of one line, in image pixels.
    pub fn line_height(&self) -> f32 {
        Self::font().map_or(self.size * 1.25, |f| self.line_metrics(&f).height)
    }

    /// Width of the halo stroked under the letters (half of it shows outside them).
    fn halo(&self) -> f32 {
        if self.outline {
            (self.size * 0.16).max(2.0)
        } else {
            0.0
        }
    }

    /// White under dark or colourful text, near-black under light text.
    pub fn halo_color(&self) -> Color32 {
        let [r, g, b, _] = self.color.to_array().map(|v| v as f32 / 255.0);
        if 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5 {
            Color32::from_rgb(0x1f, 0x1f, 0x1f)
        } else {
            Color32::WHITE
        }
    }

    /// Lays the text out: calls `glyph(id, x, baseline)` for every character, then returns the end
    /// of the last line (the caret's spot, on the top of that line).
    fn lay_out(&self, font: &skrifa::FontRef, mut glyph: impl FnMut(skrifa::GlyphId, f32, f32)) -> Pos2 {
        let lm = self.line_metrics(font);
        let charmap = font.charmap();
        let advances = font.glyph_metrics(skrifa::instance::Size::new(self.size), skrifa::instance::LocationRef::default());
        let mut end = self.pos;
        for (i, line) in self.text.split('\n').enumerate() {
            let top = self.pos.y + i as f32 * lm.height;
            let mut x = self.pos.x;
            for ch in line.chars() {
                let id = charmap.map(ch).unwrap_or(skrifa::GlyphId::NOTDEF);
                glyph(id, x, top + lm.ascent);
                x += advances.advance_width(id).unwrap_or(0.0);
            }
            end = Pos2::new(x, top);
        }
        end
    }

    /// Where the caret goes: after the last letter, at the top of its line.
    pub fn caret(&self) -> Pos2 {
        Self::font().map_or(self.pos, |f| self.lay_out(&f, |_, _, _| {}))
    }

    /// The letters' outlines in image pixels.
    fn path(&self) -> kurbo::BezPath {
        let mut path = kurbo::BezPath::new();
        let Some(font) = Self::font() else { return path };
        let outlines = font.outline_glyphs();
        let settings = || skrifa::outline::DrawSettings::unhinted(skrifa::instance::Size::new(self.size), skrifa::instance::LocationRef::default());
        self.lay_out(&font, |id, x, baseline| {
            if let Some(g) = outlines.get(id) {
                // A glyph that fails to draw is left out rather than failing the whole text.
                let _ = g.draw(settings(), &mut GlyphPen { path: &mut path, x: x as f64, y: baseline as f64 });
            }
        });
        path
    }

    /// Image pixels the text can touch, its halo included.
    fn bounds(&self) -> Rect {
        path_bounds(&self.path(), self.halo())
    }

    /// Every glyph and where it sits: `(glyph, x, baseline)`, in image pixels.
    fn glyphs(&self) -> Vec<(skrifa::GlyphId, f32, f32)> {
        let mut out = Vec::new();
        if let Some(font) = Self::font() {
            self.lay_out(&font, |id, x, baseline| out.push((id, x, baseline)));
        }
        out
    }

    /// Image pixels one glyph at `x` on `baseline` can touch, halo included.
    fn glyph_bounds(&self, font: &skrifa::FontRef, id: skrifa::GlyphId, x: f32, baseline: f32) -> Rect {
        let mut path = kurbo::BezPath::new();
        if let Some(g) = font.outline_glyphs().get(id) {
            let settings = skrifa::outline::DrawSettings::unhinted(skrifa::instance::Size::new(self.size), skrifa::instance::LocationRef::default());
            let _ = g.draw(settings, &mut GlyphPen { path: &mut path, x: x as f64, y: baseline as f64 });
        }
        path_bounds(&path, self.halo())
    }

    /// The letters and halo as 8-bit coverage over the pixels they reach in an image of `size`.
    /// Always rendered whole, from the corner of the text's own extent: vello's antialiasing shifts
    /// by a hair with the render's origin, and a fixed origin keeps every render of the same text,
    /// live or saved, identical to the last bit.
    fn masks(&self, size: [usize; 2]) -> Option<TextMasks> {
        let path = self.path();
        let image = Rect::from_min_size(Pos2::ZERO, eframe::egui::vec2(size[0] as f32, size[1] as f32));
        let b = path_bounds(&path, self.halo()).intersect(image);
        if !b.is_positive() {
            return None;
        }
        let area = PxRect {
            x0: b.min.x.floor() as usize,
            y0: b.min.y.floor() as usize,
            x1: (b.max.x.ceil() as usize).min(size[0]),
            y1: (b.max.y.ceil() as usize).min(size[1]),
        };
        let (w, h) = (u16::try_from(area.x1 - area.x0).ok()?, u16::try_from(area.y1 - area.y0).ok()?);
        let render = |stroke: Option<f32>| -> Vec<u8> {
            let mut ctx = vello_cpu::RenderContext::new(w, h);
            ctx.set_transform(kurbo::Affine::translate((-(area.x0 as f64), -(area.y0 as f64))));
            ctx.set_paint(vello_cpu::color::OpaqueColor::<vello_cpu::color::Srgb>::WHITE);
            match stroke {
                Some(width) => {
                    ctx.set_stroke(kurbo::Stroke::new(width as f64).with_join(kurbo::Join::Round));
                    ctx.stroke_path(&path);
                }
                None => ctx.fill_path(&path),
            }
            let mut pixmap = vello_cpu::Pixmap::new(w, h);
            ctx.render(&mut pixmap, &mut vello_cpu::Resources::new());
            // White paint, premultiplied: alpha is the coverage.
            pixmap.data().iter().map(|px| px.a).collect()
        };
        let halo = if self.outline { render(Some(self.halo())) } else { Vec::new() };
        Some(TextMasks { area, fill: render(None), halo })
    }
}

/// A text's coverage over the image pixels `area`, one byte per pixel: the letters and their halo.
struct TextMasks {
    area: PxRect,
    fill: Vec<u8>,
    halo: Vec<u8>,
}

impl TextMasks {
    /// Composites `t` onto `patch` (top-left image pixel `at`): the halo first, the letters over it.
    fn paint(&self, t: &Text, patch: &mut ColorImage, at: [usize; 2]) {
        let r = patch_rect(patch, at);
        let (x0, x1) = (r.x0.max(self.area.x0), r.x1.min(self.area.x1));
        let (y0, y1) = (r.y0.max(self.area.y0), r.y1.min(self.area.y1));
        let (mw, pw) = (self.area.x1 - self.area.x0, patch.size[0]);
        let halo = t.halo_color();
        for y in y0..y1 {
            for x in x0..x1 {
                let m = (y - self.area.y0) * mw + (x - self.area.x0);
                let px = &mut patch.pixels[(y - r.y0) * pw + (x - r.x0)];
                if let Some(&k) = self.halo.get(m).filter(|&&k| k > 0) {
                    *px = over(*px, halo, k as f32 / 255.0);
                }
                if self.fill[m] > 0 {
                    *px = over(*px, t.color, self.fill[m] as f32 / 255.0);
                }
            }
        }
    }
}

/// Tiles where `new` differs from `old`, text being typed: only the glyphs that changed when the
/// style and place are the same (typing adds or takes letters at the end), everything otherwise.
pub fn text_changes(old: &Text, new: &Text, tiles: &mut Tiles) {
    let same_style = (old.color, old.size, old.outline, old.pos) == (new.color, new.size, new.outline, new.pos);
    let Some(font) = Text::font().filter(|_| same_style) else {
        tiles.add(old.bounds());
        tiles.add(new.bounds());
        return;
    };
    let (a, b) = (old.glyphs(), new.glyphs());
    let same = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    for &(id, x, baseline) in a[same..].iter().chain(&b[same..]) {
        tiles.add(old.glyph_bounds(&font, id, x, baseline));
    }
}

/// `path`'s extent widened by half a `halo` and a pixel of antialiasing; nothing for an empty path.
fn path_bounds(path: &kurbo::BezPath, halo: f32) -> Rect {
    if path.elements().is_empty() {
        return Rect::NOTHING;
    }
    let b = path.bounding_box();
    Rect::from_min_max(Pos2::new(b.x0 as f32, b.y0 as f32), Pos2::new(b.x1 as f32, b.y1 as f32)).expand(halo / 2.0 + 1.0)
}

/// Collects a glyph outline from `skrifa` (y up, origin on the baseline) into image pixels.
struct GlyphPen<'a> {
    path: &'a mut kurbo::BezPath,
    x: f64,
    y: f64,
}

impl GlyphPen<'_> {
    fn at(&self, x: f32, y: f32) -> kurbo::Point {
        kurbo::Point::new(self.x + x as f64, self.y - y as f64)
    }
}

impl skrifa::outline::OutlinePen for GlyphPen<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        let p = self.at(x, y);
        self.path.move_to(p);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        let p = self.at(x, y);
        self.path.line_to(p);
    }

    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (c, p) = (self.at(cx, cy), self.at(x, y));
        self.path.quad_to(c, p);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        let (c0, c1, p) = (self.at(cx0, cy0), self.at(cx1, cy1), self.at(x, y));
        self.path.curve_to(c0, c1, p);
    }

    fn close(&mut self) {
        self.path.close_path();
    }
}

/// Anything drawn on the snip. Each mark is burned into the picture on its own, in order.
#[derive(Clone, PartialEq, Debug)]
pub enum Mark {
    Stroke(Stroke),
    Shape(Shape),
    Text(Text),
}

impl From<Text> for Mark {
    fn from(t: Text) -> Self {
        Mark::Text(t)
    }
}

impl From<Shape> for Mark {
    fn from(s: Shape) -> Self {
        Mark::Shape(s)
    }
}

impl From<Stroke> for Mark {
    fn from(s: Stroke) -> Self {
        Mark::Stroke(s)
    }
}

impl Mark {
    /// Every tile the mark can change. Never misses a pixel, so the mark can be burned run by run.
    pub fn footprint(&self, tiles: &mut Tiles) {
        match self {
            Mark::Stroke(s) => s.footprint_from(0, tiles),
            Mark::Shape(s) => s.footprint(tiles),
            Mark::Text(t) => tiles.add(t.bounds()),
        }
    }

    /// The mark ready to paint many patches of an image of `size`: its geometry (a text's letters
    /// too) is worked out once, not per tile run.
    pub fn raster(&self, size: [usize; 2]) -> Raster<'_> {
        let mut r = Raster { mark: self, segments: Vec::new(), head: None, text: None };
        match self {
            Mark::Stroke(_) => {}
            Mark::Shape(s) => {
                if s.kind != ShapeKind::Rectangle {
                    r.segments = s.segments();
                }
                r.head = (s.kind == ShapeKind::Arrow).then(|| s.arrow().1);
            }
            Mark::Text(t) => r.text = t.masks(size),
        }
        r
    }

    /// Image pixels the mark can touch, as a rectangle.
    pub fn bounds(&self) -> Rect {
        match self {
            Mark::Stroke(s) => Rect::from_points(&s.points).expand(s.radius() + 1.0),
            Mark::Shape(s) => {
                let head = if s.kind == ShapeKind::Arrow { Rect::from_points(&s.arrow().1) } else { Rect::NOTHING };
                Rect::from_two_pos(s.from, s.to).union(head).expand(s.radius() + 1.0)
            }
            Mark::Text(t) => t.bounds(),
        }
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        match self {
            Mark::Stroke(s) => s.touches(p, radius),
            Mark::Shape(s) => s.touches(p, radius),
            Mark::Text(t) => t.bounds().expand(radius).contains(p),
        }
    }

    fn translate(&mut self, d: eframe::egui::Vec2) {
        match self {
            Mark::Stroke(s) => s.points.iter_mut().for_each(|p| *p += d),
            Mark::Shape(s) => {
                s.from += d;
                s.to += d;
            }
            Mark::Text(t) => t.pos += d,
        }
    }
}

/// A mark with its geometry worked out, ready to paint patch after patch.
pub struct Raster<'a> {
    mark: &'a Mark,
    segments: Vec<(Pos2, Pos2)>,
    head: Option<[Pos2; 3]>,
    text: Option<TextMasks>,
}

impl Raster<'_> {
    /// Composites the mark onto `patch`, a piece of the picture whose top-left pixel is `at`. The
    /// result for a pixel depends only on that pixel and the mark, never on where the patch ends,
    /// so painting tile runs one at a time gives the same pixels as painting the whole picture.
    pub fn paint(&self, patch: &mut ColorImage, at: [usize; 2]) {
        match self.mark {
            Mark::Stroke(s) => s.paint(patch, at),
            Mark::Shape(s) => s.paint(&self.segments, self.head, patch, at),
            Mark::Text(t) => {
                if let Some(masks) = &self.text {
                    masks.paint(t, patch, at);
                }

            }
        }
    }
}
/// Side of the square tiles marks are rasterised and uploaded in, in image pixels.
pub const TILE: usize = 32;

/// A set of `TILE`-sized tiles of an image: where a mark can paint, or what needs redrawing.
#[derive(Clone, PartialEq, Debug)]
pub struct Tiles {
    size: [usize; 2],
    cols: usize,
    on: Vec<bool>,
}

impl Tiles {
    pub fn new(size: [usize; 2]) -> Self {
        let cols = size[0].div_ceil(TILE);
        Self { size, cols, on: vec![false; cols * size[1].div_ceil(TILE)] }
    }

    pub fn is_empty(&self) -> bool {
        !self.on.contains(&true)
    }

    /// Adds every tile that `r` (image pixels) overlaps.
    pub fn add(&mut self, r: Rect) {
        let [w, h] = self.size.map(|v| v as f32);
        // Written so a NaN rectangle adds nothing.
        if !(r.max.x > 0.0 && r.max.y > 0.0 && r.min.x < w && r.min.y < h) {
            return;
        }
        let first = |v: f32| v.max(0.0) as usize / TILE;
        let end = |v: f32, lim: f32| (v.ceil().min(lim) as usize).div_ceil(TILE);
        let (tx0, ty0, tx1, ty1) = (first(r.min.x), first(r.min.y), end(r.max.x, w), end(r.max.y, h));
        for ty in ty0..ty1 {
            self.on[ty * self.cols + tx0..ty * self.cols + tx1].fill(true);
        }
    }

    /// Tiles within `reach` of the segment `a`–`b`, taken in short pieces so a long diagonal doesn't
    /// claim its whole bounding box.
    pub fn add_segment(&mut self, a: Pos2, b: Pos2, reach: f32) {
        let n = ((b - a).length() / (TILE as f32 / 2.0)).ceil().clamp(1.0, 4096.0) as usize;
        let at = |i: usize| a + (b - a) * (i as f32 / n as f32);
        for i in 0..n {
            self.add(Rect::from_two_pos(at(i), at(i + 1)).expand(reach));
        }
    }

    pub fn union(&mut self, o: &Tiles) {
        for (a, &b) in self.on.iter_mut().zip(&o.on) {
            *a |= b;
        }
    }

    /// The tiles in both `self` and `o`.
    pub fn intersect(&self, o: &Tiles) -> Tiles {
        let on = self.on.iter().zip(&o.on).map(|(&a, &b)| a && b).collect();
        Tiles { on, ..*self }
    }

    /// The tiles in `self` but not in `o`.
    pub fn minus(&self, o: &Tiles) -> Tiles {
        let on = self.on.iter().zip(&o.on).map(|(&a, &b)| a && !b).collect();
        Tiles { on, ..*self }
    }

    /// The tiles as pixel rectangles, neighbours in a row merged into one, clipped to the image.
    pub fn runs(&self) -> Vec<PxRect> {
        let mut out = Vec::new();
        for (ty, row) in self.on.chunks(self.cols.max(1)).enumerate() {
            let mut tx = 0;
            while tx < row.len() {
                if !row[tx] {
                    tx += 1;
                    continue;
                }
                let start = tx;
                while tx < row.len() && row[tx] {
                    tx += 1;
                }
                out.push(PxRect {
                    x0: start * TILE,
                    y0: ty * TILE,
                    x1: (tx * TILE).min(self.size[0]),
                    y1: ((ty + 1) * TILE).min(self.size[1]),
                });
            }
        }
        out
    }
}

/// The image pixels a patch with top-left pixel `at` covers.
fn patch_rect(patch: &ColorImage, at: [usize; 2]) -> PxRect {
    PxRect { x0: at[0], y0: at[1], x1: at[0] + patch.size[0], y1: at[1] + patch.size[1] }
}

/// Raises `cover` (one value per pixel of `r`) to the coverage of round-capped segments of radius
/// `rad`. Taking the max means overlaps and self-crossings never add up.
fn cover_segments(cover: &mut [f32], r: PxRect, segments: impl Iterator<Item = (Pos2, Pos2)>, rad: f32) {
    let bw = r.x1 - r.x0;
    let reach = rad + 1.0;
    for (a, b) in segments {
        let sb = Rect::from_two_pos(a, b).expand(reach);
        let sx0 = (sb.min.x.floor().max(0.0) as usize).max(r.x0);
        let sy0 = (sb.min.y.floor().max(0.0) as usize).max(r.y0);
        let sx1 = (sb.max.x.ceil().max(0.0) as usize).min(r.x1);
        let sy1 = (sb.max.y.ceil().max(0.0) as usize).min(r.y1);
        if sx0 >= sx1 || sy0 >= sy1 {
            continue;
        }
        // A pixel further than `reach` from the segment's line can't be covered, so on a slanted
        // segment each row only visits its band, not the whole bounding box.
        let ab = b - a;
        let len2 = ab.length_sq();
        let inv_len2 = if len2 > 0.0 { 1.0 / len2 } else { 0.0 };
        let n = if len2 > 0.0 { ab.rot90() / len2.sqrt() } else { eframe::egui::Vec2::ZERO };
        // Squared distances beyond which a pixel is untouched, and within which it is fully covered.
        let (out2, in2) = ((rad + 0.5) * (rad + 0.5), if rad > 0.5 { (rad - 0.5) * (rad - 0.5) } else { -1.0 });
        for y in sy0..sy1 {
            let (mut x0, mut x1) = (sx0, sx1);
            if n.x.abs() > 1e-3 {
                let off = n.y * (y as f32 + 0.5 - a.y);
                let (u, v) = (a.x + (-reach - off) / n.x, a.x + (reach - off) / n.x);
                x0 = x0.max((u.min(v) - 0.5).floor().max(0.0) as usize);
                x1 = x1.min((u.max(v) + 0.5).ceil().max(0.0) as usize);
            }
            let row = &mut cover[(y - r.y0) * bw..(y - r.y0 + 1) * bw];
            let py = y as f32 + 0.5 - a.y;
            for x in x0..x1 {
                let slot = &mut row[x - r.x0];
                // Thick strokes overlap themselves a lot; a pixel already fully covered is done.
                if *slot >= 1.0 {
                    continue;
                }
                let px = x as f32 + 0.5 - a.x;
                let t = ((px * ab.x + py * ab.y) * inv_len2).clamp(0.0, 1.0);
                let (dx, dy) = (px - ab.x * t, py - ab.y * t);
                let d2 = dx * dx + dy * dy;
                if d2 >= out2 {
                    continue;
                }
                let c = if d2 <= in2 { 1.0 } else { (rad + 0.5 - d2.sqrt()).clamp(0.0, 1.0) };
                if c > *slot {
                    *slot = c;
                }
            }

        }
    }
}

/// Raises `cover` to the exact area coverage of a rectangle outline: `rect`'s edges drawn
/// `2 * half` thick, straddling the edge, with square corners. Only the band is visited, so a huge
/// rectangle costs its perimeter, not its area.
fn cover_box_outline(cover: &mut [f32], r: PxRect, rect: Rect, half: f32) {
    let (outer, inner) = (rect.expand(half), rect.shrink(half));
    let overlap = |lo: f32, hi: f32, p: f32| (hi.min(p + 1.0) - lo.max(p)).clamp(0.0, 1.0);
    let area = |b: Rect, x: f32, y: f32| if b.is_positive() { overlap(b.min.x, b.max.x, x) * overlap(b.min.y, b.max.y, y) } else { 0.0 };
    let bw = r.x1 - r.x0;
    let clip_x = |v: f32| (v.max(0.0) as usize).clamp(r.x0, r.x1);
    let (y0, y1) = ((outer.min.y.floor().max(0.0) as usize).max(r.y0), (outer.max.y.ceil().max(0.0) as usize).min(r.y1));
    let (ox0, ox1) = (clip_x(outer.min.x.floor()), clip_x(outer.max.x.ceil()));
    for y in y0..y1 {
        let fy = y as f32;
        // Rows that cross the hollow middle only need the left and right bands.
        let hollow = inner.is_positive() && fy >= inner.min.y.ceil() && fy + 1.0 <= inner.max.y.floor();
        let spans = if hollow {
            [(ox0, clip_x(inner.min.x.ceil())), (clip_x(inner.max.x.floor()), ox1)]
        } else {
            [(ox0, ox1), (0, 0)]
        };
        let row = &mut cover[(y - r.y0) * bw..(y - r.y0 + 1) * bw];
        for (sx0, sx1) in spans {
            for x in sx0..sx1 {
                let fx = x as f32;
                let c = area(outer, fx, fy) - area(inner, fx, fy);
                let slot = &mut row[x - r.x0];
                if c > *slot {
                    *slot = c;
                }
            }
        }
    }
}

/// Signed distance from `p` to a triangle (negative inside). Exact inside and along the edges,
/// slightly short just past a corner, which keeps tips sharp.
fn triangle_distance(p: Pos2, t: [Pos2; 3]) -> f32 {
    let centre = (t[0].to_vec2() + t[1].to_vec2() + t[2].to_vec2()) / 3.0;
    (0..3)
        .map(|i| {
            let (a, b) = (t[i], t[(i + 1) % 3]);
            let mut n = (b - a).rot90().normalized();
            if n.dot(a.to_vec2() - centre) < 0.0 {
                n = -n;
            }
            n.dot(p - a)
        })
        .fold(f32::MIN, f32::max)
}

/// Raises `cover` to the antialiased coverage of a filled triangle.
fn cover_triangle(cover: &mut [f32], r: PxRect, t: [Pos2; 3]) {
    if (t[1] - t[0]).rot90().dot(t[2] - t[0]).abs() < 1e-3 {
        return;
    }
    let b = Rect::from_points(&t).expand(1.0);
    let bw = r.x1 - r.x0;
    let (x0, x1) = ((b.min.x.floor().max(0.0) as usize).max(r.x0), (b.max.x.ceil().max(0.0) as usize).min(r.x1));
    let (y0, y1) = ((b.min.y.floor().max(0.0) as usize).max(r.y0), (b.max.y.ceil().max(0.0) as usize).min(r.y1));
    for y in y0..y1 {
        for x in x0..x1 {
            let c = (0.5 - triangle_distance(Pos2::new(x as f32 + 0.5, y as f32 + 0.5), t)).clamp(0.0, 1.0);
            let slot = &mut cover[(y - r.y0) * bw + (x - r.x0)];
            if c > *slot {
                *slot = c;
            }
        }
    }
}

/// Opaque `ink` over the premultiplied pixel `px` at coverage `k`.
fn over(px: Color32, ink: Color32, k: f32) -> Color32 {
    let [r0, g0, b0, a0] = px.to_array();
    let [cr, cg, cb, _] = ink.to_array();
    let mix = |base: u8, ink: u8| (base as f32 * (1.0 - k) + ink as f32 * k).round() as u8;
    let a = (a0 as f32 + (255.0 - a0 as f32) * k).round() as u8;
    Color32::from_rgba_premultiplied(mix(r0, cr), mix(g0, cg), mix(b0, cb), a)
}

/// Opaque `ink` multiplied into the premultiplied pixel `px` at coverage `k` (the W3C multiply
/// blend, source-over). On an opaque pixel this is `lerp(base, base * ink, k)`; where the picture is
/// see-through (outside a freeform snip) the ink shows as itself.
fn multiply(px: Color32, ink: Color32, k: f32) -> Color32 {
    let [r0, g0, b0, a0] = px.to_array();
    let [cr, cg, cb, _] = ink.to_array();
    let clear = 1.0 - a0 as f32 / 255.0;
    let mix = |base: u8, ink: u8| {
        let (b, i) = (base as f32, ink as f32);
        (b + k * (i * clear + b * i / 255.0 - b)).round() as u8
    };
    let a = (a0 as f32 + (255.0 - a0 as f32) * k).round() as u8;
    Color32::from_rgba_premultiplied(mix(r0, cr), mix(g0, cg), mix(b0, cb), a)
}

#[derive(Clone)]
struct Snapshot {
    image: Arc<ColorImage>,
    marks: Vec<Arc<Mark>>,
}

pub struct Doc {
    pub image: Arc<ColorImage>,
    /// Shared, so an undo snapshot costs a pointer per mark and a mark keeps its identity from one
    /// version of the list to the next (`changed_tiles` relies on that).
    pub marks: Vec<Arc<Mark>>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Bumped whenever `image` changes so the texture can be refreshed.
    pub image_rev: u64,
    /// Bumped whenever `marks` change, for the same reason.
    pub ink_rev: u64,
}

const HISTORY: usize = 50;

impl Doc {
    pub fn new(image: ColorImage) -> Self {
        Self {
            image: Arc::new(image),
            marks: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            image_rev: 0,
            ink_rev: 0,
        }
    }

    pub fn size(&self) -> [usize; 2] {
        self.image.size
    }

    /// Call before any change so it can be undone.
    pub fn checkpoint(&mut self) {
        self.undo.push(self.snapshot());
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot { image: self.image.clone(), marks: self.marks.clone() }
    }

    fn restore(&mut self, s: Snapshot) {
        if !Arc::ptr_eq(&s.image, &self.image) {
            self.image_rev += 1;
        }
        self.image = s.image;
        self.marks = s.marks;
        self.ink_rev += 1;
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn undo(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.redo.push(self.snapshot());
            self.restore(s);
        }
    }

    pub fn redo(&mut self) {
        if let Some(s) = self.redo.pop() {
            self.undo.push(self.snapshot());
            self.restore(s);
        }
    }

    /// Adds a finished mark as one undoable step.
    pub fn add_mark(&mut self, m: impl Into<Mark>) {
        self.checkpoint();
        self.marks.push(Arc::new(m.into()));
        self.ink_rev += 1;
    }

    pub fn hits(&self, p: Pos2, radius: f32) -> bool {
        self.marks.iter().any(|m| m.touches(p, radius))
    }

    /// Removes every mark within `radius` of `p`. Returns whether any went.
    pub fn erase_at(&mut self, p: Pos2, radius: f32) -> bool {
        let before = self.marks.len();
        self.marks.retain(|m| !m.touches(p, radius));
        let gone = self.marks.len() != before;
        if gone {
            self.ink_rev += 1;
        }
        gone
    }

    /// Crops to `r` (image pixels); marks move with the picture.
    pub fn crop(&mut self, r: Rect) {
        let [w, h] = self.size();
        let x0 = (r.min.x.round().max(0.0) as usize).min(w);
        let y0 = (r.min.y.round().max(0.0) as usize).min(h);
        let x1 = (r.max.x.round().max(0.0) as usize).min(w);
        let y1 = (r.max.y.round().max(0.0) as usize).min(h);
        if x1 <= x0 + 1 || y1 <= y0 + 1 || (x0, y0, x1, y1) == (0, 0, w, h) {
            return;
        }
        self.checkpoint();
        self.image = Arc::new(sub_image(&self.image, x0, y0, x1 - x0, y1 - y0));
        self.image_rev += 1;
        self.ink_rev += 1;
        let shift = eframe::egui::vec2(x0 as f32, y0 as f32);
        for m in &mut self.marks {
            Arc::make_mut(m).translate(-shift);
        }
    }

    /// Tiles whose pixels differ between the picture burned with `before` and with the current
    /// marks: the footprints of every mark in one list and not the other. Marks are compared by
    /// identity, so `before` must be an earlier version of `marks` on the same picture.
    pub fn changed_tiles(&self, before: &[Arc<Mark>]) -> Tiles {
        let ids = |v: &[Arc<Mark>]| v.iter().map(Arc::as_ptr).collect::<std::collections::HashSet<_>>();
        let (was, now) = (ids(before), ids(&self.marks));
        let mut tiles = Tiles::new(self.size());
        let gone = before.iter().filter(|m| !now.contains(&Arc::as_ptr(m)));
        let added = self.marks.iter().filter(|m| !was.contains(&Arc::as_ptr(m)));
        for m in gone.chain(added) {
            m.footprint(&mut tiles);
        }
        tiles
    }

    /// The flattened picture over each run of `tiles`, built from the bare picture and only the
    /// marks that reach that run: the same pixels `flatten` gives there.
    // ceiling: re-burns every mark that reaches a damaged run (an undo under 20 thick highlighter
    // strokes stacked on one path takes ~90 ms on 4K); keep per-mark layers if piled-up marks feel sticky.
    pub fn render_runs(&self, tiles: &Tiles) -> Vec<(PxRect, ColorImage)> {

        let marks: Vec<(Rect, Raster)> = self.marks.iter().map(|m| (m.bounds(), m.raster(self.size()))).collect();
        tiles
            .runs()
            .into_iter()
            .map(|r| {
                let mut patch = sub_image(&self.image, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
                let area = Rect::from_min_max(Pos2::new(r.x0 as f32, r.y0 as f32), Pos2::new(r.x1 as f32, r.y1 as f32));
                for (b, raster) in &marks {
                    if b.intersects(area) {
                        raster.paint(&mut patch, r.pos());
                    }
                }
                (r, patch)
            })
            .collect()
    }

    /// The picture with all ink burned in, ready to copy or save.
    pub fn flatten(&self) -> ColorImage {
        let mut out = (*self.image).clone();
        for m in &self.marks {
            burn(&mut out, m);
        }
        out
    }
}

/// A pixel rectangle `x0..x1` × `y0..y1` inside an image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PxRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
}

impl PxRect {
    pub fn pos(self) -> [usize; 2] {
        [self.x0, self.y0]
    }
}

pub fn sub_image(img: &ColorImage, x: usize, y: usize, w: usize, h: usize) -> ColorImage {
    let stride = img.size[0];
    let mut pixels = Vec::with_capacity(w * h);
    for row in y..y + h {
        pixels.extend_from_slice(&img.pixels[row * stride + x..row * stride + x + w]);
    }
    ColorImage::new([w, h], pixels)
}

/// Keeps only the pixels inside the closed `outline` (image pixels) and
/// trims the picture to its bounds; the rest turns transparent.
pub fn cut_freeform(img: &ColorImage, outline: &[Pos2]) -> Option<ColorImage> {
    // Shoelace area: a scribble that encloses (almost) nothing is not a snip.
    let twice_area: f32 = (0..outline.len())
        .map(|i| {
            let (a, b) = (outline[i], outline[(i + 1) % outline.len()]);
            a.x * b.y - b.x * a.y
        })
        .sum();
    if outline.len() < 3 || twice_area.abs() < 32.0 {
        return None;
    }
    let bounds = Rect::from_points(outline).intersect(Rect::from_min_max(
        Pos2::ZERO,
        Pos2::new(img.size[0] as f32, img.size[1] as f32),
    ));
    let x0 = bounds.min.x.floor().max(0.0) as usize;
    let y0 = bounds.min.y.floor().max(0.0) as usize;
    let x1 = (bounds.max.x.ceil() as usize).min(img.size[0]);
    let y1 = (bounds.max.y.ceil() as usize).min(img.size[1]);
    if x1 <= x0 + 1 || y1 <= y0 + 1 {
        return None;
    }
    let mut out = sub_image(img, x0, y0, x1 - x0, y1 - y0);
    let w = out.size[0];
    // Even-odd scanline fill: find where each row crosses the outline.
    let mut crossings = Vec::new();
    for row in 0..out.size[1] {
        let cy = (y0 + row) as f32 + 0.5;
        crossings.clear();
        for i in 0..outline.len() {
            let a = outline[i];
            let b = outline[(i + 1) % outline.len()];
            if (a.y <= cy) != (b.y <= cy) {
                crossings.push(a.x + (cy - a.y) / (b.y - a.y) * (b.x - a.x));
            }
        }
        crossings.sort_by(f32::total_cmp);
        let line = &mut out.pixels[row * w..(row + 1) * w];
        for (col, px) in line.iter_mut().enumerate() {
            let cx = (x0 + col) as f32 + 0.5;
            if crossings.partition_point(|&c| c < cx) % 2 == 0 {
                *px = Color32::TRANSPARENT;
            }
        }
    }
    Some(out)
}

fn dist_to_segment(p: Pos2, a: Pos2, b: Pos2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_sq();
    let t = if len2 > 0.0 { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) } else { 0.0 };
    p.distance(a + ab * t)
}

/// Burns one mark into `img`, tile run by tile run.
pub fn burn(img: &mut ColorImage, m: &Mark) {
    let mut tiles = Tiles::new(img.size);
    m.footprint(&mut tiles);
    let raster = m.raster(img.size);
    for r in tiles.runs() {
        let mut patch = sub_image(img, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
        raster.paint(&mut patch, r.pos());
        paste(img, &patch, r.pos());
    }
}

/// Copies `patch` into `img` with its top-left pixel at `at`.
pub fn paste(img: &mut ColorImage, patch: &ColorImage, at: [usize; 2]) {
    let (w, pw) = (img.size[0], patch.size[0]);
    for (row, line) in patch.pixels.chunks(pw.max(1)).enumerate() {
        let start = (at[1] + row) * w + at[0];
        img.pixels[start..start + pw].copy_from_slice(line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn white(w: usize, h: usize) -> ColorImage {
        ColorImage::new([w, h], vec![Color32::WHITE; w * h])
    }

    fn stroke(m: &Mark) -> &Stroke {
        match m {
            Mark::Stroke(s) => s,
            other => panic!("not a stroke: {other:?}"),
        }
    }

    fn shape(kind: ShapeKind, width: f32, from: (f32, f32), to: (f32, f32)) -> Mark {
        Mark::Shape(Shape { kind, color: Color32::RED, width, from: Pos2::new(from.0, from.1), to: Pos2::new(to.0, to.1) })
    }

    #[test]
    fn crop_moves_strokes_and_undo_restores() {
        let mut d = Doc::new(white(100, 80));
        d.checkpoint();
        d.marks.push(Arc::new(Mark::Stroke(Stroke {
            ink: Ink::Pen,
            color: Color32::RED,
            width: 4.0,
            points: vec![Pos2::new(50.0, 40.0)],
        })));
        d.crop(Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(60.0, 70.0)));
        assert_eq!(d.size(), [50, 50]);
        assert_eq!(stroke(&d.marks[0]).points[0], Pos2::new(40.0, 20.0));
        d.undo();
        assert_eq!(d.size(), [100, 80]);
        assert_eq!(stroke(&d.marks[0]).points[0], Pos2::new(50.0, 40.0));
        d.undo();
        assert!(d.marks.is_empty());
        d.redo();
        assert_eq!(d.marks.len(), 1);
    }

    #[test]
    fn flatten_burns_pen_ink() {
        let mut d = Doc::new(white(20, 20));
        d.marks.push(Arc::new(Mark::Stroke(Stroke {
            ink: Ink::Pen,
            color: Color32::BLACK,
            width: 6.0,
            points: vec![Pos2::new(2.0, 10.0), Pos2::new(18.0, 10.0)],
        })));
        let out = d.flatten();
        assert_eq!(out.pixels[10 * 20 + 10], Color32::BLACK);
        assert_eq!(out.pixels[0], Color32::WHITE);
        assert_eq!(d.image.pixels[10 * 20 + 10], Color32::WHITE, "original untouched");
    }

    fn zigzag() -> Stroke {
        // Crosses itself at (20, 20) and turns sharp corners at (35, 35) and (35, 5).
        Stroke {
            ink: Ink::Highlighter,
            color: Color32::from_rgb(255, 220, 0),
            width: 8.0,
            points: [(5.0, 5.0), (35.0, 35.0), (35.0, 5.0), (5.0, 35.0)].map(|(x, y)| Pos2::new(x, y)).to_vec(),
        }
    }

    /// A picture with every channel varying, so blend mistakes show up.
    fn gradient(w: usize, h: usize) -> ColorImage {
        let px = (0..w * h).map(|i| Color32::from_rgb((i % w * 255 / w) as u8, (i / w * 255 / h) as u8, ((i * 7) % 256) as u8)).collect();
        ColorImage::new([w, h], px)
    }

    /// What the editor does while a mark is drawn: repaint the given tiles from the pristine picture.
    fn repaint(shown: &mut ColorImage, pristine: &ColorImage, m: &Mark, tiles: &Tiles) {
        for r in tiles.runs() {
            let mut patch = sub_image(pristine, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
            m.raster(pristine.size).paint(&mut patch, r.pos());
            paste(shown, &patch, r.pos());
        }
    }

    #[test]
    fn highlighter_turns_white_into_its_colour_and_keeps_dark_text_dark() {
        let ink = Color32::from_rgb(255, 220, 0);
        let mut img = white(40, 20);
        for y in 0..20 {
            for x in 20..40 {
                img.pixels[y * 40 + x] = Color32::from_rgb(30, 30, 30);
            }
        }
        let mut d = Doc::new(img);
        d.add_mark(Stroke { ink: Ink::Highlighter, color: ink, width: 10.0, points: vec![Pos2::new(2.0, 10.0), Pos2::new(38.0, 10.0)] });
        let out = d.flatten();
        assert_eq!(out.pixels[10 * 40 + 8], ink, "white becomes the highlight colour");
        let dark = out.pixels[10 * 40 + 30];
        assert!(dark.r() <= 30 && dark.g() <= 30 && dark.b() <= 30, "dark text stays dark, got {dark:?}");
        assert_eq!(dark, Color32::from_rgb(30, 26, 0), "multiplied, not covered");
        assert_eq!(out.pixels[2 * 40 + 8], Color32::WHITE, "outside the stroke untouched");
    }

    #[test]
    fn highlighter_is_uniform_at_crossings_and_corners() {
        let s = zigzag();
        let mut img = white(40, 40);
        burn(&mut img, &Mark::Stroke(s.clone()));
        let at = |x: usize, y: usize| img.pixels[y * 40 + x];
        assert_eq!(at(10, 10), s.color);
        assert_eq!(at(20, 20), s.color, "self-crossing is no darker");
        assert_eq!(at(34, 34), s.color, "joint is no darker");
        assert_eq!(at(35, 36), s.color, "corner is filled round, not notched");
    }

    #[test]
    fn highlighter_on_see_through_pixels_shows_its_own_colour() {
        let ink = Color32::from_rgb(255, 220, 0);
        let mut img = ColorImage::new([20, 20], vec![Color32::TRANSPARENT; 400]);
        burn(&mut img, &Mark::Stroke(Stroke { ink: Ink::Highlighter, color: ink, width: 8.0, points: vec![Pos2::new(10.0, 10.0)] }));
        assert_eq!(img.pixels[10 * 20 + 10], ink);
    }

    #[test]
    fn live_tiles_built_point_by_point_match_the_saved_snip() {
        for ink in [Ink::Pen, Ink::Highlighter] {
            let full = Stroke { ink, ..zigzag() };
            let pristine = gradient(70, 50);
            let mut shown = pristine.clone();
            let mut done = 0;
            for n in 1..=full.points.len() {
                let s = Stroke { points: full.points[..n].to_vec(), ..full.clone() };
                let mut fresh = Tiles::new(pristine.size);
                s.footprint_from(done, &mut fresh);
                repaint(&mut shown, &pristine, &Mark::Stroke(s), &fresh);
                done = n;
            }
            let mut d = Doc::new(pristine);
            d.add_mark(full);
            assert!(shown.pixels == d.flatten().pixels, "{ink:?}: screen and saved snip differ");
        }
    }

    #[test]
    fn footprint_never_misses_a_pixel() {
        let marks = [
            Mark::Stroke(zigzag()),
            Mark::Stroke(Stroke { ink: Ink::Pen, color: Color32::RED, width: 30.0, points: vec![Pos2::new(66.0, 2.0)] }),
            Mark::Stroke(Stroke { ink: Ink::Pen, color: Color32::BLUE, width: 3.0, points: vec![Pos2::new(-5.0, 48.0), Pos2::new(75.0, 1.0)] }),
            shape(ShapeKind::Arrow, 3.0, (5.0, 45.0), (63.5, 4.2)),
            shape(ShapeKind::Arrow, 12.0, (60.0, 10.0), (10.0, 40.0)),
            shape(ShapeKind::Line, 7.0, (-20.0, 20.0), (90.0, 33.0)),
            shape(ShapeKind::Rectangle, 5.0, (3.3, 4.7), (66.2, 44.9)),
            shape(ShapeKind::Rectangle, 40.0, (20.0, 10.0), (30.0, 30.0)),
            shape(ShapeKind::Ellipse, 2.0, (2.0, 2.0), (68.0, 48.0)),
            shape(ShapeKind::Ellipse, 9.0, (30.0, -10.0), (95.0, 20.0)),
            text("Hi", Color32::RED, true),
            Mark::Text(Text { text: "a\nb".into(), color: Color32::WHITE, size: 31.7, outline: true, pos: Pos2::new(-6.3, 18.6) }),
        ];
        for m in &marks {
            let mut by_runs = gradient(70, 50);
            burn(&mut by_runs, m);
            let mut whole = gradient(70, 50);
            m.raster(whole.size).paint(&mut whole, [0, 0]);

            assert!(by_runs.pixels == whole.pixels, "{m:?} paints outside its footprint");
        }
    }

    #[test]
    fn rectangles_have_crisp_square_corners_and_a_hollow_middle() {
        let mut img = white(60, 50);
        burn(&mut img, &shape(ShapeKind::Rectangle, 4.0, (10.0, 10.0), (50.0, 40.0)));
        let at = |x: usize, y: usize| img.pixels[y * 60 + x];
        for (x, y) in [(8, 8), (11, 11), (9, 25), (30, 41), (51, 39)] {
            assert_eq!(at(x, y), Color32::RED, "outline at ({x}, {y})");
        }
        for (x, y) in [(7, 7), (12, 12), (30, 25), (52, 25)] {
            assert_eq!(at(x, y), Color32::WHITE, "clear at ({x}, {y})");
        }
    }

    #[test]
    fn arrows_end_in_a_filled_head_at_the_tip() {
        let mut img = white(80, 40);
        burn(&mut img, &shape(ShapeKind::Arrow, 4.0, (5.0, 20.0), (70.0, 20.0)));
        let at = |x: usize, y: usize| img.pixels[y * 80 + x];
        assert_eq!(at(20, 20), Color32::RED, "shaft");
        assert_eq!(at(20, 23), Color32::WHITE, "shaft is the line width");
        assert_eq!(at(60, 24), Color32::RED, "the head is wider than the shaft");
        assert_eq!(at(66, 20), Color32::RED, "filled up to the tip");
        assert_eq!(at(72, 20), Color32::WHITE, "nothing past the tip");
        // Head scales with the width.
        let Mark::Shape(thin) = shape(ShapeKind::Arrow, 2.0, (0.0, 0.0), (100.0, 0.0)) else { unreachable!() };
        let Mark::Shape(thick) = shape(ShapeKind::Arrow, 10.0, (0.0, 0.0), (100.0, 0.0)) else { unreachable!() };
        assert!(thick.arrow().1[1].y.abs() > 2.0 * thin.arrow().1[1].y.abs());
    }

    #[test]
    fn ellipses_are_round_outlines() {
        let mut img = white(60, 60);
        burn(&mut img, &shape(ShapeKind::Ellipse, 4.0, (10.0, 10.0), (50.0, 50.0)));
        let at = |x: usize, y: usize| img.pixels[y * 60 + x];
        for (x, y) in [(10, 30), (49, 30), (30, 10), (30, 49), (16, 16)] {
            assert_eq!(at(x, y), Color32::RED, "on the circle at ({x}, {y})");
        }
        for (x, y) in [(30, 30), (11, 11), (5, 30)] {
            assert_eq!(at(x, y), Color32::WHITE, "off the circle at ({x}, {y})");
        }
    }

    #[test]
    fn shift_snaps_lines_to_45_degrees_and_boxes_to_squares() {
        let from = Pos2::new(10.0, 10.0);
        let snapped = constrain(ShapeKind::Line, from, Pos2::new(20.0, 11.0));
        assert!((snapped.y - 10.0).abs() < 1e-4 && (snapped.x - (10.0 + 101f32.sqrt())).abs() < 1e-4, "{snapped:?}");
        let diag = constrain(ShapeKind::Arrow, from, Pos2::new(0.0, 21.0));
        assert!(((diag - from).angle() - 3.0 * std::f32::consts::FRAC_PI_4).abs() < 1e-4, "{diag:?}");
        assert_eq!(constrain(ShapeKind::Rectangle, from, Pos2::new(20.0, 7.0)), Pos2::new(20.0, 0.0));
        assert_eq!(constrain(ShapeKind::Ellipse, from, Pos2::new(4.0, 30.0)), Pos2::new(-10.0, 30.0));
    }

    #[test]
    fn eraser_finds_shapes_by_their_outline() {
        let mut d = Doc::new(white(100, 100));
        d.add_mark(shape(ShapeKind::Rectangle, 4.0, (10.0, 10.0), (90.0, 90.0)));
        d.add_mark(shape(ShapeKind::Arrow, 4.0, (10.0, 50.0), (60.0, 50.0)));
        assert!(!d.hits(Pos2::new(30.0, 30.0), 3.0), "inside the rectangle is empty");
        assert!(d.hits(Pos2::new(50.0, 54.0), 1.0), "in the arrow head");
        assert!(d.erase_at(Pos2::new(90.0, 40.0), 2.0));
        assert_eq!(d.marks.len(), 1);
        assert!(matches!(&*d.marks[0], Mark::Shape(Shape { kind: ShapeKind::Arrow, .. })));
        d.crop(Rect::from_min_max(Pos2::new(5.0, 5.0), Pos2::new(95.0, 95.0)));
        assert!(matches!(&*d.marks[0], Mark::Shape(s) if s.from == Pos2::new(5.0, 45.0)), "crop moves shapes too");
    }

    #[test]
    fn patching_only_the_changed_tiles_matches_a_full_flatten() {
        let mut d = Doc::new(gradient(120, 90));
        // Overlapping marks of every kind, so a redrawn tile must re-burn its neighbours in order.
        d.add_mark(Stroke { ink: Ink::Highlighter, ..zigzag() });
        d.add_mark(shape(ShapeKind::Rectangle, 6.0, (10.0, 10.0), (100.0, 70.0)));
        d.add_mark(Stroke { ink: Ink::Pen, color: Color32::BLUE, width: 5.0, points: vec![Pos2::new(0.0, 80.0), Pos2::new(119.0, 5.0)] });
        d.add_mark(shape(ShapeKind::Ellipse, 3.0, (30.0, 20.0), (110.0, 85.0)));
        d.add_mark(Stroke { ink: Ink::Highlighter, color: Color32::from_rgb(92, 214, 255), width: 14.0, points: vec![Pos2::new(5.0, 40.0), Pos2::new(115.0, 45.0)] });
        d.add_mark(Text { text: "Ok".into(), color: Color32::BLACK, size: 40.0, outline: true, pos: Pos2::new(40.0, 30.0) });

        let mut shown = d.flatten();
        let mut burned = d.marks.clone();
        let mut check = |d: &Doc, what: &str| {
            for (r, patch) in d.render_runs(&d.changed_tiles(&burned)) {
                paste(&mut shown, &patch, r.pos());
            }
            burned = d.marks.clone();
            assert!(shown.pixels == d.flatten().pixels, "after {what}");
        };
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(100.0, 40.0), 2.0));
        check(&d, "erasing the rectangle");
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(60.0, 43.0), 1.0));
        check(&d, "erasing through a pile of marks");
        d.undo();
        check(&d, "undo");
        d.undo();
        check(&d, "a second undo");
        d.redo();
        check(&d, "redo");
        d.undo();
        d.undo();
        check(&d, "undoing an add");
        assert_eq!(d.changed_tiles(&d.marks.clone()), Tiles::new(d.size()), "nothing changed, nothing to redraw");
    }

    fn text(s: &str, color: Color32, outline: bool) -> Mark {
        Mark::Text(Text { text: s.into(), color, size: 60.0, outline, pos: Pos2::new(10.0, 5.0) })
    }

    /// Pixels of `img` equal to `c`.
    fn count(img: &ColorImage, c: Color32) -> usize {
        img.pixels.iter().filter(|&&p| p == c).count()
    }

    #[test]
    fn geist_is_embedded_and_lays_out_lines() {
        assert!(Text::font().is_some(), "the embedded font parses");
        let Mark::Text(one) = text("Hi", Color32::RED, true) else { unreachable!() };
        let two = Text { text: "Hi\nthere".into(), ..one.clone() };
        assert!(one.line_height() > 60.0 && one.line_height() < 90.0, "line height {}", one.line_height());
        assert!(one.caret().x > one.pos.x + 40.0 && one.caret().y == one.pos.y, "caret after the letters: {:?}", one.caret());
        assert_eq!(two.caret().y, two.pos.y + two.line_height(), "Shift+Enter starts a second line");
        assert!(two.bounds().height() > 1.8 * one.line_height() - 20.0);
        let blank = Text { text: "  ".into(), ..one.clone() };
        assert_eq!(blank.bounds(), Rect::NOTHING, "spaces paint nothing");
        assert!(blank.caret().x > blank.pos.x, "but still move the caret");
    }

    #[test]
    fn text_burns_solid_letters_with_a_contrasting_halo() {
        let grey = Color32::from_rgb(128, 128, 128);
        let plain = ColorImage::new([200, 100], vec![grey; 20000]);
        let mut with = plain.clone();
        burn(&mut with, &text("Hi", Color32::RED, true));
        let mut without = plain.clone();
        burn(&mut without, &text("Hi", Color32::RED, false));
        assert!(count(&with, Color32::RED) > 300, "solid letters");
        assert_eq!(count(&with, Color32::RED), count(&without, Color32::RED), "the halo goes under the letters, never over them");
        assert!(count(&with, Color32::WHITE) > 100, "red text gets a white halo");
        assert_eq!(count(&without, Color32::WHITE), 0, "no halo when the outline is off");
        let mut light = plain.clone();
        burn(&mut light, &text("Hi", Color32::WHITE, true));
        assert!(count(&light, Color32::from_rgb(0x1f, 0x1f, 0x1f)) > 100, "white text gets a dark halo");
        assert_eq!(with.pixels[95 * 200 + 190], grey, "away from the text nothing changes");
    }

    #[test]
    fn text_is_erased_by_touching_it_and_moves_with_a_crop() {
        let mut d = Doc::new(white(200, 100));
        d.add_mark(text("Hi", Color32::RED, true));
        assert!(!d.hits(Pos2::new(190.0, 95.0), 2.0));
        d.crop(Rect::from_min_max(Pos2::new(5.0, 2.0), Pos2::new(200.0, 100.0)));
        assert!(matches!(&*d.marks[0], Mark::Text(t) if t.pos == Pos2::new(5.0, 3.0)));
        let c = d.marks[0].bounds().center();
        assert!(d.erase_at(c, 2.0));
        assert!(d.marks.is_empty());
    }

    #[test]
    fn tiles_merge_into_runs_clipped_to_the_image() {
        let mut t = Tiles::new([70, 40]);
        assert!(t.is_empty());
        t.add(Rect::from_min_max(Pos2::new(1.0, 1.0), Pos2::new(65.0, 5.0)));
        assert_eq!(t.runs(), vec![PxRect { x0: 0, y0: 0, x1: 70, y1: 32 }]);
        t.add(Rect::from_min_max(Pos2::new(-10.0, 33.0), Pos2::new(2.0, 90.0)));
        assert_eq!(t.runs()[1], PxRect { x0: 0, y0: 32, x1: 32, y1: 40 });
        let mut away = Tiles::new([70, 40]);
        away.add(Rect::from_min_max(Pos2::new(80.0, 0.0), Pos2::new(90.0, 5.0)));
        away.add(Rect::NOTHING);
        assert!(away.is_empty(), "nothing outside the image or empty");
        assert_eq!(t.minus(&t), Tiles::new([70, 40]));
    }

    #[test]
    fn a_long_diagonal_claims_only_the_tiles_along_it() {
        let mut t = Tiles::new([3200, 3200]);
        t.add_segment(Pos2::new(0.0, 0.0), Pos2::new(3200.0, 3200.0), 3.0);
        let on = t.on.iter().filter(|&&b| b).count();
        assert!(on < 400, "{on} of 10000 tiles");
    }

    #[test]
    fn ink_changes_bump_ink_rev() {
        let mut d = Doc::new(white(40, 40));
        d.add_mark(zigzag());
        assert_eq!(d.ink_rev, 1);
        assert!(!d.erase_at(Pos2::new(20.0, 2.0), 0.5));
        assert_eq!(d.ink_rev, 1);
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(20.0, 20.0), 1.0));
        assert_eq!(d.ink_rev, 2);
        d.undo();
        assert_eq!(d.ink_rev, 3);
        assert_eq!(d.marks.len(), 1);
    }

    #[test]
    fn eraser_removes_touched_stroke_only() {
        let mut d = Doc::new(white(50, 50));
        for y in [10.0, 40.0] {
            d.marks.push(Arc::new(Mark::Stroke(Stroke {
                ink: Ink::Highlighter,
                color: Color32::YELLOW,
                width: 4.0,
                points: vec![Pos2::new(5.0, y), Pos2::new(45.0, y)],
            })));
        }
        assert!(d.erase_at(Pos2::new(25.0, 12.0), 2.0));
        assert_eq!(d.marks.len(), 1);
        assert_eq!(stroke(&d.marks[0]).points[0].y, 40.0);
    }

    #[test]
    fn freeform_clears_outside() {
        let img = white(40, 40);
        let tri = [Pos2::new(0.0, 0.0), Pos2::new(40.0, 0.0), Pos2::new(0.0, 40.0)];
        let out = cut_freeform(&img, &tri).unwrap();
        assert_eq!(out.size, [40, 40]);
        assert_eq!(out.pixels[2 * 40 + 2], Color32::WHITE);
        assert_eq!(out.pixels[38 * 40 + 38], Color32::TRANSPARENT);
    }

    #[test]
    fn freeform_rejects_a_straight_line() {
        let line: Vec<Pos2> = (0..20).map(|i| Pos2::new(i as f32 * 2.0, i as f32)).collect();
        assert!(cut_freeform(&white(40, 40), &line).is_none());
    }
}
