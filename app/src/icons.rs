//! Toolbar icons drawn as vector strokes, so they stay sharp at any scale
//! and cost no image files.

use eframe::egui::{vec2, Color32, Painter, Rect, Shape, Stroke};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Rectangle,
    Window,
    FullScreen,
    Freeform,
    Timer,
    TimerOff,
    Chevron,
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
    Check,
    Close,
    Undo,
    Redo,
    Save,
    Copy,
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

/// Paints `icon` centred in `rect` on a 20×20 grid. `ink` tints the tip of
/// the pen and highlighter with the colour they will draw in.
pub fn paint(p: &Painter, rect: Rect, icon: Icon, color: Color32, ink: Option<Color32>) {
    let s = rect.width().min(rect.height()) / 20.0;
    let o = rect.center() - vec2(10.0, 10.0) * s;
    let at = |x: f32, y: f32| o + vec2(x, y) * s;
    let line = |pts: &[(f32, f32)], w: f32, c: Color32| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| at(x, y)).collect(), Stroke::new(w * s, c)));
    };
    let closed = |pts: &[(f32, f32)], w: f32, c: Color32| {
        p.add(Shape::closed_line(pts.iter().map(|&(x, y)| at(x, y)).collect(), Stroke::new(w * s, c)));
    };
    let filled = |pts: &[(f32, f32)], c: Color32| {
        p.add(Shape::convex_polygon(pts.iter().map(|&(x, y)| at(x, y)).collect(), c, Stroke::NONE));
    };
    let rect_at = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(at(x0, y0), at(x1, y1));
    let w = 1.4;
    let stroke = Stroke::new(w * s, color);
    let outline = eframe::egui::StrokeKind::Middle;

    match icon {
        Icon::Plus => {
            line(&[(10.0, 3.5), (10.0, 16.5)], 1.6, color);
            line(&[(3.5, 10.0), (16.5, 10.0)], 1.6, color);
        }
        Icon::Rectangle => {
            p.rect_stroke(rect_at(3.0, 5.0, 17.0, 15.0), 2.0 * s, stroke, outline);
        }
        Icon::Window => {
            p.rect_stroke(rect_at(3.0, 4.0, 17.0, 16.0), 2.0 * s, stroke, outline);
            line(&[(3.0, 7.5), (17.0, 7.5)], w, color);
        }
        Icon::FullScreen => {
            for (a, b, c) in [
                ((3.0, 7.0), (3.0, 4.0), (6.0, 4.0)),
                ((14.0, 4.0), (17.0, 4.0), (17.0, 7.0)),
                ((17.0, 13.0), (17.0, 16.0), (14.0, 16.0)),
                ((6.0, 16.0), (3.0, 16.0), (3.0, 13.0)),
            ] {
                line(&[a, b, c], w, color);
            }
        }
        Icon::Freeform => {
            closed(
                &[(4.0, 9.0), (6.5, 4.5), (11.0, 5.5), (15.5, 4.0), (17.0, 8.5), (14.5, 12.0), (16.0, 15.5), (10.0, 16.0), (5.0, 14.5)],
                w,
                color,
            );
        }
        Icon::Timer | Icon::TimerOff => {
            p.circle_stroke(at(10.0, 11.0), 6.5 * s, stroke);
            line(&[(10.0, 7.5), (10.0, 11.0), (12.5, 12.5)], w, color);
            line(&[(8.0, 2.5), (12.0, 2.5)], w, color);
            if icon == Icon::TimerOff {
                line(&[(3.0, 3.5), (17.0, 18.0)], w, color);
            }
        }
        Icon::Chevron => {
            line(&[(6.0, 8.0), (10.0, 12.0), (14.0, 8.0)], 1.3, color);
        }
        Icon::Pen => {
            closed(&[(12.5, 3.0), (15.5, 6.0), (7.0, 14.5), (4.0, 11.5)], w, color);
            filled(&[(4.0, 11.5), (7.0, 14.5), (2.5, 16.5)], ink.unwrap_or(color));
            line(&[(4.0, 11.5), (7.0, 14.5), (2.5, 16.5), (4.0, 11.5)], 1.0, ink.unwrap_or(color));
        }
        Icon::Highlighter => {
            filled(&[(5.0, 12.0), (8.0, 15.0), (6.0, 17.0), (2.5, 17.0), (2.5, 15.0)], ink.unwrap_or(color));
            closed(&[(12.0, 2.5), (17.0, 7.5), (9.0, 15.5), (4.0, 10.5)], w, color);
            line(&[(4.0, 10.5), (9.0, 15.5)], w, color);
        }
        Icon::Eraser => {
            closed(&[(11.5, 3.0), (17.0, 8.5), (10.0, 15.5), (6.5, 15.5), (3.0, 12.0)], w, color);
            line(&[(7.0, 7.5), (12.5, 13.0)], w, color);
            line(&[(6.5, 15.5), (17.0, 15.5)], w, color);
        }
        Icon::Arrow | Icon::Line | Icon::RectOutline | Icon::Ellipse => {
            match icon {
                Icon::Arrow => {
                    line(&[(3.5, 15.5), (11.0, 8.0)], w, color);
                    filled(&[(15.5, 3.5), (13.2, 10.1), (8.9, 5.8)], color);
                }
                Icon::Line => line(&[(3.5, 15.5), (15.5, 3.5)], w, color),
                Icon::RectOutline => {
                    p.rect_stroke(rect_at(2.5, 3.5, 15.5, 13.0), 0.0, stroke, outline);
                }
                _ => {
                    let pts = (0..32).map(|i| {
                        let a = std::f32::consts::TAU * i as f32 / 32.0;
                        at(9.0 + 6.5 * a.cos(), 8.5 + 5.0 * a.sin())
                    });
                    p.add(Shape::closed_line(pts.collect(), stroke));
                }
            }
            if let Some(ink) = ink {
                p.circle(at(16.0, 16.0), 2.6 * s, ink, Stroke::new(1.0 * s, color));
            }
        }
        Icon::Text => {
            line(&[(3.5, 4.0), (14.5, 4.0)], 1.6, color);
            line(&[(9.0, 4.0), (9.0, 16.0)], 1.6, color);
            if let Some(ink) = ink {
                p.circle(at(16.0, 16.0), 2.6 * s, ink, Stroke::new(1.0 * s, color));
            }
        }
        Icon::Crop => {


            line(&[(6.0, 2.5), (6.0, 14.0), (17.5, 14.0)], w, color);
            line(&[(2.5, 6.0), (14.0, 6.0), (14.0, 17.5)], w, color);
        }
        Icon::Check => {
            line(&[(4.0, 10.5), (8.0, 14.5), (16.0, 5.5)], 1.6, color);
        }
        Icon::Close => {
            line(&[(5.0, 5.0), (15.0, 15.0)], 1.5, color);
            line(&[(15.0, 5.0), (5.0, 15.0)], 1.5, color);
        }
        Icon::Undo | Icon::Redo => {
            let flip = |x: f32| if icon == Icon::Redo { 20.0 - x } else { x };
            // Arrow heading left along the top, swinging round on the right
            // and coming back along the bottom.
            let mut path = vec![at(flip(4.0), 7.0)];
            path.extend((0..=12).map(|i| {
                let a = std::f32::consts::FRAC_PI_2 - std::f32::consts::PI * i as f32 / 12.0;
                at(flip(11.5 + 4.5 * a.cos()), 11.5 - 4.5 * a.sin())
            }));
            path.push(at(flip(7.0), 16.0));
            p.add(Shape::line(path, stroke));
            line(&[(flip(7.0), 4.0), (flip(4.0), 7.0), (flip(7.0), 10.0)], w, color);
        }
        Icon::Save => {
            closed(&[(3.5, 3.5), (14.0, 3.5), (16.5, 6.0), (16.5, 16.5), (3.5, 16.5)], w, color);
            p.rect_stroke(rect_at(6.5, 3.5, 12.5, 7.5), 0.0, stroke, outline);
            p.rect_stroke(rect_at(6.0, 11.0, 14.0, 16.5), 0.0, stroke, outline);
        }
        Icon::Copy => {
            p.rect_stroke(rect_at(7.0, 6.5, 16.5, 17.0), 2.0 * s, stroke, outline);
            line(&[(4.0, 13.5), (4.0, 4.5), (5.0, 3.5), (13.0, 3.5)], w, color);
        }
        Icon::Camera => {
            closed(&[(2.5, 6.5), (6.5, 6.5), (8.0, 4.0), (12.0, 4.0), (13.5, 6.5), (17.5, 6.5), (17.5, 16.0), (2.5, 16.0)], w, color);
            p.circle_stroke(at(10.0, 11.0), 3.2 * s, stroke);
        }
        Icon::Video => {
            p.rect_stroke(rect_at(2.5, 5.0, 13.0, 15.0), 2.0 * s, stroke, outline);
            closed(&[(13.0, 9.0), (17.5, 6.0), (17.5, 14.0), (13.0, 11.0)], w, color);
        }
        Icon::Mic | Icon::MicOff => {
            p.rect_stroke(rect_at(7.5, 2.5, 12.5, 12.0), 2.5 * s, stroke, outline);
            let arc: Vec<_> = (0..=10)
                .map(|i| {
                    let a = std::f32::consts::PI * i as f32 / 10.0;
                    at(10.0 + 5.0 * a.cos(), 10.0 + 4.5 * a.sin())
                })
                .collect();
            p.add(Shape::line(arc, stroke));
            line(&[(10.0, 14.5), (10.0, 17.5)], w, color);
            if icon == Icon::MicOff {
                line(&[(3.5, 3.0), (16.5, 17.5)], w, color);
            }
        }
        Icon::SystemSound | Icon::SystemSoundOff => {
            line(&[(9.0, 14.0), (2.5, 14.0), (2.5, 3.5), (17.5, 3.5), (17.5, 8.5)], w, color);
            line(&[(5.5, 16.5), (8.5, 16.5)], w, color);
            closed(&[(11.0, 12.0), (12.5, 12.0), (15.0, 10.0), (15.0, 17.5), (12.5, 15.5), (11.0, 15.5)], 1.2, color);
            if icon == Icon::SystemSoundOff {
                line(&[(16.5, 12.0), (19.0, 15.5)], 1.2, color);
                line(&[(19.0, 12.0), (16.5, 15.5)], 1.2, color);
            } else {
                line(&[(17.0, 12.0), (17.8, 13.75), (17.0, 15.5)], 1.2, color);
            }
        }
        Icon::Record => {
            p.circle_stroke(at(10.0, 10.0), 6.5 * s, stroke);
            p.circle_filled(at(10.0, 10.0), 3.8 * s, color);
        }
        Icon::Stop => {
            p.rect_filled(rect_at(5.0, 5.0, 15.0, 15.0), 2.0 * s, color);
        }
        Icon::Play => {
            filled(&[(6.0, 3.5), (16.5, 10.0), (6.0, 16.5)], color);
        }
        Icon::More => {
            for x in [4.5, 10.0, 15.5] {
                p.circle_filled(at(x, 10.0), 1.3 * s, color);
            }
        }
    }
}
