//! GDI compositing of annotations onto a captured frame.
//!
//! Using GDI here rather than a hand-written rasterizer buys correct anti-
//! aliased text (including CJK), round-capped freehand strokes and cheap
//! ellipse outlines — for essentially no code and no extra dependencies.

use crate::annot::{Pt, Shape};
use crate::win::frame::{Dib, OwnedObject, Selected, colorref};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Gdi::*;
use windows::core::w;

/// A geometric pen: `PS_GEOMETRIC` gives us `LOGBRUSH` colouring plus round
/// end caps and joins, which is what makes freehand strokes look smooth.
const ROUND_PEN: PEN_STYLE = PEN_STYLE(0x0001_0000);

/// Multiplier applied to the font size to get a comfortable line pitch.
const LINE_PITCH: f32 = 1.35;

/// How long an arrow head is, as a multiple of the stroke width.
const ARROW_HEAD_LENGTH: f32 = 5.0;
/// Half-width of the arrow head, as a multiple of the stroke width.
const ARROW_HEAD_HALF_WIDTH: f32 = 2.1;

/// Draws every shape into `dib`.
///
/// `scale` converts overlay logical pixels to device pixels on each axis; the
/// two can differ slightly because a window's client area is not always the
/// size we asked for. `offset` is the device-pixel origin of the region `dib`
/// represents (the crop origin), subtracted so shapes are painted in crop-local
/// coordinates.
pub fn draw_shapes(dib: &mut Dib, shapes: &[Shape], scale: (f32, f32), offset: (f32, f32)) {
    unsafe {
        let _ = SetBkMode(dib.hdc, TRANSPARENT);
    }
    for shape in shapes {
        draw_shape(dib, shape, scale, offset);
    }
    // GDI leaves the alpha byte at 0 in every pixel it touched.
    dib.opaque_alpha();
}

fn project(p: Pt, scale: (f32, f32), offset: (f32, f32)) -> (i32, i32) {
    (
        (p.x * scale.0 + offset.0).round() as i32,
        (p.y * scale.1 + offset.1).round() as i32,
    )
}

fn stroke_width(width: f32, scale: (f32, f32)) -> u32 {
    (width * scale.0.max(scale.1)).round().max(1.0) as u32
}

fn round_pen(width: u32, color: u32) -> OwnedObject<HPEN> {
    unsafe {
        OwnedObject(ExtCreatePen(
            ROUND_PEN,
            width,
            &LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: colorref(color),
                lbHatch: 0,
            },
            None,
        ))
    }
}

fn draw_shape(dib: &mut Dib, shape: &Shape, scale: (f32, f32), offset: (f32, f32)) {
    match shape {
        Shape::Rect { a, b, color, width } => {
            let (x0, y0) = project(*a, scale, offset);
            let (x1, y1) = project(*b, scale, offset);
            unsafe {
                let pen = round_pen(stroke_width(*width, scale), *color);
                let _pen = Selected::new(dib.hdc, pen.0.into());
                let hollow = OwnedObject(GetStockObject(HOLLOW_BRUSH));
                let _brush = Selected::new(dib.hdc, hollow.0);
                let _ = Rectangle(dib.hdc, x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1));
            }
        }
        Shape::Ellipse { a, b, color, width } => {
            let (x0, y0) = project(*a, scale, offset);
            let (x1, y1) = project(*b, scale, offset);
            unsafe {
                let pen = round_pen(stroke_width(*width, scale), *color);
                let _pen = Selected::new(dib.hdc, pen.0.into());
                let hollow = OwnedObject(GetStockObject(HOLLOW_BRUSH));
                let _brush = Selected::new(dib.hdc, hollow.0);
                let _ = Ellipse(dib.hdc, x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1));
            }
        }
        Shape::Pen { points, color, width } => {
            let thickness = stroke_width(*width, scale);
            let projected: Vec<POINT> = points
                .iter()
                .map(|p| {
                    let (x, y) = project(*p, scale, offset);
                    POINT { x, y }
                })
                .collect();
            unsafe {
                let pen = round_pen(thickness, *color);
                let _pen = Selected::new(dib.hdc, pen.0.into());
                if projected.len() == 1 {
                    // A tap with the pencil still deserves a dot.
                    let r = thickness as i32 / 2;
                    let p = projected[0];
                    let brush = OwnedObject(CreateSolidBrush(colorref(*color)));
                    let _brush = Selected::new(dib.hdc, brush.0.into());
                    let _ = Ellipse(dib.hdc, p.x - r, p.y - r, p.x + r, p.y + r);
                } else if projected.len() > 1 {
                    let _ = Polyline(dib.hdc, &projected);
                }
            }
        }
        Shape::Arrow { a, b, color, width } => {
            let thickness = stroke_width(*width, scale);
            let (tail_x, tail_y) = project(*a, scale, offset);
            let (tip_x, tip_y) = project(*b, scale, offset);

            let dx = (tip_x - tail_x) as f32;
            let dy = (tip_y - tail_y) as f32;
            let length = (dx * dx + dy * dy).sqrt();
            if length < 1.0 {
                return;
            }
            let head = (thickness as f32 * ARROW_HEAD_LENGTH).min(length * 0.6).max(6.0);
            let half = head * ARROW_HEAD_HALF_WIDTH / ARROW_HEAD_LENGTH;
            let (ux, uy) = (dx / length, dy / length);
            // Stop the shaft short of the tip so it does not poke through the
            // solid head.
            let shaft_x = tip_x - (ux * head) as i32;
            let shaft_y = tip_y - (uy * head) as i32;

            unsafe {
                let pen = round_pen(thickness, *color);
                let _pen = Selected::new(dib.hdc, pen.0.into());
                let _ = MoveToEx(dib.hdc, tail_x, tail_y, None);
                let _ = LineTo(dib.hdc, shaft_x, shaft_y);

                let head_points = [
                    POINT { x: tip_x, y: tip_y },
                    POINT {
                        x: (tip_x as f32 - ux * head - uy * half).round() as i32,
                        y: (tip_y as f32 - uy * head + ux * half).round() as i32,
                    },
                    POINT {
                        x: (tip_x as f32 - ux * head + uy * half).round() as i32,
                        y: (tip_y as f32 - uy * head - ux * half).round() as i32,
                    },
                ];
                let brush = OwnedObject(CreateSolidBrush(colorref(*color)));
                let _brush = Selected::new(dib.hdc, brush.0.into());
                let _ = Polygon(dib.hdc, &head_points);
            }
        }
        Shape::Mosaic { points, brush, block, .. } => {
            // Kept in screen space (no crop offset) so every mosaic stroke in
            // one capture lands on the same block grid.
            let projected: Vec<(f32, f32)> = points
                .iter()
                .map(|p| (p.x * scale.0, p.y * scale.1))
                .collect();
            pixelate_stroke(
                dib,
                &projected,
                (brush * scale.0).round().max(2.0) as i32,
                (block * scale.0).round().max(2.0) as i32,
                (-offset.0.round() as i32, -offset.1.round() as i32),
            );
        }
        Shape::Text { pos, text, color, size } => {
            let (x, y) = project(*pos, scale, offset);
            let height = (size * scale.1).round().max(6.0) as i32;
            unsafe {
                let font = OwnedObject(CreateFontW(
                    -height,
                    0,
                    0,
                    0,
                    400, // FW_NORMAL
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_TT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    0,
                    w!("Microsoft YaHei"),
                ));
                let _font = Selected::new(dib.hdc, font.0.into());
                SetTextColor(dib.hdc, colorref(*color));
                let pitch = (height as f32 * LINE_PITCH).round() as i32;
                for (index, line) in text.lines().enumerate() {
                    let encoded: Vec<u16> = line.encode_utf16().collect();
                    let _ = TextOutW(dib.hdc, x, y + pitch * index as i32, &encoded);
                }
            }
        }
    }
}

/// Pixelates everything within `brush` pixels of a freehand stroke.
///
/// Only the cells the brush actually covers are touched, so this behaves like
/// painting with a mosaic pen rather than dragging out a rectangle. The source
/// region is snapshotted first: averages have to come from the untouched
/// pixels, otherwise a cell that straddles two brush passes would be averaged
/// with an already-pixelated neighbour and turn to mush.
///
/// `stroke` and `grid_origin` are expressed in one shared coordinate space —
/// screen pixels — where `grid_origin` is the position of `dib`'s own pixel
/// (0, 0). Anchoring the block grid to that space instead of to the stroke's
/// bounding box is what keeps separate strokes on one grid and makes the live
/// preview and the exported image agree cell for cell.
pub fn pixelate_stroke(
    dib: &mut Dib,
    stroke: &[(f32, f32)],
    brush: i32,
    block: i32,
    grid_origin: (i32, i32),
) {
    if stroke.is_empty() {
        return;
    }
    let points: Vec<POINT> = stroke
        .iter()
        .map(|(x, y)| POINT {
            x: x.round() as i32,
            y: y.round() as i32,
        })
        .collect();
    let points = &points[..];
    let block = block.max(2);
    let radius = (brush.max(2) / 2).max(1);

    let mut min_x = i32::MAX;
    let mut min_y = i32::MAX;
    let mut max_x = i32::MIN;
    let mut max_y = i32::MIN;
    for point in points {
        min_x = min_x.min(point.x - radius);
        max_x = max_x.max(point.x + radius);
        min_y = min_y.min(point.y - radius);
        max_y = max_y.max(point.y + radius);
    }

    let snap = |value: i32| value.div_euclid(block) * block;
    let first_x = snap(min_x).max(snap(grid_origin.0));
    let first_y = snap(min_y).max(snap(grid_origin.1));
    let last_x = snap(max_x).min(snap(grid_origin.0 + dib.width - 1));
    let last_y = snap(max_y).min(snap(grid_origin.1 + dib.height - 1));
    if last_x < first_x || last_y < first_y {
        return;
    }

    // Local window of the snapshot, covering every cell the loop can reach.
    let left = (first_x - grid_origin.0).max(0);
    let top = (first_y - grid_origin.1).max(0);
    let right = (last_x + block - grid_origin.0).min(dib.width);
    let bottom = (last_y + block - grid_origin.1).min(dib.height);
    let (width, height) = (right - left, bottom - top);
    if width <= 0 || height <= 0 {
        return;
    }

    let stride = dib.width as usize * 4;
    let source: Vec<u8> = {
        let pixels = dib.pixels();
        let mut buffer = Vec::with_capacity(width as usize * height as usize * 4);
        for row in top..bottom {
            let start = row as usize * stride + left as usize * 4;
            buffer.extend_from_slice(&pixels[start..start + width as usize * 4]);
        }
        buffer
    };

    let row_stride = width as usize * 4;
    let (dib_width, dib_height) = (dib.width, dib.height);
    let output = dib.pixels_mut();
    let cells_x = (last_x - first_x) / block + 1;
    let cells_y = (last_y - first_y) / block + 1;
    for cell_iy in 0..cells_y {
        let cell_y = first_y + cell_iy * block;
        let y0 = (cell_y - grid_origin.1).max(0) - top;
        let y1 = (cell_y + block - grid_origin.1).min(dib_height) - top;
        for cell_ix in 0..cells_x {
            let cell_x = first_x + cell_ix * block;
            let x0 = (cell_x - grid_origin.0).max(0) - left;
            let x1 = (cell_x + block - grid_origin.0).min(dib_width) - left;
            if x1 <= x0 || y1 <= y0 {
                continue;
            }

            // A round brush: the cell joins the mosaic when its centre is
            // within reach of the stroke.
            let centre = POINT {
                x: cell_x + block / 2,
                y: cell_y + block / 2,
            };
            if !stroke_covers(points, centre, radius) {
                continue;
            }

            let mut hash = [0u32; 3];
            let mut count = 0u32;
            for row in y0..y1 {
                for column in x0..x1 {
                    let index = row as usize * row_stride + column as usize * 4;
                    hash[0] += source[index] as u32;
                    hash[1] += source[index + 1] as u32;
                    hash[2] += source[index + 2] as u32;
                    count += 1;
                }
            }
            if count == 0 {
                continue;
            }
            let average = [
                (hash[0] / count) as u8,
                (hash[1] / count) as u8,
                (hash[2] / count) as u8,
            ];
            for row in y0..y1 {
                for column in x0..x1 {
                    let target =
                        (top + row) as usize * stride + (left + column) as usize * 4;
                    output[target] = average[0];
                    output[target + 1] = average[1];
                    output[target + 2] = average[2];
                    output[target + 3] = 0xFF;
                }
            }
        }
    }
}

/// Whether any segment of the stroke passes within `radius` of `point`.
fn stroke_covers(points: &[POINT], point: POINT, radius: i32) -> bool {
    if points.len() == 1 {
        return squared_distance(point, points[0]) <= (radius * radius) as f64;
    }
    points
        .windows(2)
        .any(|pair| distance_to_segment(point, pair[0], pair[1]) <= (radius * radius) as f64)
}

fn squared_distance(a: POINT, b: POINT) -> f64 {
    let (dx, dy) = ((a.x - b.x) as f64, (a.y - b.y) as f64);
    dx * dx + dy * dy
}

fn distance_to_segment(p: POINT, a: POINT, b: POINT) -> f64 {
    let (dx, dy) = ((b.x - a.x) as f64, (b.y - a.y) as f64);
    let length_squared = dx * dx + dy * dy;
    if length_squared <= f64::EPSILON {
        return squared_distance(p, a);
    }
    let t = (((p.x - a.x) as f64 * dx + (p.y - a.y) as f64 * dy) / length_squared).clamp(0.0, 1.0);
    let (cx, cy) = (a.x as f64 + t * dx, a.y as f64 + t * dy);
    let (ex, ey) = (p.x as f64 - cx, p.y as f64 - cy);
    ex * ex + ey * ey
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic, non-uniform fill, so untouched pixels are recognisable.
    fn pattern(x: i32, y: i32) -> (u8, u8, u8) {
        let seed = (x as u32).wrapping_mul(2_654_435_761) ^ (y as u32).wrapping_mul(40_503);
        (seed as u8, (seed >> 8) as u8, (seed >> 16) as u8)
    }

    fn filled(width: i32, height: i32) -> Dib {
        let mut dib = Dib::new(width, height).expect("dib");
        let pixels = dib.pixels_mut();
        for y in 0..height {
            for x in 0..width {
                let (r, g, b) = pattern(x, y);
                let index = (y as usize * width as usize + x as usize) * 4;
                pixels[index] = r;
                pixels[index + 1] = g;
                pixels[index + 2] = b;
                pixels[index + 3] = 0xFF;
            }
        }
        dib
    }

    fn pixel(dib: &Dib, x: i32, y: i32) -> (u8, u8, u8) {
        let pixels = dib.pixels();
        let index = (y as usize * dib.width as usize + x as usize) * 4;
        (pixels[index], pixels[index + 1], pixels[index + 2])
    }

    /// Whether every pixel of a block-aligned cell, clipped to the bitmap, is
    /// the same colour. That is what "this is a mosaic cell" means.
    fn cell_is_uniform(dib: &Dib, cell_x: i32, cell_y: i32, block: i32, origin: (i32, i32)) -> bool {
        let x0 = cell_x.max(origin.0);
        let y0 = cell_y.max(origin.1);
        let x1 = (cell_x + block).min(origin.0 + dib.width);
        let y1 = (cell_y + block).min(origin.1 + dib.height);
        let first = pixel(dib, x0 - origin.0, y0 - origin.1);
        for y in y0..y1 {
            for x in x0..x1 {
                if pixel(dib, x - origin.0, y - origin.1) != first {
                    return false;
                }
            }
        }
        true
    }

    /// The mosaic grid has to live in screen space, not in the coordinates of
    /// whichever bitmap a stroke happens to be rasterised into.
    ///
    /// Two things depend on it: separate strokes of one capture must share a
    /// grid, and the live preview — which rasterises a small cropped tile — must
    /// agree block for block with the exported image, which rasterises the whole
    /// crop. Anchoring to either bitmap's own origin breaks both, so the tile
    /// origin here is deliberately not a multiple of the block size.
    #[test]
    fn mosaic_blocks_follow_the_screen_grid() {
        let origin = (37, 11);
        let block = 10;
        let brush = 22;
        let mut dib = filled(120, 60);
        let before = dib.pixels().to_vec();

        pixelate_stroke(&mut dib, &[(20.0, 30.0), (100.0, 30.0)], brush, block, origin);

        // Cells are laid out on multiples of `block` in screen space; with this
        // origin the first cell therefore starts at (30, 10).
        let mut checked = 0;
        for cell_y in [20, 30] {
            for cell_x in (30..=100).step_by(block as usize) {
                let (centre_x, centre_y) = (cell_x + block / 2, cell_y + block / 2);
                // The brush covers a cell exactly when its centre is within the
                // radius of the stroke, so these cells are all fully painted.
                if centre_y != 25 && centre_y != 35 {
                    continue;
                }
                if !(9..=111).contains(&centre_x) {
                    continue;
                }
                checked += 1;
                assert!(
                    cell_is_uniform(&dib, cell_x, cell_y, block, origin),
                    "cell at ({cell_x}, {cell_y}) is not one mosaic block"
                );
            }
        }
        assert!(checked >= 12, "only {checked} cells were checked");

        // Rows the brush never reached keep the original pixels.
        for (x, y) in [(40, 55), (100, 5), (110, 30)] {
            let index = (y as usize * dib.width as usize + x as usize) * 4;
            assert_eq!(
                &dib.pixels()[index..index + 3],
                &before[index..index + 3],
                "pixel ({x}, {y}) should not have been touched"
            );
        }
        // And the band really was painted.
        assert_ne!(dib.pixels(), before.as_slice());
    }
}

