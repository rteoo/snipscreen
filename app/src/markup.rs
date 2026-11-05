//! Snip document: picture, ink strokes, crop and undo history.

use std::sync::Arc;

use eframe::egui::{Color32, ColorImage, Pos2, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Pen,
    Highlighter,
}

/// Highlighter ink is see-through; it is stored opaque and faded when drawn.
pub const HIGHLIGHT_ALPHA: f32 = 0.42;

#[derive(Clone)]
pub struct Stroke {
    pub ink: Ink,
    pub color: Color32,
    /// Thickness in image pixels.
    pub width: f32,
    /// Points in image pixels.
    pub points: Vec<Pos2>,
}

impl Stroke {
    pub fn paint_color(&self) -> Color32 {
        match self.ink {
            Ink::Pen => self.color,
            Ink::Highlighter => self.color.gamma_multiply(HIGHLIGHT_ALPHA),
        }
    }

    fn touches(&self, p: Pos2, radius: f32) -> bool {
        let reach = self.width * 0.5 + radius;
        match self.points.as_slice() {
            [only] => only.distance(p) <= reach,
            pts => pts.windows(2).any(|s| dist_to_segment(p, s[0], s[1]) <= reach),
        }
    }
}

#[derive(Clone)]
struct Snapshot {
    image: Arc<ColorImage>,
    strokes: Vec<Stroke>,
}

pub struct Doc {
    pub image: Arc<ColorImage>,
    pub strokes: Vec<Stroke>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Bumped whenever `image` changes so the texture can be refreshed.
    pub image_rev: u64,
}

const HISTORY: usize = 50;

impl Doc {
    pub fn new(image: ColorImage) -> Self {
        Self {
            image: Arc::new(image),
            strokes: Vec::new(),
            undo: Vec::new(),
            redo: Vec::new(),
            image_rev: 0,
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
        Snapshot { image: self.image.clone(), strokes: self.strokes.clone() }
    }

    fn restore(&mut self, s: Snapshot) {
        if !Arc::ptr_eq(&s.image, &self.image) {
            self.image_rev += 1;
        }
        self.image = s.image;
        self.strokes = s.strokes;
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

    pub fn hits(&self, p: Pos2, radius: f32) -> bool {
        self.strokes.iter().any(|s| s.touches(p, radius))
    }

    /// Removes every stroke within `radius` of `p`. Returns whether any went.
    pub fn erase_at(&mut self, p: Pos2, radius: f32) -> bool {
        let before = self.strokes.len();
        self.strokes.retain(|s| !s.touches(p, radius));
        self.strokes.len() != before
    }

    /// Crops to `r` (image pixels); strokes move with the picture.
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
        let shift = eframe::egui::vec2(x0 as f32, y0 as f32);
        for s in &mut self.strokes {
            for p in &mut s.points {
                *p -= shift;
            }
        }
    }

    /// The picture with all ink burned in, ready to copy or save.
    pub fn flatten(&self) -> ColorImage {
        let mut out = (*self.image).clone();
        for s in &self.strokes {
            burn_stroke(&mut out, s);
        }
        out
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

/// Rasterises one stroke with soft edges. Coverage is collected first so a
/// highlighter that crosses itself doesn't get darker where it overlaps.
fn burn_stroke(img: &mut ColorImage, s: &Stroke) {
    if s.points.is_empty() {
        return;
    }
    let [w, h] = img.size;
    let r = (s.width * 0.5).max(0.5);
    let bounds = Rect::from_points(&s.points).expand(r + 1.0);
    let x0 = bounds.min.x.floor().max(0.0) as usize;
    let y0 = bounds.min.y.floor().max(0.0) as usize;
    let x1 = (bounds.max.x.ceil().max(0.0) as usize).min(w);
    let y1 = (bounds.max.y.ceil().max(0.0) as usize).min(h);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let bw = x1 - x0;
    let mut cover = vec![0f32; bw * (y1 - y0)];
    let segments: Vec<(Pos2, Pos2)> = if s.points.len() == 1 {
        vec![(s.points[0], s.points[0])]
    } else {
        s.points.windows(2).map(|p| (p[0], p[1])).collect()
    };
    for (a, b) in segments {
        let sb = Rect::from_two_pos(a, b).expand(r + 1.0);
        let sx0 = (sb.min.x.floor().max(x0 as f32) as usize).max(x0);
        let sy0 = (sb.min.y.floor().max(y0 as f32) as usize).max(y0);
        let sx1 = (sb.max.x.ceil().max(0.0) as usize).min(x1);
        let sy1 = (sb.max.y.ceil().max(0.0) as usize).min(y1);
        for y in sy0..sy1 {
            for x in sx0..sx1 {
                let d = dist_to_segment(Pos2::new(x as f32 + 0.5, y as f32 + 0.5), a, b);
                let c = (r + 0.5 - d).clamp(0.0, 1.0);
                let slot = &mut cover[(y - y0) * bw + (x - x0)];
                if c > *slot {
                    *slot = c;
                }
            }
        }
    }
    let alpha = match s.ink {
        Ink::Pen => 1.0,
        Ink::Highlighter => HIGHLIGHT_ALPHA,
    };
    let [cr, cg, cb, _] = s.color.to_array();
    for y in y0..y1 {
        for x in x0..x1 {
            let k = cover[(y - y0) * bw + (x - x0)] * alpha;
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

    #[test]
    fn crop_moves_strokes_and_undo_restores() {
        let mut d = Doc::new(white(100, 80));
        d.checkpoint();
        d.strokes.push(Stroke {
            ink: Ink::Pen,
            color: Color32::RED,
            width: 4.0,
            points: vec![Pos2::new(50.0, 40.0)],
        });
        d.crop(Rect::from_min_max(Pos2::new(10.0, 20.0), Pos2::new(60.0, 70.0)));
        assert_eq!(d.size(), [50, 50]);
        assert_eq!(d.strokes[0].points[0], Pos2::new(40.0, 20.0));
        d.undo();
        assert_eq!(d.size(), [100, 80]);
        assert_eq!(d.strokes[0].points[0], Pos2::new(50.0, 40.0));
        d.undo();
        assert!(d.strokes.is_empty());
        d.redo();
        assert_eq!(d.strokes.len(), 1);
    }

    #[test]
    fn flatten_burns_pen_ink() {
        let mut d = Doc::new(white(20, 20));
        d.strokes.push(Stroke {
            ink: Ink::Pen,
            color: Color32::BLACK,
            width: 6.0,
            points: vec![Pos2::new(2.0, 10.0), Pos2::new(18.0, 10.0)],
        });
        let out = d.flatten();
        assert_eq!(out.pixels[10 * 20 + 10], Color32::BLACK);
        assert_eq!(out.pixels[0], Color32::WHITE);
        assert_eq!(d.image.pixels[10 * 20 + 10], Color32::WHITE, "original untouched");
    }

    #[test]
    fn eraser_removes_touched_stroke_only() {
        let mut d = Doc::new(white(50, 50));
        for y in [10.0, 40.0] {
            d.strokes.push(Stroke {
                ink: Ink::Highlighter,
                color: Color32::YELLOW,
                width: 4.0,
                points: vec![Pos2::new(5.0, y), Pos2::new(45.0, y)],
            });
        }
        assert!(d.erase_at(Pos2::new(25.0, 12.0), 2.0));
        assert_eq!(d.strokes.len(), 1);
        assert_eq!(d.strokes[0].points[0].y, 40.0);
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
