//! The application icon, taken from `assets/logo.png`.
//!
//! One file is the source for all four places an icon appears: the notification
//! area (an `HICON` built here), the executable (embedded by `build.rs` from
//! `assets/icon.ico`), the installer, and the shortcuts it creates. Regenerating
//! `assets/icon.ico` from the logo — `xshot --export-icon assets/icon.ico` — is
//! therefore the only step needed to rebrand, and the four cannot drift apart.

use std::path::Path;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The logo, as shipped.
const LOGO: &[u8] = include_bytes!("../assets/logo.png");

/// The tray/hotkey icon.
pub fn create() -> Option<HICON> {
    // Asking Windows for the size it is about to draw at means it never has to
    // rescale the bitmap, which is what keeps a small icon from going soft.
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.clamp(16, 256);
    let rgba = rasterize(size);
    // GDI wants BGRA.
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    unsafe { icon_from_bgra(&bgra, size) }
}

/// Renders the logo at `size` pixels as straight (non-premultiplied) RGBA.
pub fn rasterize(size: i32) -> Vec<u8> {
    let size = size.clamp(1, 1024) as u32;
    let logo = image::load_from_memory(LOGO)
        .expect("assets/logo.png decodes")
        .to_rgba8();
    if logo.width() == size && logo.height() == size {
        return logo.into_raw();
    }
    // Lanczos keeps the thin strokes of the mark readable when it is scaled all
    // the way down to a 16 px tray slot.
    image::imageops::resize(&logo, size, size, image::imageops::FilterType::Lanczos3).into_raw()
}

/// Writes the multi-resolution `assets/icon.ico` that Windows and the installer
/// want.
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
    image::codecs::ico::IcoEncoder::new(std::io::BufWriter::new(file)).encode_images(&images)?;
    Ok(())
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
