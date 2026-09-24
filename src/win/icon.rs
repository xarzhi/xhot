//! The notification-area icon, drawn programmatically.
//!
//! Generating the icon at runtime keeps the project free of binary assets and
//! resource compilation, which matters for a tool that should stay small. The
//! same drawing is also what produces `assets/icon.ico` for installers, via
//! `xshot --export-icon`, so there is exactly one description of the icon.

use std::path::Path;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Supersampling factor: the icon is drawn this much larger and box-filtered
/// down, which is what gives the rounded edges and thin brackets their antialiasing.
const SUPERSAMPLE: i32 = 4;
/// The geometry below is expressed in this many units across, so one drawing
/// serves every output size.
const DESIGN: f32 = 128.0;
/// Final tray icon edge length in pixels.
const SIZE: i32 = 32;

/// The tray/hotkey icon: a blue rounded tile with white capture brackets.
pub fn create() -> Option<HICON> {
    let rgba = rasterize(SIZE);
    // GDI wants BGRA.
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    unsafe { icon_from_bgra(&bgra, SIZE) }
}

/// Renders the icon at `size` pixels as straight (non-premultiplied) RGBA.
pub fn rasterize(size: i32) -> Vec<u8> {
    let edge = size * SUPERSAMPLE;
    let scale = edge as f32 / DESIGN;
    let mut canvas = vec![[0.0f32; 4]; (edge * edge) as usize];

    fill_round_rect(&mut canvas, edge, 8.0 * scale, 8.0 * scale, 120.0 * scale, 120.0 * scale, 26.0 * scale, [30.0, 136.0, 229.0]);

    // Four corner brackets, like a crop/selection frame.
    let inset = 30.0 * scale;
    let arm = 34.0 * scale;
    let thickness = 11.0 * scale;
    let far = edge as f32 - inset;
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
        fill_round_rect(&mut canvas, edge, x0, y0, x1, y1, radius, white);
    }

    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let mut alpha = 0.0;
            let mut rgb = [0.0f32; 3];
            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let px = canvas[((y * SUPERSAMPLE + sy) * edge + x * SUPERSAMPLE + sx) as usize];
                    alpha += px[3];
                    for c in 0..3 {
                        rgb[c] += px[c] * px[3];
                    }
                }
            }
            let samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;
            alpha /= samples;
            let mut out = [(alpha.clamp(0.0, 1.0) * 255.0).round() as u8, 0, 0, 0];
            if alpha > 0.001 {
                for c in 0..3 {
                    // Un-premultiply so edges keep their colour instead of
                    // fading towards black.
                    out[c] = (rgb[c] / samples / alpha).clamp(0.0, 255.0).round() as u8;
                }
            }
            pixels.extend_from_slice(&out);
        }
    }
    pixels
}

/// Writes the multi-resolution `assets/icon.ico` that installers want.
///
/// Windows picks a different size for the taskbar, the Start menu and Explorer,
/// so one resolution is not enough.
pub fn write_ico(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut images = Vec::new();
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let rgba = rasterize(size as i32);
        let buffer = image::RgbaImage::from_raw(size, size, rgba)
            .expect("icon raster matches its dimensions");
        // The ICO encoder wants to know how big each frame is; it cannot infer
        // it from the pixel data alone.
        images.push(image::codecs::ico::IcoFrame::as_png(
            &buffer,
            size,
            size,
            image::ExtendedColorType::Rgba8,
        )?);
    }
    let file = std::fs::File::create(path)?;
    image::codecs::ico::IcoEncoder::new(std::io::BufWriter::new(file))
        .encode_images(&images)?;
    Ok(())
}

/// Paints a rounded rectangle with 1x1 supersampled coverage.
fn fill_round_rect(
    canvas: &mut [[f32; 4]],
    edge: i32,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radius: f32,
    rgb: [f32; 3],
) {
    for y in 0..edge {
        for x in 0..edge {
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
            let dst = &mut canvas[(y * edge + x) as usize];
            let alpha = 1.0;
            let out_alpha = alpha + dst[3] * (1.0 - alpha);
            for c in 0..3 {
                dst[c] = (rgb[c] * alpha + dst[c] * dst[3] * (1.0 - alpha)) / out_alpha.max(1e-6);
            }
            dst[3] = out_alpha;
        }
    }
}

unsafe fn icon_from_bgra(pixels: &[u8], size: i32) -> Option<HICON> {
    unsafe {
        let screen = GetDC(None);
        let hdc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
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
        let mask_stride = ((size + 15) / 16 * 2) as usize;
        let mask_bits = vec![0u8; mask_stride * size as usize];
        let mask = CreateBitmap(
            size,
            size,
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
