//! The annotation model.
//!
//! Coordinates are stored in *logical window pixels* of the capture overlay,
//! i.e. exactly what GPUI reports for mouse positions. The GDI compositor maps
//! them onto the captured frame when it paints, so a shape drawn at 100% and at
//! 150% lands on the same physical pixels of the shot.

use std::sync::Arc;

/// A point in overlay logical space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

impl Pt {
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
}

/// An axis-aligned rectangle in overlay logical space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    /// Builds a normalized rectangle spanning two drag points.
    pub fn from_corners(a: Pt, b: Pt) -> Self {
        Self {
            x: a.x.min(b.x),
            y: a.y.min(b.y),
            w: (b.x - a.x).abs(),
            h: (b.y - a.y).abs(),
        }
    }

    pub fn right(&self) -> f32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }

    pub fn center(&self) -> Pt {
        Pt::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    pub fn contains(&self, p: Pt) -> bool {
        p.x >= self.x && p.x <= self.right() && p.y >= self.y && p.y <= self.bottom()
    }

    /// Clamps the rectangle to `[0, size]`, keeping a zero minimum extent.
    pub fn clamp(&self, w: f32, h: f32) -> Self {
        let x = self.x.max(0.0).min(w);
        let y = self.y.max(0.0).min(h);
        Self {
            x,
            y,
            w: self.w.min(w - x).max(0.0),
            h: self.h.min(h - y).max(0.0),
        }
    }

    /// Grows the rectangle by `amount` on every side.
    pub fn inflate(&self, amount: f32) -> Self {
        Self::new(
            self.x - amount,
            self.y - amount,
            self.w + amount * 2.0,
            self.h + amount * 2.0,
        )
    }
}

/// A pixelated region produced by the mosaic brush.
///
/// The image is kept next to the geometry it belongs to so that undo, erase and
/// reordering cannot desynchronise the two; it is an `Arc`, so cloning the
/// shape list every frame stays cheap.
#[derive(Debug, Clone)]
pub struct MosaicPreview {
    /// Where the bitmap belongs, in overlay logical pixels.
    pub rect: Rect,
    pub image: Arc<gpui_kit::RenderImage>,
}

/// One committed annotation.
#[derive(Debug, Clone)]
pub enum Shape {
    Rect {
        a: Pt,
        b: Pt,
        color: u32,
        width: f32,
    },
    Ellipse {
        a: Pt,
        b: Pt,
        color: u32,
        width: f32,
    },
    Pen {
        points: Vec<Pt>,
        color: u32,
        width: f32,
    },
    /// A straight line with a solid head, from tail `a` to tip `b`.
    Arrow {
        a: Pt,
        b: Pt,
        color: u32,
        width: f32,
    },
    /// A freehand pixelated stroke: paint anywhere, and that is what gets
    /// mosaic'd — not a rectangle.
    Mosaic {
        points: Vec<Pt>,
        /// Painted width in logical pixels.
        brush: f32,
        /// Block size in logical pixels.
        block: f32,
        preview: Option<MosaicPreview>,
    },
    Text {
        pos: Pt,
        text: String,
        color: u32,
        size: f32,
    },
}

/// How far a stroke has to come to a shape before the eraser counts as a hit.
const ERASE_SLACK: f32 = 3.0;

impl Shape {
    /// Moves the shape by a delta in overlay logical pixels.
    pub fn translate(&mut self, dx: f32, dy: f32) {
        match self {
            Shape::Rect { a, b, .. } | Shape::Ellipse { a, b, .. } | Shape::Arrow { a, b, .. } => {
                a.x += dx;
                a.y += dy;
                b.x += dx;
                b.y += dy;
            }
            Shape::Pen { points, .. } | Shape::Mosaic { points, .. } => {
                for point in points {
                    point.x += dx;
                    point.y += dy;
                }
            }
            Shape::Text { pos, .. } => {
                pos.x += dx;
                pos.y += dy;
            }
        }
    }

    /// Axis-aligned extent of the shape, in overlay logical pixels.
    pub fn bounds(&self) -> Rect {
        match self {
            Shape::Rect { a, b, .. }
            | Shape::Ellipse { a, b, .. }
            | Shape::Arrow { a, b, .. } => Rect::from_corners(*a, *b),
            Shape::Pen { points, .. } | Shape::Mosaic { points, .. } => {
                let slack = match self {
                    Shape::Mosaic { brush, .. } => brush / 2.0,
                    _ => 0.0,
                };
                bounds_of(points).inflate(slack)
            }
            Shape::Text { pos, text, size, .. } => {
                let widest = text.lines().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
                let lines = text.lines().count().max(1) as f32;
                Rect::new(pos.x, pos.y, widest * size * 0.6, lines * size * 1.35)
            }
        }
    }

    /// The colour and stroke width this shape is drawn with.
    pub fn ink(&self) -> (u32, f32) {
        match self {
            Shape::Rect { color, width, .. }
            | Shape::Ellipse { color, width, .. }
            | Shape::Pen { color, width, .. }
            | Shape::Arrow { color, width, .. } => (*color, *width),
            Shape::Mosaic { .. } => (0, 1.0),
            Shape::Text { color, size, .. } => (*color, *size),
        }
    }

    /// Whether a press at `point` should pick this annotation up and move it.
    ///
    /// Hollow shapes — rectangles, ellipses, text — are grabbed anywhere inside
    /// their box, because to the user the box *is* the object. Freehand strokes
    /// and arrows are grabbed only where the ink actually is, so a long
    /// diagonal pencil line does not swallow its whole bounding box.
    pub fn grabbed_by(&self, point: Pt, slack: f32) -> bool {
        match self {
            Shape::Rect { .. } | Shape::Ellipse { .. } | Shape::Text { .. } => {
                self.bounds().inflate(slack).contains(point)
            }
            _ => self.touched_by(point, point, slack),
        }
    }

    /// Whether the eraser stroke `from -> to` touches the shape itself.
    ///
    /// Deliberately geometric rather than a bounding-box test: with a
    /// bounding-box test a long diagonal pencil stroke would be erased by
    /// touching the empty corner of its box, which feels random.
    pub fn touched_by(&self, from: Pt, to: Pt, radius: f32) -> bool {
        let reach = radius + ERASE_SLACK;
        match self {
            Shape::Rect { a, b, .. } => {
                let r = Rect::from_corners(*a, *b);
                let corners = [
                    Pt::new(r.x, r.y),
                    Pt::new(r.right(), r.y),
                    Pt::new(r.right(), r.bottom()),
                    Pt::new(r.x, r.bottom()),
                ];
                (0..4).any(|i| segments_within(from, to, corners[i], corners[(i + 1) % 4], reach))
            }
            Shape::Ellipse { a, b, .. } => {
                let rect = Rect::from_corners(*a, *b);
                let (cx, cy) = (rect.center().x, rect.center().y);
                let (rx, ry) = (rect.w / 2.0, rect.h / 2.0);
                // Sampling the outline is more than accurate enough for an
                // eraser and avoids an exact segment/ellipse solve.
                (0..48).any(|step| {
                    let angle = step as f32 / 48.0 * std::f32::consts::TAU;
                    let point = Pt::new(cx + rx * angle.cos(), cy + ry * angle.sin());
                    point_segment_distance(point, from, to) <= reach
                })
            }
            Shape::Arrow { a, b, .. } => segments_within(from, to, *a, *b, reach),
            Shape::Pen { points, .. } | Shape::Mosaic { points, .. } => {
                points.windows(2).any(|pair| segments_within(from, to, pair[0], pair[1], reach))
                    || points
                        .first()
                        .map(|p| point_segment_distance(*p, from, to) <= reach)
                        .unwrap_or(false)
            }
            Shape::Text { .. } => {
                let r = self.bounds();
                let corners = [
                    Pt::new(r.x, r.y),
                    Pt::new(r.right(), r.y),
                    Pt::new(r.right(), r.bottom()),
                    Pt::new(r.x, r.bottom()),
                ];
                (0..4).any(|i| segments_within(from, to, corners[i], corners[(i + 1) % 4], reach))
            }
        }
    }
}

/// The polyline that traces a shape's outline, for shapes that have one.
///
/// Erasing turns a shape into the pieces of this polyline that survived, which
/// is what makes the eraser feel like a real one instead of a delete tool. The
/// outline is sampled finely enough that a rubber of any size can bite a piece
/// out of a straight edge, not just knock a whole corner off.
pub fn outline_of(shape: &Shape) -> Option<Vec<Pt>> {
    let step = OUTLINE_STEP;
    match shape {
        Shape::Rect { a, b, .. } => {
            let rect = Rect::from_corners(*a, *b);
            Some(densify(
                &[
                    Pt::new(rect.x, rect.y),
                    Pt::new(rect.right(), rect.y),
                    Pt::new(rect.right(), rect.bottom()),
                    Pt::new(rect.x, rect.bottom()),
                    Pt::new(rect.x, rect.y),
                ],
                step,
            ))
        }
        Shape::Ellipse { a, b, .. } => {
            let rect = Rect::from_corners(*a, *b);
            let center = rect.center();
            let (rx, ry) = (rect.w / 2.0, rect.h / 2.0);
            if rx < 0.5 || ry < 0.5 {
                return None;
            }
            const STEPS: usize = 72;
            let ring: Vec<Pt> = (0..=STEPS)
                .map(|sample| {
                    let angle = sample as f32 / STEPS as f32 * std::f32::consts::TAU;
                    Pt::new(center.x + rx * angle.cos(), center.y + ry * angle.sin())
                })
                .collect();
            Some(densify(&ring, step))
        }
        Shape::Arrow { a, b, .. } => Some(densify(&[*a, *b], step)),
        _ => None,
    }
}

/// Longest gap allowed between samples of an outline, in logical pixels.
const OUTLINE_STEP: f32 = 3.0;

/// Inserts intermediate points so that no segment is longer than `max_step`.
///
/// The eraser removes vertices, so an outline made of a handful of corners has
/// to be sampled before it can be bitten into.
fn densify(points: &[Pt], max_step: f32) -> Vec<Pt> {
    let mut sampled: Vec<Pt> = Vec::with_capacity(points.len() * 8);
    let Some(first) = points.first() else {
        return sampled;
    };
    sampled.push(*first);
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let length = (dx * dx + dy * dy).sqrt();
        let steps = (length / max_step).ceil().max(1.0) as usize;
        for step in 1..=steps {
            let t = step as f32 / steps as f32;
            sampled.push(Pt::new(a.x + dx * t, a.y + dy * t));
        }
    }
    sampled
}

/// Whether any segment of `points` comes within `radius` of the eraser stroke.
pub fn polyline_touched(points: &[Pt], from: Pt, to: Pt, radius: f32) -> bool {
    let reach = radius + ERASE_SLACK;
    if points.len() == 1 {
        return point_segment_distance(points[0], from, to) <= reach;
    }
    points
        .windows(2)
        .any(|pair| segments_within(from, to, pair[0], pair[1], reach))
}

/// Bounding box of a point list, empty lists giving a zero rect at the origin.
pub fn bounds_of(points: &[Pt]) -> Rect {
    let mut iter = points.iter().copied();
    let Some(first) = iter.next() else {
        return Rect::new(0.0, 0.0, 0.0, 0.0);
    };
    let mut rect = Rect::new(first.x, first.y, 0.0, 0.0);
    for point in iter {
        let x0 = rect.x.min(point.x);
        let y0 = rect.y.min(point.y);
        let x1 = rect.right().max(point.x);
        let y1 = rect.bottom().max(point.y);
        rect = Rect::new(x0, y0, x1 - x0, y1 - y0);
    }
    rect
}

/// Splits a pencil stroke wherever the eraser crossed it.
///
/// Erasing the middle of a freehand line should leave the two ends behind, not
/// delete the whole stroke.
pub fn split_pen(points: &[Pt], from: Pt, to: Pt, radius: f32) -> Vec<Vec<Pt>> {
    let reach = radius + ERASE_SLACK;
    let mut runs: Vec<Vec<Pt>> = Vec::new();
    let mut current: Vec<Pt> = Vec::new();
    for point in points {
        if point_segment_distance(*point, from, to) <= reach {
            if current.len() >= 2 {
                runs.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        } else {
            current.push(*point);
        }
    }
    if current.len() >= 2 {
        runs.push(current);
    }
    runs
}

fn point_segment_distance(p: Pt, a: Pt, b: Pt) -> f32 {
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let length_squared = dx * dx + dy * dy;
    if length_squared <= f32::EPSILON {
        return ((p.x - a.x).powi(2) + (p.y - a.y).powi(2)).sqrt();
    }
    let t = (((p.x - a.x) * dx + (p.y - a.y) * dy) / length_squared).clamp(0.0, 1.0);
    let (cx, cy) = (a.x + t * dx, a.y + t * dy);
    ((p.x - cx).powi(2) + (p.y - cy).powi(2)).sqrt()
}

/// Whether two segments come within `reach` of each other.
fn segments_within(a1: Pt, a2: Pt, b1: Pt, b2: Pt, reach: f32) -> bool {
    if segments_intersect(a1, a2, b1, b2) {
        return true;
    }
    point_segment_distance(b1, a1, a2) <= reach
        || point_segment_distance(b2, a1, a2) <= reach
        || point_segment_distance(a1, b1, b2) <= reach
        || point_segment_distance(a2, b1, b2) <= reach
}

fn segments_intersect(p1: Pt, p2: Pt, p3: Pt, p4: Pt) -> bool {
    let cross = |a: Pt, b: Pt, c: Pt| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
    let d1 = cross(p3, p4, p1);
    let d2 = cross(p3, p4, p2);
    let d3 = cross(p1, p2, p3);
    let d4 = cross(p1, p2, p4);
    ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0))
}

/// Which drawing tool is armed, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Rect,
    Ellipse,
    Arrow,
    Pen,
    Mosaic,
    Eraser,
    Text,
}

impl Tool {
    pub fn label(self) -> &'static str {
        match self {
            Tool::Rect => "矩形",
            Tool::Ellipse => "圆形",
            Tool::Arrow => "箭头",
            Tool::Pen => "铅笔",
            Tool::Mosaic => "马赛克",
            Tool::Eraser => "橡皮",
            Tool::Text => "文字",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Tool::Rect => "拖动绘制矩形",
            Tool::Ellipse => "拖动绘制椭圆",
            Tool::Arrow => "从起点拖到箭头指向处",
            Tool::Pen => "按住自由手绘",
            Tool::Mosaic => "涂抹需要打码的区域",
            Tool::Eraser => "涂抹需要擦掉的标注",
            Tool::Text => "点击后输入文字",
        }
    }
}

/// The colours offered in the toolbar (0xRRGGBB).
pub const PALETTE: [u32; 6] = [
    0xE53935, // red
    0xFB8C00, // orange
    0xFDD835, // yellow
    0x43A047, // green
    0x1E88E5, // blue
    0x212121, // near-black
];

/// Stroke widths offered in the toolbar, in logical pixels.
pub const WIDTHS: [f32; 3] = [2.0, 4.0, 7.0];

/// Mosaic brush widths, in logical pixels.
pub const BRUSHES: [f32; 3] = [12.0, 22.0, 36.0];

/// Eraser widths, in logical pixels.
pub const ERASERS: [f32; 3] = [14.0, 26.0, 42.0];

/// Converts `0xRRGGBB` into a GDI `COLORREF` (`0x00BBGGRR`).
pub fn colorref(rgb: u32) -> u32 {
    let r = (rgb >> 16) & 0xFF;
    let g = (rgb >> 8) & 0xFF;
    let b = rgb & 0xFF;
    (b << 16) | (g << 8) | r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rectangle() -> Shape {
        // Screen (700, 400) to (1100, 700), matching the shape the eraser is
        // normally used on.
        Shape::Rect {
            a: Pt::new(700.0, 400.0),
            b: Pt::new(1100.0, 700.0),
            color: PALETTE[0],
            width: 4.0,
        }
    }

    /// An outline has to be sampled finely enough for a rubber to take a bite
    /// out of the middle of a straight edge instead of only knocking corners off.
    #[test]
    fn outline_is_sampled_along_every_edge() {
        let outline = outline_of(&rectangle()).expect("rectangles have an outline");
        assert!(outline.len() > 200, "outline had {} points", outline.len());
        // No gap may be longer than the sampling step, or the rubber could slip
        // between two samples on a long edge.
        let longest = outline
            .windows(2)
            .map(|pair| {
                let (dx, dy) = (pair[1].x - pair[0].x, pair[1].y - pair[0].y);
                (dx * dx + dy * dy).sqrt()
            })
            .fold(0.0f32, f32::max);
        assert!(longest <= OUTLINE_STEP + 1e-3, "longest gap was {longest}");
    }

    /// Erasing across the left edge must open a gap there — and nowhere else.
    #[test]
    fn erasing_an_edge_leaves_the_rest_of_the_shape() {
        let shape = rectangle();
        let outline = outline_of(&shape).expect("outline");
        let (color, width) = shape.ink();
        assert_eq!((color, width), (PALETTE[0], 4.0));
        let radius = ERASERS[1] / 2.0;

        // A horizontal rub 40 px to the left of the left edge, sweeping across
        // it at y = 545, exactly like a hand moving the rubber.
        let (from, to) = (Pt::new(660.0, 545.0), Pt::new(745.0, 545.0));
        assert!(polyline_touched(&outline, from, to, radius));

        let runs = split_pen(&outline, from, to, radius);
        assert_eq!(runs.len(), 2, "the edge should be cut in two");

        // Nothing survives from inside the rubbed span...
        let reach = radius + ERASE_SLACK;
        let survivors: Vec<Pt> = runs.iter().flatten().copied().collect();
        assert!(
            survivors
                .iter()
                .all(|p| point_segment_distance(*p, from, to) > reach)
        );

        // ...the two halves still cover the rest of the outline, and the top,
        // right and bottom edges come through untouched.
        let total: f32 = runs
            .iter()
            .map(|run| {
                run.windows(2)
                    .map(|pair| {
                        let (dx, dy) = (pair[1].x - pair[0].x, pair[1].y - pair[0].y);
                        (dx * dx + dy * dy).sqrt()
                    })
                    .sum::<f32>()
            })
            .sum();
        assert!((total - 1400.0).abs() < 4.0 * (radius + ERASE_SLACK) + 4.0);
        for corner in [
            Pt::new(700.0, 400.0),
            Pt::new(1100.0, 400.0),
            Pt::new(1100.0, 700.0),
            Pt::new(700.0, 700.0),
        ] {
            assert!(
                survivors.iter().any(|p| (p.x - corner.x).abs() < 0.01 && (p.y - corner.y).abs() < 0.01),
                "corner {corner:?} was lost"
            );
        }
    }
    /// A shape the rubber never reaches is kept exactly as it was, so it keeps
    /// its identity — and with it its resize handles.
    #[test]
    fn erasing_elsewhere_does_not_disturb_a_shape() {
        let outline = outline_of(&rectangle()).expect("outline");
        assert!(!polyline_touched(
            &outline,
            Pt::new(200.0, 200.0),
            Pt::new(300.0, 200.0),
            ERASERS[2] / 2.0
        ));
    }
}

