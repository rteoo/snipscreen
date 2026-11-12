//! Snip document: picture, marks (ink strokes), crop and undo history.
//!
//! Every mark is rasterised on the CPU by `Mark::paint`, tile run by tile run. The editor shows a
//! mark being drawn by painting the same runs over the picture it will be burned into, so what is
//! on screen is exactly what gets copied or saved, blend modes included.

use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use eframe::egui::{Color32, ColorImage, Pos2, Rect};
use skrifa::MetadataProvider as _;
use vello_cpu::kurbo;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ink {
    Pen,
    /// Multiplies into the picture like a real highlighter instead of a see-through coat laid on top:
    /// white and light text turn the ink's colour, dark text stays dark, and a dark page only takes a
    /// faint tint, so what is highlighted is the content, not a band over it.
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
    /// A rectangle or ellipse filled with `color` and edged with a thin line in a contrasting colour.
    /// Ignored by lines and arrows.
    pub fill: bool,
}

impl Shape {
    fn filled(&self) -> bool {
        self.fill && matches!(self.kind, ShapeKind::Rectangle | ShapeKind::Ellipse)
    }

    /// Half the width of the line drawn: the shape's own, or a filled shape's thin edge.
    pub fn radius(&self) -> f32 {
        if self.filled() {
            (self.width * 0.15).clamp(0.75, 1.5)
        } else {
            (self.width * 0.5).max(0.5)
        }
    }

    /// The colour of the line: the shape's own, or the edge's, which stands out from the fill.
    fn line_color(&self) -> Color32 {
        if self.filled() { contrast(self.color) } else { self.color }
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
        if self.filled() {
            tiles.add(Rect::from_two_pos(self.from, self.to).expand(reach));
            return;
        }
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
        let bounds = Rect::from_two_pos(self.from, self.to);
        if self.filled() {
            match self.kind {
                ShapeKind::Rectangle => cover_box(&mut cover, r, bounds),
                _ => cover_ellipse(&mut cover, r, bounds),
            }
            composite(patch, &mut cover, self.color);
        }
        match self.kind {
            ShapeKind::Rectangle => cover_box_outline(&mut cover, r, bounds, self.radius()),
            _ => cover_segments(&mut cover, r, segments.iter().copied(), self.radius()),
        }
        if let Some(head) = head {
            cover_triangle(&mut cover, r, head);
        }
        composite(patch, &mut cover, self.line_color());
    }

    /// Adds the tiles that the fill paints in its colour alone, with nothing of the edge in them:
    /// every pixel there comes out `color`, whatever is under it. The colour, or `None` for a shape
    /// that fills nothing.
    fn solid(&self, tiles: &mut Tiles) -> Option<Color32> {
        if !self.filled() {
            return None;
        }
        let r = Rect::from_two_pos(self.from, self.to);
        // Past the edge line's reach and its antialiasing, with room to spare.
        let margin = self.radius() + 3.0;
        let (c, a, b) = (r.center(), r.width() / 2.0, r.height() / 2.0);
        // The ellipse scaled about its centre by `s` keeps a ball of radius `(1 - s) * min(a, b)` round
        // each of its points inside the ellipse, so this `s` leaves `margin` to the edge, where the
        // fill's approximate edge distance is under -0.5 too (full coverage).
        let s = 1.0 - margin / a.min(b).max(1e-3);
        let inner = r.shrink(margin);
        let inside = |p: Pos2| match self.kind {
            ShapeKind::Rectangle => inner.contains(p),
            _ => {
                let (qx, qy) = ((p.x - c.x) / a, (p.y - c.y) / b);
                s > 0.0 && qx * qx + qy * qy <= s * s
            }
        };
        let t = TILE as f32;
        for idx in tiles_in(r, tiles.size) {
            let (x0, y0) = ((idx % tiles.cols * TILE) as f32, (idx / tiles.cols * TILE) as f32);
            // Both shapes are convex: a tile whose corners are inside is inside.
            if [(x0, y0), (x0 + t, y0), (x0, y0 + t), (x0 + t, y0 + t)].into_iter().all(|(x, y)| inside(Pos2::new(x, y))) {
                tiles.on[idx] = true;
            }
        }
        Some(self.color)
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        let reach = self.radius() + radius;
        let r = Rect::from_two_pos(self.from, self.to);
        let inside = match self.kind {
            ShapeKind::Rectangle => r.contains(p),
            _ => ellipse_distance(p, r) <= 0.0,
        };
        (self.filled() && inside)
            || self.segments().into_iter().any(|(a, b)| dist_to_segment(p, a, b) <= reach)
            || (self.kind == ShapeKind::Arrow && triangle_distance(p, self.arrow().1) <= radius)
    }
}

/// Opaque `ink` over `patch` at the coverage in `cover`, which is left cleared for the next layer.
fn composite(patch: &mut ColorImage, cover: &mut [f32], ink: Color32) {
    for (px, k) in patch.pixels.iter_mut().zip(cover.iter_mut()) {
        if *k > 0.0 {
            *px = over(*px, ink, *k);
            *k = 0.0;
        }
    }
}

/// White against dark or colourful ink, near-black against light ink.
pub fn contrast(color: Color32) -> Color32 {
    let [r, g, b, _] = color.to_array().map(|v| v as f32 / 255.0);
    if 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.5 {
        Color32::from_rgb(0x1f, 0x1f, 0x1f)
    } else {
        Color32::WHITE
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
        contrast(self.color)
    }

    /// Lays the text out from `from` (the start, or a checkpoint of an earlier layout of a text
    /// with the same letters up to there) to byte `upto`: tells `step` of every character the font
    /// has and of a checkpoint every `CHECKPOINT_CHARS` characters, then returns the end of the last
    /// line (the caret's spot, on the top of that line). A character the font lacks (an emoji, CJK)
    /// is skipped and takes no room, so nothing draws a box for it. Resuming from a checkpoint gives
    /// exactly the positions a whole layout gives.
    fn lay_out(&self, font: &skrifa::FontRef, from: Pen, upto: usize, mut step: impl FnMut(Step)) -> Pos2 {
        let lm = self.line_metrics(font);
        let charmap = font.charmap();
        let advances = font.glyph_metrics(skrifa::instance::Size::new(self.size), skrifa::instance::LocationRef::default());
        let mut pen = from;
        for (i, ch) in self.text[from.byte..upto].char_indices() {
            if pen.chars.is_multiple_of(CHECKPOINT_CHARS) && pen.chars > from.chars {
                step(Step::Mark(Pen { byte: from.byte + i, ..pen }));
            }
            pen.chars += 1;
            if ch == '\n' {
                (pen.line, pen.x) = (pen.line + 1, self.pos.x);
            } else if let Some(id) = charmap.map(ch) {
                step(Step::Glyph(id, pen.x, self.pos.y + pen.line as f32 * lm.height + lm.ascent));
                pen.x += advances.advance_width(id).unwrap_or(0.0);
            }
        }
        Pos2::new(pen.x, self.pos.y + pen.line as f32 * lm.height)
    }

    /// Where the caret goes: after the last letter, at the top of its line.
    pub fn caret(&self) -> Pos2 {
        Self::font().map_or(self.pos, |f| self.lay_out(&f, Pen::start(self), self.text.len(), |_| {}))
    }

    /// Every glyph that draws something, flattened and placed, in reading order.
    fn placed(&self) -> Vec<Placed> {
        let mut layout = Layout::default();
        self.place(Pen::start(self), &mut layout);
        layout.glyphs
    }

    /// Lays the text out from `from` onto `layout`, which holds what comes before it (nothing, from
    /// the start): its glyphs that draw something, its checkpoints, their bounds and the caret.
    fn place(&self, from: Pen, layout: &mut Layout) {
        let Some(font) = Self::font() else {
            layout.end = self.pos;
            return;
        };
        let halo = self.halo();
        let mut cache = GLYPHS.lock().unwrap_or_else(PoisonError::into_inner);
        let Layout { glyphs, marks, bounds, end } = layout;
        *end = self.lay_out(&font, from, self.text.len(), |step| match step {
            Step::Mark(pen) => marks.push(Checkpoint { pen, glyphs: glyphs.len(), bounds: *bounds }),
            // Far past any picture (a long pasted line at a big size): left out, as it can't show,
            // and before its 1/256 px origin outgrows an i32.
            Step::Glyph(_, x, baseline) if x.abs() > MAX_ORIGIN || baseline.abs() > MAX_ORIGIN => {}
            Step::Glyph(id, x, baseline) => {
                let edges = cache.get(&font, id, self.size, halo);
                if edges.bbox[0] <= edges.bbox[2] {
                    let g = Placed { edges, at: [fixed(x), fixed(baseline)] };
                    *bounds = bounds.union(g.rect());
                    glyphs.push(g);
                }
            }
        });
    }

    /// Image pixels the text can touch, its halo included.
    fn bounds(&self) -> Rect {
        placed_bounds(&self.placed())
    }
}

/// What `Text::lay_out` comes to: a glyph `(id, x, baseline)`, or a checkpoint.
enum Step {
    Glyph(skrifa::GlyphId, f32, f32),
    Mark(Pen),
}

/// Characters between layout checkpoints: how far back an edit has to start laying out again.
const CHECKPOINT_CHARS: usize = 256;

/// Where a layout stands before the character at `byte`: its index, line, and pen position.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Pen {
    byte: usize,
    chars: usize,
    line: usize,
    x: f32,
}

impl Pen {
    fn start(t: &Text) -> Self {
        Self { byte: 0, chars: 0, line: 0, x: t.pos.x }
    }
}

/// A layout's state at a checkpoint: the pen, and the glyphs placed before it and their bounds.
#[derive(Clone, Copy, Debug)]
struct Checkpoint {
    pen: Pen,
    glyphs: usize,
    bounds: Rect,
}

/// A text laid out: the glyphs that draw something, checkpoints to resume from, the glyphs' bounds,
/// and the end of the text (the caret).
struct Layout {
    glyphs: Vec<Placed>,
    marks: Vec<Checkpoint>,
    bounds: Rect,
    end: Pos2,
}

impl Default for Layout {
    fn default() -> Self {
        Self { glyphs: Vec::new(), marks: Vec::new(), bounds: Rect::NOTHING, end: Pos2::ZERO }
    }
}

/// Precision of glyph edges: 1/256 of a pixel, as FreeType keeps them.
const SUB: i32 = 256;
/// Glyphs placed farther than this many pixels from the picture's corner are dropped. In 1/256 px
/// it is half of what an i32 holds, which leaves room for the glyph's own edges (well under 1M px
/// even at the largest text size and zoom).
const MAX_ORIGIN: f32 = (1 << 22) as f32;
/// How far a flattened curve may stray from the true one, in pixels (vello's tolerance, which drew text before).
const FLATNESS: f64 = 0.25;

fn fixed(v: f32) -> i32 {
    (v * SUB as f32).round() as i32
}

/// A straight piece of a closed outline, `[x0, y0, x1, y1]` in 1/256 px.
type Edge = [i32; 4];

/// One glyph at one size, flattened: its outline and its halo's, as edges in 1/256 px from the
/// glyph's origin on the baseline (y down).
struct GlyphEdges {
    fill: Vec<Edge>,
    halo: Vec<Edge>,
    /// Extent of every edge, `[x0, y0, x1, y1]`; `x0 > x1` for a glyph that draws nothing.
    bbox: [i32; 4],
}

impl GlyphEdges {
    fn new(font: &skrifa::FontRef, id: skrifa::GlyphId, size: f32, halo: f32) -> Self {
        let mut path = kurbo::BezPath::new();
        if let Some(g) = font.outline_glyphs().get(id) {
            let settings = skrifa::outline::DrawSettings::unhinted(skrifa::instance::Size::new(size), skrifa::instance::LocationRef::default());
            // A glyph that fails to draw is left out rather than failing the whole text.
            let _ = g.draw(settings, &mut GlyphPen { path: &mut path, x: 0.0, y: 0.0 });
        }
        let fill = edges_of(&path);
        let halo = if halo > 0.0 && !fill.is_empty() {
            let style = kurbo::Stroke::new(halo as f64).with_join(kurbo::Join::Round);
            edges_of(&kurbo::stroke(path.iter(), &style, &kurbo::StrokeOpts::default(), FLATNESS))
        } else {
            Vec::new()
        };
        let mut bbox = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        for e in fill.iter().chain(&halo) {
            bbox = [bbox[0].min(e[0]).min(e[2]), bbox[1].min(e[1]).min(e[3]), bbox[2].max(e[0]).max(e[2]), bbox[3].max(e[1]).max(e[3])];
        }
        Self { fill, halo, bbox }
    }

    fn len(&self) -> usize {
        self.fill.len() + self.halo.len()
    }
}

/// `path` flattened into closed loops of edges in 1/256 px; an open subpath is closed, as a fill closes it.
fn edges_of(path: &kurbo::BezPath) -> Vec<Edge> {
    let mut els = Vec::new();
    kurbo::flatten(path.iter(), FLATNESS, |el| els.push(el));
    let q = |p: kurbo::Point| [(p.x * SUB as f64).round() as i32, (p.y * SUB as f64).round() as i32];
    let mut out = Vec::new();
    let (mut start, mut last) = ([0, 0], [0, 0]);
    fn line(out: &mut Vec<Edge>, a: [i32; 2], b: [i32; 2]) {
        if a != b {
            out.push([a[0], a[1], b[0], b[1]]);
        }
    }
    for el in els {
        match el {
            kurbo::PathEl::MoveTo(p) => {
                line(&mut out, last, start);
                (start, last) = (q(p), q(p));
            }
            kurbo::PathEl::LineTo(p) => {
                line(&mut out, last, q(p));
                last = q(p);
            }
            kurbo::PathEl::ClosePath => {
                line(&mut out, last, start);
                last = start;
            }
            // `flatten` hands out lines only.
            _ => {}
        }
    }
    line(&mut out, last, start);
    out
}

/// Flattened glyphs by (glyph, size, halo width), shared by every text.
struct GlyphCache {
    map: std::collections::HashMap<(u32, u32, u32), Arc<GlyphEdges>>,
    edges: usize,
}

/// Edges kept before the glyph cache starts over (16 bytes each).
// ceiling: the whole cache is dropped past this (dragging the size slider fills it with sizes used
// once); keep an LRU instead if re-flattening after a reset ever shows up in a profile.
const GLYPH_CACHE_EDGES: usize = 1 << 20;

static GLYPHS: LazyLock<Mutex<GlyphCache>> = LazyLock::new(|| Mutex::new(GlyphCache { map: Default::default(), edges: 0 }));

impl GlyphCache {
    fn get(&mut self, font: &skrifa::FontRef, id: skrifa::GlyphId, size: f32, halo: f32) -> Arc<GlyphEdges> {
        let key = (id.to_u32(), size.to_bits(), halo.to_bits());
        if let Some(g) = self.map.get(&key) {
            return g.clone();
        }
        let g = Arc::new(GlyphEdges::new(font, id, size, halo));
        if self.edges + g.len() > GLYPH_CACHE_EDGES {
            self.map.clear();
            self.edges = 0;
        }
        self.edges += g.len();
        self.map.insert(key, g.clone());
        g
    }
}

/// A glyph placed in the text: its edges move by `at` (1/256 px).
#[derive(Clone)]
struct Placed {
    edges: Arc<GlyphEdges>,
    at: [i32; 2],
}

impl Placed {
    fn bbox(&self) -> [i32; 4] {
        let [x0, y0, x1, y1] = self.edges.bbox;
        [x0 + self.at[0], y0 + self.at[1], x1 + self.at[0], y1 + self.at[1]]
    }

    /// The image pixels the glyph's edges reach.
    fn rect(&self) -> Rect {
        let [x0, y0, x1, y1] = self.bbox();
        let (floor, ceil) = (|v: i32| v.div_euclid(SUB) as f32, |v: i32| -(-v).div_euclid(SUB) as f32);
        Rect::from_min_max(Pos2::new(floor(x0), floor(y0)), Pos2::new(ceil(x1), ceil(y1)))
    }

    fn same(&self, o: &Placed) -> bool {
        Arc::ptr_eq(&self.edges, &o.edges) && self.at == o.at
    }
}

fn placed_bounds(glyphs: &[Placed]) -> Rect {
    glyphs.iter().fold(Rect::NOTHING, |b, g| b.union(g.rect()))
}

/// Antialiased coverage of closed outlines over the pixels of `r`: the area each pixel's signed
/// winding covers, clamped to full. That is exact nonzero filling wherever no two loops overlap
/// inside a pixel, as with a glyph's own outline. A stroked halo's loops do overlap (at joins and
/// tight curves), and a pixel only partly covered there counts the overlap twice, so it can come
/// out a little more opaque than true nonzero; fully covered pixels are still exact. Each edge adds
/// whole-number cover and area to the pixel cells it crosses, worked out from its own end points
/// alone; so a pixel's value never depends on which rectangle is being filled or in what order
/// edges arrive, and any tile can be rendered on its own, bit for bit as in a whole render.
struct Coverage {
    r: PxRect,
    /// `[cover, area]` per pixel of `r`, FreeType style.
    cells: Vec<[i32; 2]>,
    /// Cover of the cells left of `r`, per row.
    carry: Vec<i32>,
}

impl Coverage {
    fn new(r: PxRect) -> Self {
        let (w, h) = (r.x1 - r.x0, r.y1 - r.y0);
        Self { r, cells: vec![[0, 0]; w * h], carry: vec![0; h] }
    }

    /// Adds the edge `[x0, y0, x1, y1]` moved by `at`.
    fn add(&mut self, e: &Edge, at: [i32; 2]) {
        let (x0, y0, x1, y1) = (e[0] + at[0], e[1] + at[1], e[2] + at[0], e[3] + at[1]);
        if y0 == y1 {
            return;
        }
        let (dir, xa, ya, xb, yb) = if y0 < y1 { (1, x0, y0, x1, y1) } else { (-1, x1, y1, x0, y0) };
        let (ys, ye) = (ya.max(self.r.y0 as i32 * SUB), yb.min(self.r.y1 as i32 * SUB));
        let x_at = |y: i32| xa + ((y - ya) as i64 * (xb - xa) as i64).div_euclid((yb - ya) as i64) as i32;
        let mut y = ys;
        while y < ye {
            let next = ((y.div_euclid(SUB) + 1) * SUB).min(ye);
            self.row_piece(dir, x_at(y), y, x_at(next), next);
            y = next;
        }
    }

    /// One edge's piece inside a single pixel row, from `(xs, ys)` down to `(xe, ye)`.
    fn row_piece(&mut self, dir: i32, xs: i32, ys: i32, xe: i32, ye: i32) {
        let py = ys.div_euclid(SUB);
        let row = py as usize - self.r.y0;
        let (cs, ce) = (xs.div_euclid(SUB), xe.div_euclid(SUB));
        if cs == ce {
            self.cell(row, cs, dir * (ye - ys), dir * (ye - ys) * (xs + xe - 2 * cs * SUB));
            return;
        }
        let (dx, dy) = (xe - xs, ye - ys);
        let step = if dx > 0 { 1 } else { -1 };
        let (mut cx, mut px, mut pyy) = (cs, xs, ys);
        while cx != ce {
            let bx = if step > 0 { (cx + 1) * SUB } else { cx * SUB };
            let by = ys + ((bx - xs) as i64 * dy as i64).div_euclid(dx as i64) as i32;
            self.cell(row, cx, dir * (by - pyy), dir * (by - pyy) * (px + bx - 2 * cx * SUB));
            (px, pyy, cx) = (bx, by, cx + step);
        }
        self.cell(row, ce, dir * (ye - pyy), dir * (ye - pyy) * (px + xe - 2 * ce * SUB));
    }

    fn cell(&mut self, row: usize, cx: i32, cover: i32, area: i32) {
        let (x0, x1) = (self.r.x0 as i32, self.r.x1 as i32);
        if cx < x0 {
            self.carry[row] += cover;
        } else if cx < x1 {
            let c = &mut self.cells[row * (self.r.x1 - self.r.x0) + (cx - x0) as usize];
            c[0] += cover;
            c[1] += area;
        }
    }

    /// The coverage as one byte per pixel of `r`.
    fn alphas(&self) -> Vec<u8> {
        let w = (self.r.x1 - self.r.x0).max(1);
        // Full coverage is `SUB * SUB * 2`: cover in 1/256 px rows, times twice the x in 1/256 px.
        const FULL: i32 = SUB * SUB * 2;
        let mut out = vec![0u8; self.cells.len()];
        for ((cells, out), &carry) in self.cells.chunks(w).zip(out.chunks_mut(w)).zip(&self.carry) {
            let mut acc = carry;
            for (&[c, a], o) in cells.iter().zip(out) {
                let n = (acc + c) * (2 * SUB) - a;
                acc += c;
                *o = ((n.unsigned_abs().min(FULL as u32) * 255 + FULL as u32 / 2) / FULL as u32) as u8;
            }
        }
        out
    }
}

/// One tile of a text: its letters' and halo's coverage, one byte per pixel of `r`.
struct TextTile {
    r: PxRect,
    fill: Vec<u8>,
    halo: Vec<u8>,
}

/// A text ready to paint: its glyphs placed, and their coverage worked out tile by tile as the tiles
/// are first painted. `Coverage` gives a pixel the same value whatever rectangle it is rendered in,
/// so a tile looks the same whichever paint asked for it first, live or saved, and editing the text
/// redoes only the tiles its changed letters reach.
pub struct TextRaster {
    text: Text,
    layout: Layout,
    size: [usize; 2],
    /// Rendered tiles by index (`None`: nothing of the text there).
    tiles: std::collections::HashMap<usize, Option<TextTile>>,
}

impl TextRaster {
    pub fn new(t: &Text, size: [usize; 2]) -> Self {
        let mut layout = Layout::default();
        t.place(Pen::start(t), &mut layout);
        Self { text: t.clone(), layout, size, tiles: Default::default() }
    }

    pub fn text(&self) -> &Text {
        &self.text
    }

    /// Image pixels the text can touch.
    pub fn bounds(&self) -> Rect {
        self.layout.bounds
    }

    /// Where the caret goes after the whole text, as `Text::caret` puts it, without laying it out again.
    pub fn caret(&self) -> Pos2 {
        self.layout.end
    }

    /// Where the caret goes after the text's first `byte` bytes (a char boundary): laid out from the
    /// checkpoint before it.
    pub fn caret_at(&self, byte: usize) -> Pos2 {
        let t = &self.text;
        let from = self.layout.marks.iter().rev().find(|m| m.pen.byte <= byte).map_or(Pen::start(t), |m| m.pen);
        Text::font().map_or(t.pos, |f| t.lay_out(&f, from, byte, |_| {}))
    }

    /// Renders tiles `tx0..tx1` of tile row `ty` in one pass (every glyph's edges are walked once
    /// for the whole span, not once per tile) and stores each of them.
    fn render_span(&mut self, ty: usize, tx0: usize, tx1: usize) {
        let r = PxRect { x0: tx0 * TILE, y0: ty * TILE, x1: (tx1 * TILE).min(self.size[0]), y1: ((ty + 1) * TILE).min(self.size[1]) };
        let (x0, y0, x1, y1) = (r.x0 as i32 * SUB, r.y0 as i32 * SUB, r.x1 as i32 * SUB, r.y1 as i32 * SUB);
        let mut fill = Coverage::new(r);
        let mut halo = self.text.outline.then(|| Coverage::new(r));
        for g in &self.layout.glyphs {
            let b = g.bbox();
            // A glyph wholly left of the span adds no cover to it (its loops are closed), nor does one
            // wholly above, below or right of it.
            if b[2] <= x0 || b[0] >= x1 || b[3] <= y0 || b[1] >= y1 {
                continue;
            }
            for e in &g.edges.fill {
                fill.add(e, g.at);
            }
            if let Some(h) = halo.as_mut() {
                for e in &g.edges.halo {
                    h.add(e, g.at);
                }
            }
        }
        let (fill, halo) = (fill.alphas(), halo.map_or_else(Vec::new, |h| h.alphas()));
        let cols = self.size[0].div_ceil(TILE);
        let w = r.x1 - r.x0;
        for tx in tx0..tx1 {
            let t = PxRect { x0: tx * TILE, x1: ((tx + 1) * TILE).min(self.size[0]), ..r };
            let cut = |v: &[u8]| -> Vec<u8> {
                let mut out = Vec::with_capacity((t.x1 - t.x0) * (t.y1 - t.y0));
                for row in v.chunks(w) {
                    out.extend_from_slice(&row[t.x0 - r.x0..t.x1 - r.x0]);
                }
                out
            };
            let tile_fill = cut(&fill);
            let tile_halo = if halo.is_empty() { Vec::new() } else { cut(&halo) };
            let blank = tile_fill.iter().chain(&tile_halo).all(|&k| k == 0);
            self.tiles.insert(ty * cols + tx, (!blank).then_some(TextTile { r: t, fill: tile_fill, halo: tile_halo }));
        }
    }

    /// Composites the text onto `patch` (top-left image pixel `at`): the halo first, the letters over it.
    pub fn paint(&mut self, patch: &mut ColorImage, at: [usize; 2]) {
        let r = patch_rect(patch, at);
        let reach = self.layout.bounds.intersect(r.rect());
        let cols = self.size[0].div_ceil(TILE);
        // Tiles not rendered yet, rendered a row at a time.
        let mut missing: Vec<usize> = tiles_in(reach, self.size).filter(|i| !self.tiles.contains_key(i)).collect();
        missing.sort_unstable();
        for row in missing.chunk_by(|a, b| a / cols == b / cols) {
            let (first, last) = (row[0], row[row.len() - 1]);
            self.render_span(first / cols, first % cols, last % cols + 1);
        }
        let (color, halo) = (self.text.color, self.text.halo_color());
        let pw = patch.size[0];
        for idx in tiles_in(reach, self.size) {
            let Some(Some(t)) = self.tiles.get(&idx) else { continue };
            let (x0, x1) = (r.x0.max(t.r.x0), r.x1.min(t.r.x1));
            let (y0, y1) = (r.y0.max(t.r.y0), r.y1.min(t.r.y1));
            let tw = t.r.x1 - t.r.x0;
            for y in y0..y1 {
                let m = (y - t.r.y0) * tw;
                let (mx0, mx1) = (m + x0 - t.r.x0, m + x1 - t.r.x0);
                let row = &mut patch.pixels[(y - r.y0) * pw + x0 - r.x0..(y - r.y0) * pw + x1 - r.x0];
                if let Some(ks) = t.halo.get(mx0..mx1) {
                    for (px, &k) in row.iter_mut().zip(ks) {
                        *px = over8(*px, halo, k);
                    }
                }
                for (px, &k) in row.iter_mut().zip(&t.fill[mx0..mx1]) {
                    *px = over8(*px, color, k);
                }
            }
        }
    }

    /// Turns this into the raster of `new`, adding to `dirty` the tiles whose pixels can differ. While
    /// only letters at the end change (typing), only the tiles those letters reach are redone.
    pub fn retext(&mut self, new: &Text, dirty: &mut Tiles) {
        // ceiling: a new size redoes every tile of the text. The editor paints only those in view at
        // once (~13 ms per slider frame for a 12-line 160 px block at 100% in a 1280x800 view,
        // release), but at fit zoom all of a big block is in view (~45 ms); show a scaled preview
        // while the slider moves if dragging it there feels sticky.
        if (self.text.size, self.text.outline, self.text.pos) != (new.size, new.outline, new.pos) {
            dirty.add(self.layout.bounds);
            *self = TextRaster::new(new, self.size);
            dirty.add(self.layout.bounds);
            return;
        }
        if self.text.color != new.color {
            dirty.add(self.layout.bounds);
        }
        // Only what follows the last checkpoint before the first changed letter is laid out again,
        // so a keystroke after a long paste costs as much as one after a word.
        let (old, now) = (self.text.text.as_bytes(), new.text.as_bytes());
        let mut common = old.iter().zip(now).take_while(|(a, b)| a == b).count();
        while !new.text.is_char_boundary(common) {
            common -= 1;
        }
        let keep = self.layout.marks.partition_point(|m| m.pen.byte <= common);
        let (from, kept, bounds) = match keep.checked_sub(1).map(|i| self.layout.marks[i]) {
            Some(m) => (m.pen, m.glyphs, m.bounds),
            None => (Pen::start(new), 0, Rect::NOTHING),
        };
        self.layout.marks.truncate(keep);
        let before = self.layout.glyphs.split_off(kept);
        self.layout.bounds = bounds;
        new.place(from, &mut self.layout);
        let after = &self.layout.glyphs[kept..];
        let same = before.iter().zip(after).take_while(|(a, b)| a.same(b)).count();
        let mut changed = Tiles::new(self.size);
        for g in before[same..].iter().chain(&after[same..]) {
            changed.add(g.rect());
        }
        self.tiles.retain(|&idx, _| !changed.on[idx]);
        dirty.union(&changed);
        self.text = new.clone();
    }

    /// Whether `p` is within `radius` of one of the letters (their halo included).
    fn touches(&self, p: Pos2, radius: f32) -> bool {
        self.layout.bounds.expand(radius).contains(p) && self.layout.glyphs.iter().any(|g| g.rect().expand(radius).contains(p))
    }
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

    /// The mark ready to paint many patches of an image of `size`: its geometry is worked out once,
    /// not per tile run (a text's letters as each tile is first painted).
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
            Mark::Text(t) => r.text = Some(TextRaster::new(t, size)),
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
            Mark::Text(t) => t.placed().iter().any(|g| g.rect().expand(radius).contains(p)),
        }
    }

    pub fn translate(&mut self, d: eframe::egui::Vec2) {
        match self {
            Mark::Stroke(s) => s.points.iter_mut().for_each(|p| *p += d),
            Mark::Shape(s) => {
                s.from += d;
                s.to += d;
            }
            Mark::Text(t) => t.pos += d,
        }
    }

    /// The box the mark's geometry spans, in image pixels: a stroke's points, the two corners a shape
    /// was dragged between, a text's letters. What the editor's selection handles frame.
    pub fn frame(&self) -> Rect {
        match self {
            Mark::Stroke(s) => Rect::from_points(&s.points),
            Mark::Shape(s) => Rect::from_two_pos(s.from, s.to),
            Mark::Text(t) => t.bounds(),
        }
    }

    /// Stretches the mark from the box `from` (its `frame`) to `to`: every point keeps its place
    /// within the box. `to` may be turned inside out (min past max), which mirrors the mark. Along
    /// a side where `from` is flat (a straight horizontal stroke) the mark only moves with `to`'s
    /// min. A shape's corners take `to`'s sides as they are, so a box squashed flat widens again.
    /// Line widths stay as they are; a text keeps its top-left corner's place in the box and takes
    /// its size from the change in height, unless either height is flat.
    pub fn reframe(&mut self, from: Rect, to: Rect) {
        let axis = |v: f32, a0: f32, a1: f32, b0: f32, b1: f32| if (a1 - a0).abs() > 1e-3 { b0 + (v - a0) * (b1 - b0) / (a1 - a0) } else { v + (b0 - a0) };
        let map = |p: Pos2| Pos2::new(axis(p.x, from.min.x, from.max.x, to.min.x, to.max.x), axis(p.y, from.min.y, from.max.y, to.min.y, to.max.y));
        match self {
            Mark::Stroke(s) => s.points.iter_mut().for_each(|p| *p = map(*p)),
            Mark::Shape(s) => {
                // The corner on the min side (the first one, when they are level) takes `to`'s min.
                let sides = |a: f32, b: f32, lo: f32, hi: f32| if a <= b { (lo, hi) } else { (hi, lo) };
                (s.from.x, s.to.x) = sides(s.from.x, s.to.x, to.min.x, to.max.x);
                (s.from.y, s.to.y) = sides(s.from.y, s.to.y, to.min.y, to.max.y);
            }
            Mark::Text(t) => {
                t.pos = map(t.pos);
                if from.height() > 1e-3 && to.height().abs() > 1e-3 {
                    t.size *= (to.height() / from.height()).abs();
                }
            }
        }
    }

    pub fn color(&self) -> Color32 {
        match self {
            Mark::Stroke(s) => s.color,
            Mark::Shape(s) => s.color,
            Mark::Text(t) => t.color,
        }
    }
}

/// The tiles of an image of `size` that come out the same with `b` in `a`'s place in the stack:
/// those both fill solid in the same colour, where nothing under them shows. So moving or resizing
/// a filled box repaints its edges, not its whole area.
pub fn same_tiles(a: &Mark, b: &Mark, size: [usize; 2]) -> Tiles {
    let (mut ta, mut tb) = (Tiles::new(size), Tiles::new(size));
    match (a, b) {
        (Mark::Shape(a), Mark::Shape(b)) if a.solid(&mut ta).is_some_and(|c| b.solid(&mut tb) == Some(c)) => ta.intersect(&tb),
        _ => Tiles::new(size),
    }
}

/// A mark with its geometry worked out, ready to paint patch after patch.
pub struct Raster<'a> {
    mark: &'a Mark,
    segments: Vec<(Pos2, Pos2)>,
    head: Option<[Pos2; 3]>,
    text: Option<TextRaster>,
}

impl<'a> Raster<'a> {
    /// The raster of `mark`, a text, from its text raster.
    pub fn of_text(mark: &'a Mark, text: TextRaster) -> Self {
        Raster { mark, segments: Vec::new(), head: None, text: Some(text) }
    }

    /// The text raster inside, for a text mark.
    pub fn into_text(self) -> Option<TextRaster> {
        self.text
    }

    /// Composites the mark onto `patch`, a piece of the picture whose top-left pixel is `at`. The
    /// result for a pixel depends only on that pixel and the mark, never on where the patch ends,
    /// so painting tile runs one at a time gives the same pixels as painting the whole picture.
    pub fn paint(&mut self, patch: &mut ColorImage, at: [usize; 2]) {
        match self.mark {
            Mark::Stroke(s) => s.paint(patch, at),
            Mark::Shape(s) => s.paint(&self.segments, self.head, patch, at),
            Mark::Text(_) => {
                if let Some(text) = self.text.as_mut() {
                    text.paint(patch, at);
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

    /// Indices of the tiles in the set, row by row (`y * columns + x`).
    pub fn indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.on.iter().enumerate().filter_map(|(i, &on)| on.then_some(i))
    }

    /// Keeps only the tiles whose index passes `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(usize) -> bool) {
        for (i, on) in self.on.iter_mut().enumerate() {
            *on = *on && keep(i);
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

/// Indices of the tiles of an image of `size` that `r` (image pixels) overlaps, row by row.
fn tiles_in(r: Rect, size: [usize; 2]) -> impl Iterator<Item = usize> {
    let cols = size[0].div_ceil(TILE);
    let span = |lo: f32, hi: f32, lim: usize| {
        // Written so a NaN or empty rectangle gives nothing.
        if hi > lo && hi > 0.0 && lo < lim as f32 {
            (lo.max(0.0) as usize / TILE, (hi.ceil().min(lim as f32) as usize).div_ceil(TILE))
        } else {
            (0, 0)
        }
    };
    let ((tx0, tx1), (ty0, ty1)) = (span(r.min.x, r.max.x, size[0]), span(r.min.y, r.max.y, size[1]));
    (ty0..ty1).flat_map(move |ty| (tx0..tx1).map(move |tx| ty * cols + tx))
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

/// Raises `cover` to the exact area coverage of the filled rectangle `rect`.
fn cover_box(cover: &mut [f32], r: PxRect, rect: Rect) {
    let overlap = |lo: f32, hi: f32, p: f32| (hi.min(p + 1.0) - lo.max(p)).clamp(0.0, 1.0);
    let bw = r.x1 - r.x0;
    let (x0, x1) = ((rect.min.x.floor().max(0.0) as usize).max(r.x0), (rect.max.x.ceil().max(0.0) as usize).min(r.x1));
    let (y0, y1) = ((rect.min.y.floor().max(0.0) as usize).max(r.y0), (rect.max.y.ceil().max(0.0) as usize).min(r.y1));
    for y in y0..y1 {
        let cy = overlap(rect.min.y, rect.max.y, y as f32);
        for x in x0..x1 {
            let slot = &mut cover[(y - r.y0) * bw + (x - r.x0)];
            *slot = slot.max(cy * overlap(rect.min.x, rect.max.x, x as f32));
        }
    }
}

/// Approximate signed distance from `p` to the edge of the ellipse inscribed in `rect` (negative
/// inside): the implicit function over its gradient, exact on the edge, which is all antialiasing needs.
fn ellipse_distance(p: Pos2, rect: Rect) -> f32 {
    let (a, b) = ((rect.width() / 2.0).max(1e-3), (rect.height() / 2.0).max(1e-3));
    let d = p - rect.center();
    let (qx, qy) = (d.x / a, d.y / b);
    let grad = eframe::egui::vec2(qx / a, qy / b).length();
    if grad < 1e-6 {
        return -a.min(b);
    }
    (qx * qx + qy * qy - 1.0) / (2.0 * grad)
}

/// Raises `cover` to the antialiased coverage of the filled ellipse inscribed in `rect`.
fn cover_ellipse(cover: &mut [f32], r: PxRect, rect: Rect) {
    let bw = r.x1 - r.x0;
    let (x0, x1) = ((rect.min.x.floor().max(0.0) as usize).max(r.x0), (rect.max.x.ceil().max(0.0) as usize).min(r.x1));
    let (y0, y1) = ((rect.min.y.floor().max(0.0) as usize).max(r.y0), (rect.max.y.ceil().max(0.0) as usize).min(r.y1));
    for y in y0..y1 {
        for x in x0..x1 {
            let c = (0.5 - ellipse_distance(Pos2::new(x as f32 + 0.5, y as f32 + 0.5), rect)).clamp(0.0, 1.0);
            let slot = &mut cover[(y - r.y0) * bw + (x - r.x0)];
            *slot = slot.max(c);
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

/// Opaque `ink` over the premultiplied pixel `px` at 8-bit coverage `k`, rounded exactly.
fn over8(px: Color32, ink: Color32, k: u8) -> Color32 {
    match k {
        0 => px,
        255 => Color32::from_rgba_premultiplied(ink.r(), ink.g(), ink.b(), 255),
        _ => {
            let (k, j) = (k as u32, 255 - k as u32);
            let mix = |base: u8, ink: u8| ((base as u32 * j + ink as u32 * k + 127) / 255) as u8;
            let [r0, g0, b0, a0] = px.to_array();
            Color32::from_rgba_premultiplied(mix(r0, ink.r()), mix(g0, ink.g()), mix(b0, ink.b()), mix(a0, 255))
        }
    }
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
    /// Rendered text marks by identity, for the picture size `texts_size`: only marks in `marks`, as
    /// a text's tiles take megabytes and undo history can hold many. Each entry keeps its mark's
    /// allocation alive (`Weak`), so no other mark can take its address while the entry exists.
    texts: std::collections::HashMap<usize, (Weak<Mark>, TextRaster)>,
    texts_size: [usize; 2],
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
            texts: Default::default(),
            texts_size: [0, 0],
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

    /// Takes back every change since the last `checkpoint`, leaving nothing to redo: a cancelled edit.
    pub fn revert(&mut self) {
        if let Some(s) = self.undo.pop() {
            self.restore(s);
        }
    }

    /// Adds a finished mark as one undoable step.
    pub fn add_mark(&mut self, m: impl Into<Mark>) {
        self.checkpoint();
        self.marks.push(Arc::new(m.into()));
        self.ink_rev += 1;
    }

    /// Adds finished text whose raster is already worked out (the one that showed it while it was
    /// typed), so its tiles are never rendered twice.
    pub fn add_text(&mut self, raster: TextRaster) {
        self.add_mark(raster.text().clone());
        self.sync_text_cache();
        if let Some(m) = self.marks.last() {
            self.texts.insert(Arc::as_ptr(m) as usize, (Arc::downgrade(m), raster));
        }
    }

    /// Drops cached text rasters of marks the list doesn't hold now (an undo that brings one back
    /// renders it again), and all of them when the picture changed size (a raster's tiles are cut
    /// to the picture).
    fn sync_text_cache(&mut self) {
        if self.texts_size != self.size() {
            self.texts.clear();
            self.texts_size = self.size();
        }
        if self.texts.is_empty() {
            return;
        }
        let shown: std::collections::HashSet<usize> = self.marks.iter().filter(|m| matches!(***m, Mark::Text(_))).map(|m| Arc::as_ptr(m) as usize).collect();
        self.texts.retain(|key, _| shown.contains(key));
    }

    fn cached_text(&self, m: &Arc<Mark>) -> Option<&TextRaster> {
        self.texts.get(&(Arc::as_ptr(m) as usize)).filter(|_| self.texts_size == self.size()).map(|(_, r)| r)
    }

    fn touches(&self, m: &Arc<Mark>, p: Pos2, radius: f32) -> bool {
        match self.cached_text(m) {
            Some(r) => r.touches(p, radius),
            None => m.touches(p, radius),
        }
    }

    pub fn hits(&self, p: Pos2, radius: f32) -> bool {
        self.marks.iter().any(|m| self.touches(m, p, radius))
    }

    /// The topmost mark within `radius` of `p`, found as the eraser finds marks.
    pub fn mark_at(&self, p: Pos2, radius: f32) -> Option<usize> {
        self.marks.iter().rposition(|m| self.touches(m, p, radius))
    }

    /// Mark `i`'s `Mark::frame`, from its rendered raster for a text (no layout again).
    pub fn frame_of(&self, i: usize) -> Option<Rect> {
        let m = self.marks.get(i)?;
        Some(self.cached_text(m).map_or_else(|| m.frame(), |r| r.bounds()))
    }

    /// Puts `m` in place of mark `i`, in the same spot of the stack. It is a new mark to
    /// `changed_tiles`, so both the old and the new footprint repaint. Not an undo step by itself:
    /// call `checkpoint` once before an edit (a whole drag, one option change).
    pub fn replace_mark(&mut self, i: usize, m: Mark) {
        if let Some(slot) = self.marks.get_mut(i) {
            *slot = Arc::new(m);
            self.ink_rev += 1;
        }
    }

    /// Removes mark `i` as one undoable step.
    pub fn remove_mark(&mut self, i: usize) {
        if i < self.marks.len() {
            self.checkpoint();
            self.marks.remove(i);
            self.ink_rev += 1;
        }
    }

    /// Removes every mark within `radius` of `p`. Returns whether any went.
    pub fn erase_at(&mut self, p: Pos2, radius: f32) -> bool {
        let keep: Vec<bool> = self.marks.iter().map(|m| !self.touches(m, p, radius)).collect();
        if keep.iter().all(|&k| k) {
            return false;
        }
        let mut keep = keep.into_iter();
        self.marks.retain(|_| keep.next().unwrap_or(true));
        self.ink_rev += 1;
        true
    }

    /// Crops to `r` (image pixels); marks move with the picture, and those it leaves wholly outside go
    /// (undo brings them back with the picture).
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
        let picture = Rect::from_min_size(Pos2::ZERO, eframe::egui::vec2((x1 - x0) as f32, (y1 - y0) as f32));
        self.marks.retain(|m| m.bounds().intersects(picture));
    }

    /// Tiles whose pixels differ between the picture burned with `before` and with the current
    /// marks: the footprints of every mark in one list and not the other. Marks are compared by
    /// identity, so `before` must be an earlier version of `marks` on the same picture.
    /// A mark put in another's place (`replace_mark`) leaves out the tiles `same_tiles` finds.
    pub fn changed_tiles(&self, before: &[Arc<Mark>]) -> Tiles {
        let ids = |v: &[Arc<Mark>]| v.iter().map(Arc::as_ptr).collect::<std::collections::HashSet<_>>();
        let (was, now) = (ids(before), ids(&self.marks));
        let size = self.size();
        let footprint = |m: &Arc<Mark>, tiles: &mut Tiles| match self.cached_text(m) {
            Some(r) => tiles.add(r.bounds()),
            None => m.footprint(tiles),
        };
        let mut tiles = Tiles::new(size);
        let mut paired = std::collections::HashSet::new();
        if before.len() == self.marks.len() {
            for (old, new) in before.iter().zip(&self.marks) {
                if now.contains(&Arc::as_ptr(old)) || was.contains(&Arc::as_ptr(new)) {
                    continue;
                }
                let same = same_tiles(old, new, size);
                if !same.is_empty() {
                    let mut both = Tiles::new(size);
                    footprint(old, &mut both);
                    footprint(new, &mut both);
                    tiles.union(&both.minus(&same));
                    paired.extend([Arc::as_ptr(old), Arc::as_ptr(new)]);
                }
            }
        }
        let gone = before.iter().filter(|m| !now.contains(&Arc::as_ptr(m)));
        let added = self.marks.iter().filter(|m| !was.contains(&Arc::as_ptr(m)));
        for m in gone.chain(added).filter(|m| !paired.contains(&Arc::as_ptr(m))) {
            footprint(m, &mut tiles);
        }
        tiles
    }

    /// The flattened picture over each run of `tiles`, built from the bare picture and only the
    /// marks that reach that run: the same pixels `flatten` gives there. Texts keep their rendered
    /// tiles between calls, so an erase or undo beside them costs nothing for them.
    // ceiling: re-burns every mark that reaches a damaged run (an undo under 20 thick highlighter
    // strokes stacked on one path takes ~90 ms on 4K); keep per-mark layers if piled-up marks feel sticky.
    pub fn render_runs(&mut self, tiles: &Tiles) -> Vec<(PxRect, ColorImage)> {
        let runs = tiles.runs();
        if runs.is_empty() {
            return Vec::new();
        }
        self.sync_text_cache();
        let size = self.size();
        let reach = runs.iter().fold(Rect::NOTHING, |b, r| b.union(r.rect()));
        let hit = |b: Rect| b.intersects(reach) && runs.iter().any(|r| b.intersects(r.rect()));
        enum Layer<'a> {
            Plain(Box<Raster<'a>>),
            Text(usize),
        }
        let Doc { image, marks, texts, .. } = self;
        let mut layers = Vec::new();
        for m in marks.iter() {
            match &**m {
                Mark::Text(t) => {
                    let key = Arc::as_ptr(m) as usize;
                    let (_, raster) = texts.entry(key).or_insert_with(|| (Arc::downgrade(m), TextRaster::new(t, size)));
                    if hit(raster.bounds()) {
                        layers.push((raster.bounds(), Layer::Text(key)));
                    }
                }
                other => {
                    let b = other.bounds();
                    if hit(b) {
                        layers.push((b, Layer::Plain(Box::new(other.raster(size)))));
                    }
                }
            }
        }
        runs.into_iter()
            .map(|r| {
                let mut patch = sub_image(image, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
                for (b, layer) in &mut layers {
                    if b.intersects(r.rect()) {
                        match layer {
                            Layer::Plain(raster) => raster.paint(&mut patch, r.pos()),
                            Layer::Text(key) => {
                                if let Some((_, raster)) = texts.get_mut(key) {
                                    raster.paint(&mut patch, r.pos());
                                }
                            }
                        }
                    }
                }
                (r, patch)
            })
            .collect()
    }

    /// `flatten`, rendering texts through the cache, so the hit tests and frames after it find them
    /// laid out (a crop empties the cache).
    pub fn flatten_cached(&mut self) -> ColorImage {
        let mut out = (*self.image).clone();
        if self.marks.is_empty() {
            return out;
        }
        let mut all = Tiles::new(self.size());
        all.on.fill(true);
        for (r, patch) in self.render_runs(&all) {
            paste(&mut out, &patch, r.pos());
        }
        out
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

    pub fn rect(self) -> Rect {
        Rect::from_min_max(Pos2::new(self.x0 as f32, self.y0 as f32), Pos2::new(self.x1 as f32, self.y1 as f32))
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
    let mut raster = m.raster(img.size);
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
        Mark::Shape(Shape { kind, color: Color32::RED, width, from: Pos2::new(from.0, from.1), to: Pos2::new(to.0, to.1), fill: false })
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
    fn crop_drops_the_marks_it_leaves_outside_and_undo_brings_them_back() {
        let mut d = Doc::new(white(200, 100));
        d.add_mark(shape(ShapeKind::Line, 4.0, (150.0, 80.0), (190.0, 90.0)));
        d.add_mark(Text { text: "far".into(), color: Color32::RED, size: 20.0, outline: true, pos: Pos2::new(150.0, 10.0) });
        d.add_mark(shape(ShapeKind::Line, 4.0, (10.0, 10.0), (60.0, 20.0)));
        d.crop(Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(100.0, 100.0)));
        assert_eq!(d.marks.len(), 1, "only the mark inside stays");
        assert!(!d.hits(Pos2::new(170.0, 85.0), 30.0), "nothing invisible is left to hit");
        d.undo();
        assert_eq!(d.marks.len(), 3, "undo restores them");
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
    fn on_a_dark_page_the_highlighter_colours_light_text_and_only_tints_the_page() {
        // A dark page (30, 30, 34) with a light "text" column (220, 220, 220) in its right half.
        let mut page = ColorImage::new([40, 20], vec![Color32::from_rgb(30, 30, 34); 800]);
        for y in 0..20 {
            for x in 26..32 {
                page.pixels[y * 40 + x] = Color32::from_gray(220);
            }
        }
        let mut d = Doc::new(page);
        d.add_mark(Stroke { ink: Ink::Highlighter, color: Color32::from_rgb(255, 255, 0), width: 10.0, points: vec![Pos2::new(2.0, 10.0), Pos2::new(38.0, 10.0)] });
        let out = d.flatten();
        assert_eq!(out.pixels[10 * 40 + 28], Color32::from_rgb(220, 220, 0), "light text takes the colour");
        assert_eq!(out.pixels[10 * 40 + 8], Color32::from_rgb(30, 30, 0), "the page keeps its darkness");
        assert_eq!(out.pixels[2 * 40 + 8], Color32::from_rgb(30, 30, 34), "outside the stroke untouched");
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
    fn filled_shapes_take_the_ink_inside_a_thin_contrasting_edge() {
        let grey = Color32::from_gray(128);
        for kind in [ShapeKind::Rectangle, ShapeKind::Ellipse] {
            let filled = Mark::Shape(Shape { kind, color: Color32::RED, width: 8.0, from: Pos2::new(10.0, 10.0), to: Pos2::new(50.0, 50.0), fill: true });
            let mut img = ColorImage::new([60, 60], vec![grey; 3600]);
            burn(&mut img, &filled);
            let at = |x: usize, y: usize| img.pixels[y * 60 + x];
            for (x, y) in [(30, 30), (13, 30), (30, 46)] {
                assert_eq!(at(x, y), Color32::RED, "{kind:?} filled at ({x}, {y})");
            }
            // Red is dark enough for a white edge, and the edge stays thin whatever the size.
            assert_eq!(at(10, 30), Color32::WHITE, "{kind:?} edge");
            assert_eq!(at(7, 30), grey, "{kind:?} outside");
            let mut doc = Doc::new(ColorImage::new([60, 60], vec![grey; 3600]));
            doc.add_mark(filled.clone());
            assert!(doc.hits(Pos2::new(30.0, 30.0), 1.0), "{kind:?}: a filled shape is hit anywhere inside");
            let Mark::Shape(mut hollow) = filled else { unreachable!() };
            hollow.fill = false;
            let mut doc = Doc::new(ColorImage::new([60, 60], vec![grey; 3600]));
            doc.add_mark(hollow);
            assert!(!doc.hits(Pos2::new(30.0, 30.0), 1.0), "{kind:?}: an outline is only hit on its line");
        }
        // Lines and arrows ignore the fill.
        let mut a = white(60, 60);
        let mut b = white(60, 60);
        burn(&mut a, &shape(ShapeKind::Arrow, 4.0, (10.0, 10.0), (50.0, 50.0)));
        burn(&mut b, &Mark::Shape(Shape { kind: ShapeKind::Arrow, color: Color32::RED, width: 4.0, from: Pos2::new(10.0, 10.0), to: Pos2::new(50.0, 50.0), fill: true }));
        assert!(a.pixels == b.pixels);
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
        let mut check = |d: &mut Doc, what: &str| {
            let tiles = d.changed_tiles(&burned);
            for (r, patch) in d.render_runs(&tiles) {
                paste(&mut shown, &patch, r.pos());
            }
            burned = d.marks.clone();
            assert!(shown.pixels == d.flatten().pixels, "after {what}");
        };
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(100.0, 40.0), 2.0));
        check(&mut d, "erasing the rectangle");
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(60.0, 43.0), 1.0));
        check(&mut d, "erasing through a pile of marks");
        d.undo();
        check(&mut d, "undo");
        d.undo();
        check(&mut d, "a second undo");
        d.redo();
        check(&mut d, "redo");
        d.undo();
        d.undo();
        check(&mut d, "undoing an add");
        assert_eq!(d.changed_tiles(&d.marks.clone()), Tiles::new(d.size()), "nothing changed, nothing to redraw");
    }

    #[test]
    fn an_edited_mark_repaints_its_old_and_new_place_and_undo_brings_the_old_one_back() {
        let mut d = Doc::new(gradient(160, 120));
        d.add_mark(shape(ShapeKind::Rectangle, 4.0, (10.0, 10.0), (50.0, 40.0)));
        d.add_mark(Stroke { ink: Ink::Highlighter, color: Color32::from_rgb(92, 214, 255), width: 12.0, points: vec![Pos2::new(5.0, 30.0), Pos2::new(150.0, 35.0)] });
        d.add_mark(Text { text: "Hi".into(), color: Color32::BLACK, size: 30.0, outline: true, pos: Pos2::new(20.0, 60.0) });
        let mut shown = d.flatten();
        let mut burned = d.marks.clone();
        let mut check = |d: &mut Doc, what: &str| {
            let tiles = d.changed_tiles(&burned);
            for (r, patch) in d.render_runs(&tiles) {
                paste(&mut shown, &patch, r.pos());
            }
            burned = d.marks.clone();
            assert!(shown.pixels == d.flatten().pixels, "after {what}");
        };
        let original = (*d.marks[0]).clone();
        let mut moved = original.clone();
        moved.translate(eframe::egui::vec2(90.0, 60.0));
        let before = d.marks.clone();
        d.checkpoint();
        d.replace_mark(0, moved.clone());
        let tiles = d.changed_tiles(&before);
        let (mut old, mut new) = (Tiles::new(d.size()), Tiles::new(d.size()));
        original.footprint(&mut old);
        moved.footprint(&mut new);
        assert!(old.minus(&tiles).is_empty() && new.minus(&tiles).is_empty(), "both places repaint");
        assert!(Arc::ptr_eq(&d.marks[1], &before[1]) && Arc::ptr_eq(&d.marks[2], &before[2]), "the others are left alone");
        check(&mut d, "moving the rectangle under the highlighter");
        assert_eq!(d.marks.len(), 3, "it keeps its place in the stack");
        // Resizing the text from its rendered frame, then recolouring it, each its own step.
        let frame = d.frame_of(2).unwrap();
        assert_eq!(frame, d.marks[2].frame(), "the cached raster frames the text as its layout does");
        let mut bigger = (*d.marks[2]).clone();
        bigger.reframe(frame, Rect::from_min_size(frame.min, frame.size() * 1.5));
        assert!(matches!(&bigger, Mark::Text(t) if (t.size - 45.0).abs() < 1e-3), "{bigger:?}");
        d.checkpoint();
        d.replace_mark(2, bigger);
        check(&mut d, "resizing the text");
        let Mark::Text(t) = &*d.marks[2] else { panic!("text") };
        let red = Text { color: Color32::RED, ..t.clone() };
        d.checkpoint();
        d.replace_mark(2, red.clone().into());
        check(&mut d, "recolouring the text");
        d.undo();
        check(&mut d, "undoing the colour");
        assert!(matches!(&*d.marks[2], Mark::Text(t) if t.color == Color32::BLACK && t.size > 40.0));
        d.undo();
        check(&mut d, "undoing the resize");
        d.undo();
        check(&mut d, "undoing the move");
        assert_eq!(*d.marks[0], original);
        d.redo();
        check(&mut d, "redoing the move");
        assert_eq!(*d.marks[0], moved);
        d.remove_mark(1);
        check(&mut d, "removing the highlighter");
        assert_eq!(d.marks.len(), 2);
        d.undo();
        check(&mut d, "undoing the removal");
        assert_eq!(d.marks.len(), 3);
    }

    #[test]
    fn moving_or_resizing_a_filled_box_repaints_its_edges_not_its_middle() {
        let mut d = Doc::new(gradient(640, 480));
        d.add_mark(Stroke { ink: Ink::Highlighter, color: Color32::from_rgb(92, 214, 255), width: 30.0, points: vec![Pos2::new(5.0, 200.0), Pos2::new(630.0, 260.0)] });
        let boxed = |kind, from: (f32, f32), to: (f32, f32), color| Mark::Shape(Shape { kind, color, width: 8.0, from: Pos2::new(from.0, from.1), to: Pos2::new(to.0, to.1), fill: true });
        d.add_mark(boxed(ShapeKind::Rectangle, (40.0, 40.0), (400.0, 300.0), Color32::RED));
        d.add_mark(boxed(ShapeKind::Ellipse, (300.0, 100.0), (620.0, 460.0), Color32::BLUE));
        d.add_mark(Stroke { ink: Ink::Highlighter, color: Color32::from_rgb(255, 230, 0), width: 24.0, points: vec![Pos2::new(5.0, 150.0), Pos2::new(630.0, 400.0)] });
        let mut shown = d.flatten();
        let mut burned = d.marks.clone();
        let tile_count = |t: &Tiles| t.indices().count();
        let mut step = |d: &mut Doc, i: usize, m: Mark, what: &str| {
            let mut both = Tiles::new(d.size());
            d.marks[i].footprint(&mut both);
            m.footprint(&mut both);
            d.replace_mark(i, m);
            let tiles = d.changed_tiles(&burned);
            for (r, patch) in d.render_runs(&tiles) {
                paste(&mut shown, &patch, r.pos());
            }
            burned = d.marks.clone();
            assert!(shown.pixels == d.flatten().pixels, "after {what}");
            (tile_count(&tiles), tile_count(&both))
        };
        let (redrawn, both) = step(&mut d, 1, boxed(ShapeKind::Rectangle, (50.0, 45.0), (410.0, 305.0), Color32::RED), "moving the box");
        assert!(redrawn * 2 < both, "the middle stays: {redrawn} of {both} tiles");
        let (redrawn, both) = step(&mut d, 2, boxed(ShapeKind::Ellipse, (300.0, 100.0), (610.0, 448.0), Color32::BLUE), "shrinking the ellipse");
        // An ellipse's footprint is its whole box, corners and all.
        assert!(redrawn * 4 < both * 3, "the middle stays: {redrawn} of {both} tiles");
        let (redrawn, both) = step(&mut d, 1, boxed(ShapeKind::Rectangle, (50.0, 45.0), (410.0, 305.0), Color32::GREEN), "recolouring the box");
        assert_eq!(redrawn, both, "a new colour repaints it all");
        let thin = boxed(ShapeKind::Ellipse, (300.0, 100.0), (620.0, 112.0), Color32::BLUE);
        assert!(same_tiles(&thin, &thin, d.size()).is_empty(), "too thin to have a middle");
        let Mark::Shape(hollow) = boxed(ShapeKind::Rectangle, (50.0, 45.0), (410.0, 305.0), Color32::GREEN) else { unreachable!() };
        let hollow = Mark::Shape(Shape { fill: false, ..hollow });
        assert!(same_tiles(&hollow, &hollow, d.size()).is_empty(), "an outline shows what is under it");
        step(&mut d, 1, hollow, "unfilling it");
        d.undo();
        let tiles = d.changed_tiles(&burned);
        for (r, patch) in d.render_runs(&tiles) {
            paste(&mut shown, &patch, r.pos());
        }
        assert!(shown.pixels == d.flatten().pixels, "after undoing that");
    }

    #[test]
    fn a_crop_lays_texts_out_again_once_for_the_hit_tests_after_it() {
        let mut d = Doc::new(gradient(200, 120));
        d.add_mark(text("Hi there", Color32::BLACK, true));
        d.flatten_cached();
        assert!(d.cached_text(&d.marks[0]).is_some());
        d.crop(Rect::from_min_max(Pos2::new(5.0, 2.0), Pos2::new(190.0, 110.0)));
        assert!(d.cached_text(&d.marks[0]).is_none(), "the crop changed the picture's size");
        let flat = d.flatten_cached();
        assert!(flat.pixels == d.flatten().pixels, "the same pixels as a plain flatten");
        assert!(d.cached_text(&d.marks[0]).is_some(), "and the text is laid out for what comes next");
    }

    #[test]
    fn revert_takes_back_an_edit_without_a_redo() {
        let mut d = Doc::new(white(40, 40));
        d.add_mark(shape(ShapeKind::Line, 2.0, (5.0, 5.0), (30.0, 5.0)));
        let original = d.marks[0].clone();
        d.checkpoint();
        d.replace_mark(0, shape(ShapeKind::Line, 2.0, (5.0, 15.0), (30.0, 15.0)));
        d.revert();
        assert!(Arc::ptr_eq(&d.marks[0], &original) && !d.can_redo());
        assert!(d.can_undo(), "adding the line is still there to undo");
    }

    #[test]
    fn the_topmost_mark_under_the_pointer_is_found() {
        let mut d = Doc::new(white(100, 100));
        d.add_mark(Shape { fill: true, ..stroke_free_shape(ShapeKind::Rectangle, (10.0, 10.0), (90.0, 90.0)) });
        d.add_mark(shape(ShapeKind::Line, 4.0, (0.0, 50.0), (100.0, 50.0)));
        d.add_mark(shape(ShapeKind::Rectangle, 2.0, (40.0, 40.0), (60.0, 60.0)));
        assert_eq!(d.mark_at(Pos2::new(70.0, 50.0), 1.0), Some(1), "the line over the filled box");
        assert_eq!(d.mark_at(Pos2::new(50.0, 40.0), 1.0), Some(2));
        assert_eq!(d.mark_at(Pos2::new(50.0, 45.0), 1.0), Some(0), "inside an outline: the fill under it");
        assert_eq!(d.mark_at(Pos2::new(95.0, 5.0), 1.0), None);
        assert_eq!(d.mark_at(Pos2::new(95.0, 5.0), 8.0), Some(0), "within the tolerance");
        assert_eq!(d.frame_of(3), None);
    }

    fn stroke_free_shape(kind: ShapeKind, from: (f32, f32), to: (f32, f32)) -> Shape {
        Shape { kind, color: Color32::BLUE, width: 2.0, from: Pos2::new(from.0, from.1), to: Pos2::new(to.0, to.1), fill: false }
    }

    #[test]
    fn reframing_stretches_points_and_corners_into_the_new_box() {
        let mut s = Mark::Stroke(Stroke { ink: Ink::Pen, color: Color32::RED, width: 3.0, points: vec![Pos2::new(10.0, 10.0), Pos2::new(20.0, 30.0), Pos2::new(30.0, 20.0)] });
        let from = s.frame();
        assert_eq!(from, Rect::from_min_max(Pos2::new(10.0, 10.0), Pos2::new(30.0, 30.0)));
        s.reframe(from, Rect::from_min_max(Pos2::new(10.0, 10.0), Pos2::new(50.0, 20.0)));
        assert_eq!(stroke(&s).points, vec![Pos2::new(10.0, 10.0), Pos2::new(30.0, 20.0), Pos2::new(50.0, 15.0)]);
        assert_eq!(stroke(&s).width, 3.0, "the line keeps its width");
        // Dragged past the opposite side: mirrored.
        let from = s.frame();
        s.reframe(from, Rect::from_min_max(Pos2::new(10.0, 10.0), Pos2::new(-30.0, 20.0)));
        assert_eq!(stroke(&s).points[2], Pos2::new(-30.0, 15.0));
        // A flat stroke only moves along its flat side.
        let mut flat = Mark::Stroke(Stroke { ink: Ink::Pen, color: Color32::RED, width: 3.0, points: vec![Pos2::new(0.0, 5.0), Pos2::new(10.0, 5.0)] });
        let from = flat.frame();
        flat.reframe(from, Rect::from_min_max(Pos2::new(0.0, 8.0), Pos2::new(20.0, 30.0)));
        assert_eq!(stroke(&flat).points, vec![Pos2::new(0.0, 8.0), Pos2::new(20.0, 8.0)]);
        let mut r = shape(ShapeKind::Ellipse, 2.0, (40.0, 30.0), (10.0, 10.0));
        let from = r.frame();
        r.reframe(from, Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(60.0, 40.0)));
        assert!(matches!(&r, Mark::Shape(s) if s.from == Pos2::new(60.0, 40.0) && s.to == Pos2::new(0.0, 0.0)), "{r:?}");
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

    fn tiles_of(size: [usize; 2], r: Rect) -> Tiles {
        let mut t = Tiles::new(size);
        t.add(r);
        t
    }

    /// Paints `t` over `base` in patches of `w` x `h` pixels, last patch first when `reverse`.
    fn paint_in_patches(t: &Text, base: &ColorImage, w: usize, h: usize, reverse: bool) -> ColorImage {
        let mut out = base.clone();
        let mut raster = TextRaster::new(t, base.size);
        let mut spots: Vec<[usize; 2]> = (0..base.size[1]).step_by(h).flat_map(|y| (0..base.size[0]).step_by(w).map(move |x| [x, y])).collect();
        if reverse {
            spots.reverse();
        }
        for [x, y] in spots {
            let mut patch = sub_image(base, x, y, w.min(base.size[0] - x), h.min(base.size[1] - y));
            raster.paint(&mut patch, [x, y]);
            paste(&mut out, &patch, [x, y]);
        }
        out
    }

    #[test]
    fn text_coverage_is_the_same_whatever_patch_it_is_rendered_in() {
        let t = Text { text: "Wavy gj\n0@%".into(), color: Color32::from_rgb(0x00, 0x78, 0xd4), size: 47.3, outline: true, pos: Pos2::new(3.7, -9.2) };
        let base = gradient(200, 130);
        let whole = paint_in_patches(&t, &base, 200, 130, false);
        assert!(whole.pixels != base.pixels, "the text shows");
        for (w, h, reverse) in [(32, 32, false), (7, 13, true), (64, 5, false), (1, 130, true)] {
            assert!(paint_in_patches(&t, &base, w, h, reverse).pixels == whole.pixels, "{w}x{h} patches differ from one whole render");
        }
    }

    #[test]
    fn retyped_text_redraws_only_its_changed_tiles_and_matches_a_fresh_render() {
        let base = gradient(260, 160);
        let at = |s: &str, color| Text { text: s.into(), color, size: 38.0, outline: true, pos: Pos2::new(6.0, 4.0) };
        let mut shown = base.clone();
        let mut live = TextRaster::new(&at("", Color32::RED), base.size);
        for (s, color) in [("H", Color32::RED), ("Hello", Color32::RED), ("Hello\nwo", Color32::RED), ("Hello\nw", Color32::RED), ("Hello\nw", Color32::BLUE), ("Hel", Color32::BLUE)] {
            let new = at(s, color);
            let mut dirty = Tiles::new(base.size);
            live.retext(&new, &mut dirty);
            for r in dirty.runs() {
                let mut patch = sub_image(&base, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
                live.paint(&mut patch, r.pos());
                paste(&mut shown, &patch, r.pos());
            }
            let mut want = base.clone();
            burn(&mut want, &Mark::Text(new));
            assert!(shown.pixels == want.pixels, "after typing to {s:?}");
        }
        // Typing one letter at the end redoes the tiles that letter reaches, not the whole text.
        let mut dirty = Tiles::new(base.size);
        live.retext(&at("Hell", Color32::BLUE), &mut dirty);
        let all = tiles_of(base.size, live.bounds());
        assert!(dirty.indices().count() < all.indices().count() / 2, "{} of {} tiles", dirty.indices().count(), all.indices().count());
    }

    #[test]
    fn a_long_text_relaid_from_its_checkpoints_matches_a_whole_layout() {
        // Past several checkpoints, with lines, spaces, a letter the font lacks and a multi-byte one.
        let line = "Pasted line \u{1F600} with é and spaces   ";
        let long: String = (0..40).map(|i| format!("{i} {line}\n")).collect();
        let at = |s: &str| Text { text: s.into(), color: Color32::RED, size: 20.0, outline: true, pos: Pos2::new(7.5, 3.25) };
        let same_layout = |a: &TextRaster, b: &TextRaster| {
            a.layout.glyphs.len() == b.layout.glyphs.len()
                && a.layout.glyphs.iter().zip(&b.layout.glyphs).all(|(x, y)| x.same(y))
                && a.bounds() == b.bounds()
                && a.caret() == b.caret()
        };
        let mut live = TextRaster::new(&at(""), [800, 600]);
        let cut = long.char_indices().map(|(i, _)| i).nth(5000 % long.chars().count()).unwrap();
        for s in [long.as_str(), &format!("{long}more"), &format!("{long}mor"), &long[..cut], &long[..long.len() - 1], &long.replacen("17 ", "seventeen ", 1), "", &long] {
            let mut dirty = Tiles::new([800, 600]);
            live.retext(&at(s), &mut dirty);
            let fresh = TextRaster::new(&at(s), [800, 600]);
            assert!(same_layout(&live, &fresh), "after retyping to {} bytes", s.len());
            assert_eq!(fresh.caret(), at(s).caret());
        }
        assert!(live.layout.marks.len() >= long.chars().count() / CHECKPOINT_CHARS, "checkpoints all along");
        for b in (0..=long.len()).filter(|&b| long.is_char_boundary(b)).step_by(97) {
            assert_eq!(live.caret_at(b), at(&long[..b]).caret(), "caret after {b} bytes");
        }
    }

    #[test]
    fn characters_the_font_lacks_are_skipped_not_boxed() {
        let with = Text { text: "a\u{1F600}b\u{4E2D}".into(), color: Color32::BLACK, size: 30.0, outline: false, pos: Pos2::new(5.0, 5.0) };
        let without = Text { text: "ab".into(), ..with.clone() };
        assert_eq!(with.caret(), without.caret(), "a missing glyph takes no room");
        let (mut a, mut b) = (white(120, 50), white(120, 50));
        burn(&mut a, &Mark::Text(with));
        burn(&mut b, &Mark::Text(without));
        assert!(a.pixels == b.pixels, "and draws nothing");
    }

    #[test]
    fn a_line_running_millions_of_pixels_past_the_picture_is_cut_short_not_a_panic() {
        // A long paste at a big size: past ~8.4M px a 1/256 px origin no longer fits an i32.
        let mut t = Text { text: "W".repeat(6000), color: Color32::RED, size: 2000.0, outline: true, pos: Pos2::ZERO };
        // Baseline across the middle of the picture, so the first letter shows.
        t.pos = Pos2::new(-200.0, 128.0 - t.line_metrics(&Text::font().unwrap()).ascent);
        assert!(t.caret().x > 1.0e7, "the line does run that far");
        let burn = |t: &Text| {
            let mut d = Doc::new(white(256, 256));
            d.add_mark(t.clone());
            d.flatten()
        };
        let out = burn(&t);
        assert!(out.pixels.iter().any(|&p| p != Color32::WHITE), "its start still shows");
        assert!(out == burn(&Text { text: "WW".into(), ..t.clone() }), "exactly as the start of the line alone");
        assert!(t.bounds().max.x <= MAX_ORIGIN + 4000.0);
    }

    #[test]
    fn the_eraser_finds_text_by_its_letters_not_its_box() {
        let mut d = Doc::new(white(400, 120));
        d.add_mark(Text { text: "l          l".into(), color: Color32::BLACK, size: 60.0, outline: false, pos: Pos2::new(10.0, 10.0) });
        let b = d.marks[0].bounds();
        assert!(!d.hits(Pos2::new(b.center().x, b.center().y), 2.0), "the gap between the letters is not text");
        assert!(d.hits(Pos2::new(b.min.x + 4.0, b.center().y), 2.0), "a letter is");
        // Once rendered, hit tests use the cached letters.
        d.render_runs(&tiles_of(d.size(), b));
        assert!(!d.hits(Pos2::new(b.center().x, b.center().y), 2.0));
        assert!(d.erase_at(Pos2::new(b.max.x - 4.0, b.center().y), 2.0));
    }

    #[test]
    fn an_erase_beside_text_renders_none_of_the_text() {
        let mut d = Doc::new(white(600, 300));
        d.add_mark(text("Far away", Color32::RED, true));
        let dot = shape(ShapeKind::Line, 4.0, (560.0, 280.0), (570.0, 280.0));
        d.add_mark(dot);
        let before = d.marks.clone();
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(565.0, 280.0), 3.0));
        let tiles = d.changed_tiles(&before);
        let runs = d.render_runs(&tiles);
        assert!(!runs.is_empty());
        let rendered: usize = d.texts.values().map(|(_, r)| r.tiles.len()).sum();
        assert_eq!(rendered, 0, "no tile of the far text was rendered");
        d.undo();
        let tiles = d.changed_tiles(&d.marks[..1]);
        d.render_runs(&tiles);
        assert_eq!(d.texts.values().map(|(_, r)| r.tiles.len()).sum::<usize>(), 0, "nor on undo");
    }

    #[test]
    fn rendered_text_is_kept_only_while_it_is_on_the_snip() {
        let mut d = Doc::new(white(600, 300));
        d.add_mark(text("Kept", Color32::RED, true));
        let all = |d: &Doc| {
            let mut t = Tiles::new(d.size());
            t.add(Rect::from_min_size(Pos2::ZERO, eframe::egui::vec2(600.0, 300.0)));
            t
        };
        let shown = d.render_runs(&all(&d));
        assert_eq!(d.texts.len(), 1);
        let before = d.marks.clone();
        d.checkpoint();
        assert!(d.erase_at(Pos2::new(60.0, 40.0), 30.0), "rubs the text out");
        d.render_runs(&d.changed_tiles(&before));
        assert!(d.texts.is_empty(), "undo history still holds it, the cache doesn't");
        d.undo();
        assert!(d.render_runs(&all(&d)) == shown, "undo renders it again, the same");
        assert_eq!(d.texts.len(), 1);
    }
}
