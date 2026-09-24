//! Putting a finished shot on the Windows clipboard.
//!
//! GPUI can also write images to the clipboard, but it only publishes
//! registered formats such as `PNG`. Plenty of Windows software (Paint, Office,
//! most chat clients) still pastes only `CF_DIB`/`CF_DIBV5`, so we publish the
//! device-independent bitmap ourselves and add `PNG` for the rest.

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_DIBV5};
use windows::core::w;

use crate::win::frame::Dib;

/// Publishes `dib` as `CF_DIBV5`, `CF_DIB` and `PNG`.
pub fn copy_image(dib: &Dib) -> Result<()> {
    let dibv5 = build_dibv5(dib);
    let dib_legacy = build_dib(dib);
    let png = dib.to_png().ok();

    // Another process may hold the clipboard open for a moment; retry briefly
    // instead of failing the user's copy.
    let mut last_error = None;
    for _ in 0..10 {
        match try_copy(&dibv5, &dib_legacy, png.as_deref()) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("clipboard unavailable")))
}

fn try_copy(dibv5: &[u8], dib: &[u8], png: Option<&[u8]>) -> Result<()> {
    unsafe {
        OpenClipboard(None).context("OpenClipboard")?;
        let result = (|| -> Result<()> {
            EmptyClipboard().context("EmptyClipboard")?;
            set_bytes(CF_DIBV5.0 as u32, dibv5)?;
            set_bytes(CF_DIB.0 as u32, dib)?;
            if let Some(png) = png {
                let format = RegisterClipboardFormatW(w!("PNG"));
                if format != 0 {
                    set_bytes(format, png)?;
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

unsafe fn set_bytes(format: u32, bytes: &[u8]) -> Result<()> {
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).context("GlobalAlloc")?;
        let pointer = GlobalLock(handle);
        if pointer.is_null() {
            let _ = GlobalFree(Some(handle));
            bail!("GlobalLock returned null");
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
        let _ = GlobalUnlock(handle);

        match SetClipboardData(format, Some(HANDLE(handle.0))) {
            // On success the clipboard owns the block.
            Ok(_) => Ok(()),
            Err(error) => {
                let _ = GlobalFree(Some(handle));
                Err(error).context("SetClipboardData")
            }
        }
    }
}

/// `CF_DIBV5`: a 124-byte `BITMAPV5HEADER` followed by bottom-up `BGRA` rows.
fn build_dibv5(dib: &Dib) -> Vec<u8> {
    let (width, height) = (dib.width, dib.height);
    let image_size = (width as u32) * (height as u32) * 4;

    let mut out = Vec::with_capacity(124 + image_size as usize);
    push_u32(&mut out, 124); // bV5Size
    push_i32(&mut out, width); // bV5Width
    push_i32(&mut out, height); // bV5Height (positive => bottom-up)
    push_u16(&mut out, 1); // bV5Planes
    push_u16(&mut out, 32); // bV5BitCount
    push_u32(&mut out, 3); // bV5Compression = BI_BITFIELDS
    push_u32(&mut out, image_size); // bV5SizeImage
    push_i32(&mut out, 2835); // bV5XPelsPerMeter (~72 dpi)
    push_i32(&mut out, 2835); // bV5YPelsPerMeter
    push_u32(&mut out, 0); // bV5ClrUsed
    push_u32(&mut out, 0); // bV5ClrImportant
    push_u32(&mut out, 0x00FF_0000); // bV5RedMask
    push_u32(&mut out, 0x0000_FF00); // bV5GreenMask
    push_u32(&mut out, 0x0000_00FF); // bV5BlueMask
    push_u32(&mut out, 0xFF00_0000); // bV5AlphaMask
    push_u32(&mut out, 0x7352_4742); // bV5CSType = LCS_sRGB
    out.extend_from_slice(&[0u8; 36]); // bV5Endpoints
    for _ in 0..3 {
        push_u32(&mut out, 0); // bV5Gamma*
    }
    push_u32(&mut out, 0); // bV5Intent
    push_u32(&mut out, 0); // bV5ProfileData
    push_u32(&mut out, 0); // bV5ProfileSize
    push_u32(&mut out, 0); // bV5Reserved

    push_bottom_up(&mut out, dib);
    out
}

/// `CF_DIB`: a 40-byte `BITMAPINFOHEADER`, the three bit-field masks that
/// `BI_BITFIELDS` requires, then bottom-up `BGRA` rows.
fn build_dib(dib: &Dib) -> Vec<u8> {
    let (width, height) = (dib.width, dib.height);
    let image_size = (width as u32) * (height as u32) * 4;

    let mut out = Vec::with_capacity(52 + image_size as usize);
    push_u32(&mut out, 40); // biSize
    push_i32(&mut out, width); // biWidth
    push_i32(&mut out, height); // biHeight
    push_u16(&mut out, 1); // biPlanes
    push_u16(&mut out, 32); // biBitCount
    push_u32(&mut out, 3); // biCompression = BI_BITFIELDS
    push_u32(&mut out, image_size); // biSizeImage
    push_i32(&mut out, 2835); // biXPelsPerMeter
    push_i32(&mut out, 2835); // biYPelsPerMeter
    push_u32(&mut out, 0); // biClrUsed
    push_u32(&mut out, 0); // biClrImportant
    push_u32(&mut out, 0x00FF_0000);
    push_u32(&mut out, 0x0000_FF00);
    push_u32(&mut out, 0x0000_00FF);

    push_bottom_up(&mut out, dib);
    out
}

fn push_bottom_up(out: &mut Vec<u8>, dib: &Dib) {
    let stride = dib.width as usize * 4;
    let pixels = dib.pixels();
    for row in (0..dib.height as usize).rev() {
        out.extend_from_slice(&pixels[row * stride..(row + 1) * stride]);
    }
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
