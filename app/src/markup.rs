//! Snip document: picture, marks (ink strokes), crop and undo history.

use std::sync::Arc;

use eframe::egui::{Color32, ColorImage, Pos2, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Pen,
    Highlighter,
}

/// Highlighter ink is see-through; it is stored opaque and faded when drawn.
pub const HIGHLIGHT_ALPHA: f32 = 0.42;

#[derive(Clone, PartialEq)]
pub struct Stroke {
    pub ink: Ink,
    pub color: Color32,
    /// Thickness in image pixels.
    pub width: f32,
    /// Points in image pixels.
    pub points: Vec<Pos2>,
}

impl Stroke {
    fn alpha(&self) -> f32 {
        match self.ink {
            Ink::Pen => 1.0,
            Ink::Highlighter => HIGHLIGHT_ALPHA,
        }
    }

    fn radius(&self) -> f32 {
        (self.width * 0.5).max(0.5)
    }

    /// Pixels of an image of `size` that the segments from point `from - 1`
    /// onwards can touch; `from = 0` is the whole stroke.
    pub fn bounds_from(&self, from: usize, size: [usize; 2]) -> Option<PxRect> {
        let pts = self.points.get(from.saturating_sub(1)..).filter(|p| !p.is_empty())?;
        let b = Rect::from_points(pts).expand(self.radius() + 1.0);
        let r = PxRect {
            x0: b.min.x.floor().max(0.0) as usize,
            y0: b.min.y.floor().max(0.0) as usize,
            x1: (b.max.x.ceil().max(0.0) as usize).min(size[0]),
            y1: (b.max.y.ceil().max(0.0) as usize).min(size[1]),
        };
        (r.x1 > r.x0 && r.y1 > r.y0).then_some(r)
    }

    /// Coverage (0..=1) of every pixel in `r`, the max over all segments so
    /// overlaps don't add up.
    fn coverage(&self, r: PxRect) -> Vec<f32> {
        let rad = self.radius();
        let bw = r.x1 - r.x0;
        let mut cover = vec![0f32; bw * (r.y1 - r.y0)];
        let segments: Vec<(Pos2, Pos2)> = if self.points.len() == 1 {
            vec![(self.points[0], self.points[0])]
        } else {
            self.points.windows(2).map(|p| (p[0], p[1])).collect()
        };
        for (a, b) in segments {
            let sb = Rect::from_two_pos(a, b).expand(rad + 1.0);
            let sx0 = (sb.min.x.floor().max(r.x0 as f32) as usize).max(r.x0);
            let sy0 = (sb.min.y.floor().max(r.y0 as f32) as usize).max(r.y0);
            let sx1 = (sb.max.x.ceil().max(0.0) as usize).min(r.x1);
            let sy1 = (sb.max.y.ceil().max(0.0) as usize).min(r.y1);
            for y in sy0..sy1 {
                for x in sx0..sx1 {
                    let d = dist_to_segment(Pos2::new(x as f32 + 0.5, y as f32 + 0.5), a, b);
                    let c = (rad + 0.5 - d).clamp(0.0, 1.0);
                    let slot = &mut cover[(y - r.y0) * bw + (x - r.x0)];
                    if c > *slot {
                        *slot = c;
                    }
                }
            }
        }
        cover
    }

    /// The stroke's ink over `r` alone, premultiplied, with exactly the
    /// coverage and alpha `burn_stroke` uses: drawn over the picture it
    /// gives the same pixels as the saved snip.
    pub fn ink_tile(&self, r: PxRect) -> ColorImage {
        let alpha = self.alpha();
        let [cr, cg, cb, _] = self.color.to_array();
        let pixels = self
            .coverage(r)
            .into_iter()
            .map(|c| {
                let k = c * alpha;
                let m = |v: u8| (v as f32 * k).round() as u8;
                Color32::from_rgba_premultiplied(m(cr), m(cg), m(cb), m(255))
            })
            .collect();
        ColorImage::new([r.x1 - r.x0, r.y1 - r.y0], pixels)
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        let reach = self.width * 0.5 + radius;
        match self.points.as_slice() {
            [only] => only.distance(p) <= reach,
            pts => pts.windows(2).any(|s| dist_to_segment(p, s[0], s[1]) <= reach),
        }
    }
}

/// Anything drawn on the snip. Each mark is burned into the picture on its own, in order.
#[derive(Clone, PartialEq)]
pub enum Mark {
    Stroke(Stroke),
}

impl From<Stroke> for Mark {
    fn from(s: Stroke) -> Self {
        Mark::Stroke(s)
    }
}

impl Mark {
    /// Pixels of an image of `size` that the mark can touch.
    pub fn bounds(&self, size: [usize; 2]) -> Option<PxRect> {
        match self {
            Mark::Stroke(s) => s.bounds_from(0, size),
        }
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        match self {
            Mark::Stroke(s) => s.touches(p, radius),
        }
    }

    fn translate(&mut self, d: eframe::egui::Vec2) {
        match self {
            Mark::Stroke(s) => s.points.iter_mut().for_each(|p| *p += d),
        }
    }
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
    pub fn union(self, o: PxRect) -> PxRect {
        PxRect { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }

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

pub fn burn(img: &mut ColorImage, m: &Mark) {
    match m {
        Mark::Stroke(s) => burn_stroke(img, s),
    }
}

/// Rasterises one stroke with soft edges. Coverage is collected first so a
/// highlighter that crosses itself doesn't get darker where it overlaps.
fn burn_stroke(img: &mut ColorImage, s: &Stroke) {
    let Some(r) = s.bounds_from(0, img.size) else { return };
    let w = img.size[0];
    let bw = r.x1 - r.x0;
    let cover = s.coverage(r);
    let alpha = s.alpha();
    let [cr, cg, cb, _] = s.color.to_array();
    for y in r.y0..r.y1 {
        for x in r.x0..r.x1 {
            let k = cover[(y - r.y0) * bw + (x - r.x0)] * alpha;
            if k <= 0.0 {
                continue;
            }
            let px = &mut img.pixels[y * w + x];
            let [r0, g0, b0, a0] = px.to_array();
            let mix = |base: u8, ink: u8| (base as f32 * (1.0 - k) + ink as f32 * k).round() as u8;
            // Pixels are premultiplied: ink adds its own coverage to alpha.
            let a = (a0 as f32 + (255.0 - a0 as f32) * k).round() as u8;
            *px = Color32::from_rgba_premultiplied(mix(r0, cr), mix(g0, cg), mix(b0, cb), a);
        }
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
        }
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

    fn paste(dst: &mut ColorImage, tile: &ColorImage, at: [usize; 2]) {
        for y in 0..tile.size[1] {
            for x in 0..tile.size[0] {
                dst.pixels[(at[1] + y) * dst.size[0] + at[0] + x] = tile.pixels[y * tile.size[0] + x];
            }
        }
    }

    #[test]
    fn highlighter_tile_is_uniform_at_crossings_and_corners() {
        let s = zigzag();
        let r = s.bounds_from(0, [40, 40]).unwrap();
        let tile = s.ink_tile(r);
        let alpha_at = |x: usize, y: usize| tile.pixels[(y - r.y0) * tile.size[0] + (x - r.x0)].a();
        let single = alpha_at(10, 10);
        assert_eq!(single, (255.0 * HIGHLIGHT_ALPHA).round() as u8);
        assert_eq!(alpha_at(20, 20), single, "self-crossing is no darker");
        assert_eq!(alpha_at(34, 34), single, "joint is no darker");
        assert_eq!(alpha_at(35, 36), single, "corner is filled round, not notched");
    }

    #[test]
    fn ink_tile_over_picture_matches_saved_pixels() {
        let mut d = Doc::new(white(40, 40));
        d.add_mark(zigzag());
        let saved = d.flatten();
        let s = stroke(&d.marks[0]);
        let r = s.bounds_from(0, d.size()).unwrap();
        let mut shown = white(40, 40);
        let tile = s.ink_tile(r);
        for y in r.y0..r.y1 {
            for x in r.x0..r.x1 {
                // Premultiplied "over", as the GPU composites the live layer.
                let ink = tile.pixels[(y - r.y0) * tile.size[0] + (x - r.x0)].to_array();
                let px = &mut shown.pixels[y * 40 + x];
                let base = px.to_array();
                let o = |i: usize| (ink[i] as f32 + base[i] as f32 * (1.0 - ink[3] as f32 / 255.0)).round() as u8;
                *px = Color32::from_rgba_premultiplied(o(0), o(1), o(2), o(3));
            }
        }
        for (a, b) in shown.pixels.iter().zip(&saved.pixels) {
            for (ca, cb) in a.to_array().into_iter().zip(b.to_array()) {
                assert!(ca.abs_diff(cb) <= 1, "shown {a:?} vs saved {b:?}");
            }
        }
    }

    #[test]
    fn live_tiles_built_point_by_point_match_one_pass() {
        let full = zigzag();
        let size = [40, 40];
        let mut layer = ColorImage::new(size, vec![Color32::TRANSPARENT; 1600]);
        let mut done = 0;
        for n in 1..=full.points.len() {
            let s = Stroke { points: full.points[..n].to_vec(), ..full.clone() };
            let r = s.bounds_from(done, size).unwrap();
            paste(&mut layer, &s.ink_tile(r), r.pos());
            done = n;
        }
        let mut once = ColorImage::new(size, vec![Color32::TRANSPARENT; 1600]);
        let r = full.bounds_from(0, size).unwrap();
        paste(&mut once, &full.ink_tile(r), r.pos());
        assert!(layer.pixels == once.pixels);
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
