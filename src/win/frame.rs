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

    /// Refills this bitmap from the screen, without reallocating it.
    ///
    /// A session grabs the same rectangle dozens of times a second, and a fresh
    /// GDI section per frame costs as much as the copy does.
    pub fn grab_from_screen(&mut self, x: i32, y: i32) -> Result<()> {
        unsafe {
            let dc = GetDC(None);
            let result = BitBlt(
                self.hdc,
                0,
                0,
                self.width,
                self.height,
                Some(dc),
                x,
                y,
                ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
            );
            ReleaseDC(None, dc);
            result.context("BitBlt region capture")?;
        }
        self.opaque_alpha();
        Ok(())
    }

    /// Copies another bitmap of the same size over this one.
    ///
    /// Used to hand out the desktop copy the duplication module keeps: a straight
    /// slice copy of bytes that are already in cache, rather than a GDI blit.
    pub fn copy_from(&mut self, other: &Dib) -> Result<()> {
        if self.width != other.width || self.height != other.height {
            bail!(
                "cannot copy a {}x{} bitmap into a {}x{} one",
                other.width,
                other.height,
                self.width,
                self.height
            );
        }
        self.pixels_mut().copy_from_slice(other.pixels());
        Ok(())
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
        crate::timing("c5 copy start");
        let buffer = image::RgbaImage::from_raw(self.width as u32, self.height as u32, self.pixels().to_vec())
            .expect("DIB dimensions match its buffer length");
        let image = std::sync::Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(buffer)]));
        crate::timing("c6 copy done");
        image
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

/// Grabs the whole virtual desktop.
///
/// The pixels come off the GPU through [`crate::win::duplication`] when it is
/// available, which is most of the time and about ten times faster than GDI;
/// `BitBlt` is the fallback, and `CAPTUREBLT` is what makes *it* include layered
/// windows (menus, some overlays) instead of leaving holes where they were.
pub fn capture_virtual_screen() -> Result<Dib> {
    let screen = crate::win::util::virtual_screen();
    if screen.is_empty() {
        bail!("no monitor reported a usable virtual screen");
    }
    crate::timing("c1 virtual screen known");

    let mut dib = Dib::new(screen.w, screen.h)?;
    crate::timing("c2 dib allocated");
    if crate::win::duplication::capture_into(&mut dib) {
        crate::timing("c3 duplicated from the gpu");
    } else {
        bitblt_into(&mut dib, screen.x, screen.y)?;
        // The GPU path keeps its own copy of the desktop to answer a still
        // screen with; this is the same picture, so it can start from here.
        crate::win::duplication::adopt(&dib);
        crate::timing("c3 bitblt done");
    }
    dib.opaque_alpha();
    crate::timing("c4 alpha done");
    Ok(dib)
}

/// The GDI fallback: one `BitBlt` from the screen DC, at a screen position.
///
/// Also what the duplication module primes itself with, which is why it is
/// visible to it.
pub(crate) fn bitblt_into(dib: &mut Dib, x: i32, y: i32) -> Result<()> {
    unsafe {
        let dc = GetDC(None);
        let result = BitBlt(
            dib.hdc,
            0,
            0,
            dib.width,
            dib.height,
            Some(dc),
            x,
            y,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );
        ReleaseDC(None, dc);
        result.context("BitBlt screen capture")?;
    }
    Ok(())
}

/// Grabs one region of the screen, at its own size.
///
/// The shape a capture session wants: the same rectangle, over and over, while
/// the user drives the window underneath it.
pub fn capture_region(x: i32, y: i32, w: i32, h: i32) -> Result<Dib> {
    let mut dib = Dib::new(w, h)?;
    dib.grab_from_screen(x, y)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Times the ways a full-desktop grab can be done, so the capture path is
    /// chosen from measurement rather than taste.
    ///
    /// A measurement, not a check: `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not a check"]
    fn capture_variants() {
        let screen = crate::win::util::virtual_screen();
        let (w, h) = (screen.w, screen.h);

        let times = |label: &str, mut run: Box<dyn FnMut() + '_>| {
            // One warm-up, then the best of three: the first BitBlt after the
            // desktop has been idle pays for the compositor waking up.
            run();
            let mut best = f64::MAX;
            for _ in 0..3 {
                let start = Instant::now();
                run();
                best = best.min(start.elapsed().as_secs_f64() * 1000.0);
            }
            println!("{label:<34} {best:>7.2} ms");
        };

        times("whole screen, CAPTUREBLT", Box::new(|| {
            let dib = Dib::new(w, h).unwrap();
            unsafe {
                let dc = GetDC(None);
                let _ = BitBlt(
                    dib.hdc,
                    0,
                    0,
                    w,
                    h,
                    Some(dc),
                    screen.x,
                    screen.y,
                    ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
                );
                ReleaseDC(None, dc);
            }
            std::hint::black_box(&dib);
        }));

        times("whole screen, plain SRCCOPY", Box::new(|| {
            let dib = Dib::new(w, h).unwrap();
            unsafe {
                let dc = GetDC(None);
                let _ = BitBlt(dib.hdc, 0, 0, w, h, Some(dc), screen.x, screen.y, SRCCOPY);
                ReleaseDC(None, dc);
            }
            std::hint::black_box(&dib);
        }));

        times("allocation alone", Box::new(|| {
            let dib = Dib::new(w, h).unwrap();
            std::hint::black_box(&dib);
        }));

        times("to_render_image copy", Box::new(|| {
            let dib = Dib::new(w, h).unwrap();
            std::hint::black_box(dib.to_render_image());
        }));

        // Four bands, each on its own thread with its own DC and its own DIB.
        times("4 threads, CAPTUREBLT bands", Box::new(|| {
            let mut bands: Vec<Dib> = (0..4)
                .map(|i| Dib::new(w, ((h + 3) / 4).min(h - i * (h / 4))).unwrap())
                .collect();
            std::thread::scope(|scope| {
                for (index, dib) in bands.iter_mut().enumerate() {
                    let top = screen.y + index as i32 * (h / 4);
                    let height = dib.height;
                    scope.spawn(move || unsafe {
                        let dc = GetDC(None);
                        let _ = BitBlt(
                            dib.hdc,
                            0,
                            0,
                            w,
                            height,
                            Some(dc),
                            screen.x,
                            top,
                            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
                        );
                        ReleaseDC(None, dc);
                    });
                }
            });
            std::hint::black_box(&bands);
        }));

        times("2 threads, CAPTUREBLT halves", Box::new(|| {
            let mut bands: Vec<Dib> = (0..2)
                .map(|i| Dib::new(w, if i == 0 { h / 2 } else { h - h / 2 }).unwrap())
                .collect();
            std::thread::scope(|scope| {
                for (index, dib) in bands.iter_mut().enumerate() {
                    let top = screen.y + index as i32 * (h / 2);
                    let height = dib.height;
                    scope.spawn(move || unsafe {
                        let dc = GetDC(None);
                        let _ = BitBlt(
                            dib.hdc,
                            0,
                            0,
                            w,
                            height,
                            Some(dc),
                            screen.x,
                            top,
                            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
                        );
                        ReleaseDC(None, dc);
                    });
                }
            });
            std::hint::black_box(&bands);
        }));
    }

    /// Times the other way to read the desktop: DXGI desktop duplication, which
    /// hands back a GPU texture the driver then copies to a staging buffer.
    ///
    /// A measurement, not a check: `cargo test --release -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not a check"]
    fn duplication_variant() {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
            D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        };
        use windows::Win32::Graphics::Dxgi::{
            DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIDevice, IDXGIOutput1,
            IDXGIResource,
        };
        use windows::core::Interface as _;

        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut level = D3D_FEATURE_LEVEL::default();
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
            .expect("D3D11CreateDevice");
            let device = device.unwrap();
            let context = context.unwrap();

            let dxgi_device: IDXGIDevice = device.cast().expect("IDXGIDevice");
            let adapter = dxgi_device.GetAdapter().expect("GetAdapter");
            let output = adapter.EnumOutputs(0).expect("EnumOutputs");
            let output1: IDXGIOutput1 = output.cast().expect("IDXGIOutput1");
            let duplication = output1.DuplicateOutput(&device).expect("DuplicateOutput");
            println!("duplication ready");

            let mut staging: Option<ID3D11Texture2D> = None;
            let screen = crate::win::util::virtual_screen();
            let mut dib = Dib::new(screen.w, screen.h).unwrap();

            for attempt in 0..4 {
                let start = Instant::now();
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;
                let acquired = duplication.AcquireNextFrame(500, &mut info, &mut resource);
                let waited = start.elapsed().as_secs_f64() * 1000.0;
                let (resource, texture) = match acquired {
                    Ok(()) => {
                        let resource = resource.expect("resource");
                        let texture: ID3D11Texture2D = resource.cast().expect("texture");
                        (Some(resource), Some(texture))
                    }
                    Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                        println!("  attempt {attempt}: no new frame after {waited:.2} ms");
                        (None, None)
                    }
                    Err(error) => {
                        println!("  attempt {attempt}: acquire failed: {error}");
                        (None, None)
                    }
                };
                let Some(resource) = resource else { continue };
                let texture = texture.unwrap();

                let mut desc = D3D11_TEXTURE2D_DESC::default();
                texture.GetDesc(&mut desc);
                desc.Usage = D3D11_USAGE_STAGING;
                desc.BindFlags = 0;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.MiscFlags = 0;
                desc.ArraySize = 1;
                desc.MipLevels = 1;
                desc.SampleDesc.Count = 1;
                desc.SampleDesc.Quality = 0;
                if staging.is_none() {
                    let mut created = None;
                    device
                        .CreateTexture2D(&desc, None, Some(&mut created))
                        .expect("staging texture");
                    staging = created;
                }
                let staging = staging.as_ref().unwrap();

                let copy_start = Instant::now();
                context.CopyResource(staging, &texture);
                let copied = copy_start.elapsed().as_secs_f64() * 1000.0;

                let map_start = Instant::now();
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context
                    .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .expect("Map");
                let mapped_ms = map_start.elapsed().as_secs_f64() * 1000.0;

                let rows_start = Instant::now();
                let row_bytes = (desc.Width * 4) as usize;
                for row in 0..desc.Height as usize {
                    let source = (mapped.pData as *const u8).add(row * mapped.RowPitch as usize);
                    let target = dib.pixels_mut().as_mut_ptr().add(row * row_bytes);
                    std::ptr::copy_nonoverlapping(source, target, row_bytes);
                }
                let rows = rows_start.elapsed().as_secs_f64() * 1000.0;
                context.Unmap(staging, 0);
                let _ = duplication.ReleaseFrame();
                let _ = resource;

                println!(
                    "  attempt {attempt}: wait {waited:.2} / copy {copied:.2} / map {mapped_ms:.2} / rows {rows:.2} / total {:.2} ms  ({}x{})",
                    waited + copied + mapped_ms + rows,
                    desc.Width,
                    desc.Height
                );
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }

    /// Checks that the GPU path and the GDI path agree, pixel for pixel.
    ///
    /// `cargo test --release -- --nocapture duplication_matches_gdi`
    #[test]
    fn duplication_matches_gdi() {
        // The duplication path logs what it saw when the marks are on.
        unsafe { std::env::set_var("XSHOT_TIMING", "1") };
        // A still screen has no frames to report, so this is what the app does at
        // startup: build everything and take the first copy from GDI.
        crate::win::duplication::warm();
        let screen = crate::win::util::virtual_screen();
        let mut gpu = Dib::new(screen.w, screen.h).unwrap();
        if !crate::win::duplication::capture_into(&mut gpu) {
            println!("desktop duplication unavailable here; nothing to compare");
            return;
        }
        // Both together, so the desktop has as little chance to change between
        // them as possible.
        let gdi = Dib::new(screen.w, screen.h).unwrap();
        unsafe {
            let dc = GetDC(None);
            let _ = BitBlt(
                gdi.hdc,
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
        }

        let (a, b) = (gpu.pixels(), gdi.pixels());
        let mut differing = 0u64;
        let mut worst = 0u8;
        let mut samples = 0u64;
        // Every thirteenth pixel: enough to catch a shifted, swapped or blank
        // image, cheap enough to run in a test.
        for index in (0..a.len() / 4).step_by(13) {
            let base = index * 4;
            let delta = (0..3)
                .map(|channel| a[base + channel].abs_diff(b[base + channel]))
                .max()
                .unwrap_or(0);
            samples += 1;
            if delta > 8 {
                differing += 1;
            }
            worst = worst.max(delta);
        }
        println!(
            "duplication vs GDI over {samples} samples: {differing} differ by more than 8, worst {worst}"
        );
        assert!(
            differing * 100 < samples,
            "{differing} of {samples} sampled pixels differ between the GPU and GDI captures"
        );
    }
}
