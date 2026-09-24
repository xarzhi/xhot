//! A 32-bit top-down DIB section: the one pixel buffer every stage shares.
//!
//! GDI writes into it (capture, then annotation compositing), and we read the
//! bytes back out for the clipboard and for PNG encoding, so no intermediate
//! copies of a full-screen image are ever needed.

use std::ffi::c_void;
use std::path::Path;

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::*;

/// A memory DC with a 32-bpp top-down `BGRA` bitmap selected into it.
///
/// `Send` is sound: GDI handles are process-wide objects, not thread-affine,
/// and this type hands out `&self`/`&mut self` so two threads can never drive
/// the same DC at once. We need it only so a capture performed on a worker
/// thread can be handed to the UI thread.
pub struct Dib {
    pub hdc: HDC,
    pub bitmap: HBITMAP,
    previous: HGDIOBJ,
    pub width: i32,
    pub height: i32,
    bits: *mut u8,
}

unsafe impl Send for Dib {}
unsafe impl Sync for Dib {}

impl Dib {
    pub fn new(width: i32, height: i32) -> Result<Self> {
        if width <= 0 || height <= 0 {
            bail!("invalid DIB size {width}x{height}");
        }
        unsafe {
            let screen = GetDC(None);
            let hdc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);
            if hdc.is_invalid() {
                bail!("CreateCompatibleDC failed");
            }

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    // Negative height selects a top-down bitmap, which matches
                    // how we index pixels everywhere else.
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits: *mut c_void = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(hdc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                .context("CreateDIBSection")?;
            let previous = SelectObject(hdc, bitmap.into());

            Ok(Self { hdc, bitmap, previous, width, height, bits: bits as *mut u8 })
        }
    }

    pub fn len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn pixels(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.bits, self.len()) }
    }

    pub fn pixels_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, self.len()) }
    }

    /// GDI never writes the alpha byte of a 32-bpp DIB, so after any GDI pass
    /// every pixel claims to be fully transparent. GPUI's compositor honours
    /// that alpha, which would make a freshly captured screen render as
    /// nothing at all, so every GDI pass ends here.
    pub fn opaque_alpha(&mut self) {
        for px in self.pixels_mut().chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
    }

    /// Copies a sub-rectangle into a fresh DIB.
    pub fn crop(&self, x: i32, y: i32, w: i32, h: i32) -> Result<Self> {
        let mut out = Dib::new(w, h)?;
        unsafe {
            BitBlt(out.hdc, 0, 0, w, h, Some(self.hdc), x, y, SRCCOPY).context("BitBlt crop")?;
        }
        out.opaque_alpha();
        Ok(out)
    }

    /// Wraps the pixels as a GPUI-ready image.
    ///
    /// GPUI's `RenderImage` is `image::RgbaImage` whose bytes are actually in
    /// `BGRA` order, which is exactly the DIB layout, so this is a plain move.
    pub fn to_render_image(&self) -> std::sync::Arc<gpui_kit::RenderImage> {
        let buffer = image::RgbaImage::from_raw(self.width as u32, self.height as u32, self.pixels().to_vec())
            .expect("DIB dimensions match its buffer length");
        std::sync::Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(buffer)]))
    }

    /// Encodes the DIB as PNG bytes.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        // `image` expects real RGBA; the DIB holds BGRA.
        let mut rgba = self.pixels().to_vec();
        for px in rgba.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(self.width as u32, self.height as u32, rgba)
            .context("building RGBA buffer")?;

        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(buffer)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .context("encoding PNG")?;
        Ok(png)
    }

    pub fn save_png(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(path, self.to_png()?).with_context(|| format!("writing {}", path.display()))
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.previous);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.hdc);
        }
    }
}

/// Grabs the whole virtual desktop with `BitBlt`.
///
/// `CAPTUREBLT` is required to include layered windows (menus, some overlays);
/// without it those regions come back as holes.
pub fn capture_virtual_screen() -> Result<Dib> {
    let screen = crate::win::util::virtual_screen();
    if screen.is_empty() {
        bail!("no monitor reported a usable virtual screen");
    }

    let mut dib = Dib::new(screen.w, screen.h)?;
    unsafe {
        let dc = GetDC(None);
        let result = BitBlt(
            dib.hdc,
            0,
            0,
            screen.w,
            screen.h,
            Some(dc),
            screen.x,
            screen.y,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        ReleaseDC(None, dc);
        result.context("BitBlt screen capture")?;
    }
    dib.opaque_alpha();
    Ok(dib)
}

/// Creates a `COLORREF` from `0xRRGGBB`.
pub fn colorref(rgb: u32) -> COLORREF {
    COLORREF(crate::annot::colorref(rgb))
}

/// Convenience for the common "select this GDI object, remember the old one"
/// dance, so callers cannot forget to restore it.
pub struct Selected {
    hdc: HDC,
    previous: HGDIOBJ,
}

impl Selected {
    /// # Safety
    /// `object` must be a valid GDI object compatible with `hdc`.
    pub unsafe fn new(hdc: HDC, object: HGDIOBJ) -> Self {
        Self { hdc, previous: unsafe { SelectObject(hdc, object) } }
    }
}

impl Drop for Selected {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.hdc, self.previous);
        }
    }
}

/// Deletes a GDI object when it goes out of scope.
pub struct OwnedObject<T: Copy + Into<HGDIOBJ>>(pub T);

impl<T: Copy + Into<HGDIOBJ>> Drop for OwnedObject<T> {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.0.into());
        }
    }
}
