//! The notification-area icon, drawn programmatically.
//!
//! Generating the icon at runtime keeps the project free of binary assets and
//! resource compilation, which matters for a tool that should stay small.

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Supersampled drawing canvas; 4x the final icon gives clean edges.
const CANVAS: i32 = 128;
/// Final icon edge length in pixels.
const SIZE: i32 = 32;
const SUPERSAMPLE: i32 = CANVAS / SIZE;

/// The tray/hotkey icon: a blue rounded tile with white capture brackets.
pub fn create() -> Option<HICON> {
    let mut canvas = vec![[0.0f32; 4]; (CANVAS * CANVAS) as usize];

    fill_round_rect(&mut canvas, 8.0, 8.0, 120.0, 120.0, 26.0, [30.0, 136.0, 229.0]);

    // Four corner brackets, like a crop/selection frame.
    let inset = 30.0;
    let arm = 34.0;
    let thickness = 11.0;
    let far = CANVAS as f32 - inset;
    let white = [255.0, 255.0, 255.0];
    let radius = thickness / 2.0;

    let bars = [
        (inset, inset, inset + arm, inset + thickness),
        (inset, inset, inset + thickness, inset + arm),
        (far - arm, inset, far, inset + thickness),
        (far - thickness, inset, far, inset + arm),
        (inset, far - thickness, inset + arm, far),
        (inset, far - arm, inset + thickness, far),
        (far - arm, far - thickness, far, far),
        (far - thickness, far - arm, far, far),
    ];
    for (x0, y0, x1, y1) in bars {
        fill_round_rect(&mut canvas, x0, y0, x1, y1, radius, white);
    }

    let mut pixels = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut alpha = 0.0;
            let mut rgb = [0.0f32; 3];
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let px = canvas[((y * SUPERSAMPLE + sy) * CANVAS + x * SUPERSAMPLE + sx) as usize];
                    alpha += px[3];
                    for c in 0..3 {
                        rgb[c] += px[c] * px[3];
                    }
                }
            }
            let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
            alpha /= samples;
            let mut out = [0u8, 0, 0, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8];
            if alpha > 0.001 {
                for c in 0..3 {
                    // Un-premultiply so edges keep their colour instead of
                    // fading towards black.
                    out[c] = (rgb[c] / samples / alpha).clamp(0.0, 255.0).round() as u8;
                }
            }
            // GDI wants BGRA.
            pixels.extend_from_slice(&[out[2], out[1], out[0], out[3]]);
        }
    }

    unsafe { icon_from_bgra(&pixels) }
}

/// Paints a rounded rectangle with 1x1 supersampled coverage.
fn fill_round_rect(
    canvas: &mut [[f32; 4]],
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    rgb: [f32; 3],
) {
    for y in 0..CANVAS {
        for x in 0..CANVAS {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            if px < x0 || px > x1 || py < y0 || py > y1 {
                continue;
            }
            // Distance outside the inner rectangle that the corners round off.
            let dx = (x0 + radius - px).max(px - (x1 - radius)).max(0.0);
            let dy = (y0 + radius - py).max(py - (y1 - radius)).max(0.0);
            if dx * dx + dy * dy > radius * radius {
                continue;
            }
            // Source-over onto the supersampled canvas.
            let dst = &mut canvas[(y * CANVAS + x) as usize];
            let alpha = 1.0;
            let out_alpha = alpha + dst[3] * (1.0 - alpha);
            for c in 0..3 {
                dst[c] = (rgb[c] * alpha + dst[c] * dst[3] * (1.0 - alpha)) / out_alpha.max(1e-6);
            }
            dst[3] = out_alpha;
        }
    }
}

unsafe fn icon_from_bgra(pixels: &[u8]) -> Option<HICON> {
    unsafe {
        let screen = GetDC(None);
        let hdc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(Some(hdc), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits as *mut u8, pixels.len());

        // Setting every mask bit to 0 tells Windows to take transparency from
        // the colour bitmap's alpha channel.
        let mask_stride = ((SIZE + 15) / 16 * 2) as usize;
        let mask_bits = vec![0u8; mask_stride * SIZE as usize];
        let mask = CreateBitmap(
            SIZE,
            SIZE,
            1,
            1,
            Some(mask_bits.as_ptr() as *const std::ffi::c_void),
        );

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&icon_info).ok();

        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        let _ = DeleteDC(hdc);

        icon
    }
}
