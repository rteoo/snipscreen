//! Snip document: picture, marks (ink strokes), crop and undo history.
//!
//! Every mark is rasterised on the CPU by `Mark::paint`, tile run by tile run. The editor shows a
//! mark being drawn by painting the same runs over the picture it will be burned into, so what is
//! on screen is exactly what gets copied or saved, blend modes included.

use std::sync::Arc;

use eframe::egui::{Color32, ColorImage, Pos2, Rect};

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
                // Ramanujan's perimeter; chords of about 3 px keep the outline round at any size.
                let perimeter = std::f32::consts::PI * (3.0 * (a + b) - ((3.0 * a + b) * (a + 3.0 * b)).sqrt());
                let n = (perimeter / 3.0).clamp(24.0, 4096.0) as usize;
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

    fn paint(&self, patch: &mut ColorImage, at: [usize; 2]) {
        let r = patch_rect(patch, at);
        let mut cover = vec![0f32; patch.pixels.len()];
        match self.kind {
            ShapeKind::Rectangle => cover_box_outline(&mut cover, r, Rect::from_two_pos(self.from, self.to), self.radius()),
            _ => cover_segments(&mut cover, r, self.segments().into_iter(), self.radius()),
        }
        if self.kind == ShapeKind::Arrow {
            cover_triangle(&mut cover, r, self.arrow().1);
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

/// Anything drawn on the snip. Each mark is burned into the picture on its own, in order.
#[derive(Clone, PartialEq, Debug)]
pub enum Mark {
    Stroke(Stroke),
    Shape(Shape),
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
        }
    }

    /// Composites the mark onto `patch`, a piece of the picture whose top-left pixel is `at`. The
    /// result for a pixel depends only on that pixel and the mark, never on where the patch ends,
    /// so painting tile runs one at a time gives the same pixels as painting the whole picture.
    pub fn paint(&self, patch: &mut ColorImage, at: [usize; 2]) {
        match self {
            Mark::Stroke(s) => s.paint(patch, at),
            Mark::Shape(s) => s.paint(patch, at),
        }
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        match self {
            Mark::Stroke(s) => s.touches(p, radius),
            Mark::Shape(s) => s.touches(p, radius),
        }
    }

    fn translate(&mut self, d: eframe::egui::Vec2) {
        match self {
            Mark::Stroke(s) => s.points.iter_mut().for_each(|p| *p += d),
            Mark::Shape(s) => {
                s.from += d;
                s.to += d;
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
    for (a, b) in segments {
        let sb = Rect::from_two_pos(a, b).expand(rad + 1.0);
        let sx0 = (sb.min.x.floor().max(0.0) as usize).max(r.x0);
        let sy0 = (sb.min.y.floor().max(0.0) as usize).max(r.y0);
        let sx1 = (sb.max.x.ceil().max(0.0) as usize).min(r.x1);
        let sy1 = (sb.max.y.ceil().max(0.0) as usize).min(r.y1);
        for y in sy0..sy1 {
            let row = &mut cover[(y - r.y0) * bw..(y - r.y0 + 1) * bw];
            for x in sx0..sx1 {
                let d = dist_to_segment(Pos2::new(x as f32 + 0.5, y as f32 + 0.5), a, b);
                let c = (rad + 0.5 - d).clamp(0.0, 1.0);
                let slot = &mut row[x - r.x0];
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
    marks: Vec<Mark>,
}

pub struct Doc {
    pub image: Arc<ColorImage>,
    pub marks: Vec<Mark>,
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
        self.marks.push(m.into());
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
            m.translate(-shift);
        }
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
    for r in tiles.runs() {
        let mut patch = sub_image(img, r.x0, r.y0, r.x1 - r.x0, r.y1 - r.y0);
        m.paint(&mut patch, r.pos());
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
        d.marks.push(Mark::Stroke(Stroke {
            ink: Ink::Pen,
            color: Color32::RED,
            width: 4.0,
            points: vec![Pos2::new(50.0, 40.0)],
        }));
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
        d.marks.push(Mark::Stroke(Stroke {
            ink: Ink::Pen,
            color: Color32::BLACK,
            width: 6.0,
            points: vec![Pos2::new(2.0, 10.0), Pos2::new(18.0, 10.0)],
        }));
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
            m.paint(&mut patch, r.pos());
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
        ];
        for m in &marks {
            let mut by_runs = gradient(70, 50);
            burn(&mut by_runs, m);
            let mut whole = gradient(70, 50);
            m.paint(&mut whole, [0, 0]);
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
        assert!(matches!(&d.marks[0], Mark::Shape(Shape { kind: ShapeKind::Arrow, .. })));
        d.crop(Rect::from_min_max(Pos2::new(5.0, 5.0), Pos2::new(95.0, 95.0)));
        assert!(matches!(&d.marks[0], Mark::Shape(s) if s.from == Pos2::new(5.0, 45.0)), "crop moves shapes too");
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
            d.marks.push(Mark::Stroke(Stroke {
                ink: Ink::Highlighter,
                color: Color32::YELLOW,
                width: 4.0,
                points: vec![Pos2::new(5.0, y), Pos2::new(45.0, y)],
            }));
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
