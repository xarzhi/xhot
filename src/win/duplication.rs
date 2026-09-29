//! Reading the desktop off the GPU instead of copying it through GDI.
//!
//! `BitBlt` from the screen DC is a cross-process copy through the compositor's
//! redirection surface, and it is what a capture used to wait for: measured on a
//! 2560×1440 desktop, 26 ms — the single largest piece of the walk from the
//! hotkey to a frozen screen. Splitting the blit across threads does not help
//! (GDI serialises the readback) and dropping `CAPTUREBLT` saves 0.6 ms. DXGI
//! desktop duplication hands the same pixels over as a GPU texture, and getting
//! that texture to the CPU takes ~2 ms.
//!
//! The awkward part is that duplication reports *changes*, not pictures: on a
//! screen nobody has touched since, the frame it returns carries no content at
//! all (`AccumulatedFrames == 0`) and its texture is empty. That is exactly the
//! state a screenshot tool is invoked in, so this module keeps its own copy of
//! the whole desktop and refreshes it from whatever each monitor says has
//! changed — which is the complete answer either way, because a monitor that
//! changed nothing is still showing what the copy already holds.
//!
//! GDI stays behind all of it as the fallback. Duplication is unavailable in
//! some sessions (a remote desktop, a full-screen exclusive game, a secure
//! desktop) and dies with `DXGI_ERROR_ACCESS_LOST` when the display mode
//! changes, and a screenshot tool cannot afford to have no answer there.

use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_FLAG, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter, IDXGIDevice, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface as _;

use crate::win::frame::Dib;

/// How long to wait for a frame from a monitor.
///
/// A wait that expires is answered by the copy that is already held, so this is
/// short on purpose: a capture must never block on a still screen.
const FRAME_WAIT_MS: u32 = 15;

/// One monitor's duplication, and where that monitor sits on the desktop.
struct Monitor {
    duplication: IDXGIOutputDuplication,
    /// Top-left corner in virtual-desktop coordinates, which is what the DIB is
    /// indexed by.
    x: i32,
    y: i32,
    /// The staging texture a frame is copied into, kept between captures.
    staging: Option<ID3D11Texture2D>,
}

impl Monitor {
    /// Pulls this monitor's changes into `desktop`, if it has any.
    ///
    /// Returns whether the frame carried content. A frame without any is a
    /// pointer-shape update: the texture that comes with it is empty and must
    /// not be copied, and `desktop` already holds what is on screen.
    fn refresh(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        desktop: &mut Dib,
    ) -> Result<bool> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        match unsafe { self.duplication.AcquireNextFrame(FRAME_WAIT_MS, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(error) => bail!("AcquireNextFrame: {error}"),
        }
        // Even a frame with no content has to be released, or no later capture
        // can ever acquire one again.
        let content = info.AccumulatedFrames > 0 && info.LastPresentTime != 0;
        let copied = match (content, resource) {
            (true, Some(resource)) => self.copy_frame(device, context, &resource, desktop),
            _ => Ok(()),
        };
        let _ = unsafe { self.duplication.ReleaseFrame() };
        copied?;
        Ok(content)
    }

    fn copy_frame(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        frame: &IDXGIResource,
        desktop: &mut Dib,
    ) -> Result<()> {
        let texture: ID3D11Texture2D = frame.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };

        // A new monitor or a resolution change: the staging texture has to match
        // whatever the duplication is handing out now.
        let stale = match self.staging.as_ref() {
            None => true,
            Some(staging) => {
                let mut current = D3D11_TEXTURE2D_DESC::default();
                unsafe { staging.GetDesc(&mut current) };
                current.Width != desc.Width
                    || current.Height != desc.Height
                    || current.Format != desc.Format
            }
        };
        if stale {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Width: desc.Width,
                Height: desc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: desc.Format,
                SampleDesc: desc.SampleDesc,
                // Staging is the only usage that can be mapped for reading.
                Usage: D3D11_USAGE_STAGING,
                BindFlags: D3D11_BIND_FLAG(0).0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut created: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut created))? };
            self.staging = created;
        }
        let staging = self.staging.as_ref().context("staging texture")?;

        unsafe {
            context.CopyResource(staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let copied = self.copy_rows(&mapped, desc.Width, desc.Height, desktop);
            context.Unmap(staging, 0);
            copied?;
        }
        Ok(())
    }

    /// Moves the mapped rows into the desktop copy, clipped to it.
    ///
    /// # Safety
    /// `mapped.pData` must be the base of a readable buffer of `height` rows of
    /// `mapped.RowPitch` bytes, which is what `Map` guarantees.
    unsafe fn copy_rows(
        &self,
        mapped: &D3D11_MAPPED_SUBRESOURCE,
        width: u32,
        height: u32,
        desktop: &mut Dib,
    ) -> Result<()> {
        let screen = crate::win::util::virtual_screen();
        // Where this monitor lands in the desktop copy, and the part of it that
        // fits. A monitor partly off the virtual screen is clipped, not refused.
        let left = (self.x - screen.x).max(0);
        let top = (self.y - screen.y).max(0);
        let right = (self.x - screen.x + width as i32).min(desktop.width);
        let bottom = (self.y - screen.y + height as i32).min(desktop.height);
        if right <= left || bottom <= top {
            bail!(
                "monitor at ({}, {}) is not part of the captured desktop",
                self.x,
                self.y
            );
        }
        let source_x = (left - (self.x - screen.x)) as usize * 4;
        let source_y = (top - (self.y - screen.y)) as usize;
        let columns = (right - left) as usize * 4;
        let row_pitch = mapped.RowPitch as usize;
        let stride = desktop.width as usize * 4;
        let pixels = desktop.pixels_mut();
        unsafe {
            let base = mapped.pData as *const u8;
            for row in 0..(bottom - top) as usize {
                let source = base.add((source_y + row) * row_pitch + source_x);
                let target = pixels
                    .as_mut_ptr()
                    .add((top as usize + row) * stride + left as usize * 4);
                std::ptr::copy_nonoverlapping(source, target, columns);
            }
        }
        Ok(())
    }
}

/// The device, one duplication per monitor, and our copy of the desktop.
///
/// Building it costs a Direct3D device and a duplication for every monitor — far
/// more than a capture does — so it is built once and reused. Dropping it (on any
/// failure) is the recovery path: the next capture builds a new one.
struct Duplicator {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    monitors: Vec<Monitor>,
    /// The whole desktop as of the last thing we were told about.
    ///
    /// This is what makes a capture of a still screen possible: duplication only
    /// reports what changed, so the parts nobody touched have to come from here.
    desktop: Option<Dib>,
    /// Whether that copy holds a real picture yet.
    ///
    /// A duplication that has never handed out a frame leaves `desktop` as a
    /// freshly allocated, all-zero bitmap, and answering a still screen with it
    /// captures a *black* screen. Until the copy is primed — by a frame, or by
    /// the GDI capture the caller falls back to — captures have to take the slow
    /// path.
    primed: bool,
}

impl Duplicator {
    fn new() -> Option<Self> {
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
            .ok()?;
            let device = device?;
            let context = context?;

            // Captures run on whichever background thread the executor picks, so
            // the immediate context has to be usable from any of them.
            if let Ok(multithread) = context.cast::<ID3D11Multithread>() {
                let _ = multithread.SetMultithreadProtected(true);
            }

            let dxgi_device: IDXGIDevice = device.cast().ok()?;
            let adapter: IDXGIAdapter = dxgi_device.GetAdapter().ok()?;
            let mut monitors = Vec::new();
            let mut index = 0;
            loop {
                let Ok(output) = adapter.EnumOutputs(index) else {
                    break;
                };
                index += 1;
                let Ok(desc) = output.GetDesc() else {
                    continue;
                };
                let Ok(output1) = output.cast::<IDXGIOutput1>() else {
                    continue;
                };
                let Ok(duplication) = output1.DuplicateOutput(&device) else {
                    continue;
                };
                let rect = desc.DesktopCoordinates;
                monitors.push(Monitor {
                    duplication,
                    x: rect.left,
                    y: rect.top,
                    staging: None,
                });
            }
            if monitors.is_empty() {
                return None;
            }
            // Only usable if the monitors together cover the desktop: a capture
            // missing one of them would silently show a hole where it is.
            let screen = crate::win::util::virtual_screen();
            let covered: i64 = monitors
                .iter()
                .map(|monitor| {
                    let width = (monitor.x + 1).max(screen.x + screen.w) - monitor.x.max(screen.x);
                    let height = (monitor.y + 1).max(screen.y + screen.h) - monitor.y.max(screen.y);
                    width.max(0) as i64 * height.max(0) as i64
                })
                .sum();
            if covered < screen.w as i64 * screen.h as i64 {
                return None;
            }
            Some(Self {
                device,
                context,
                monitors,
                desktop: None,
                primed: false,
            })
        }
    }

    /// Takes what each monitor has changed, then hands the whole desktop over.
    ///
    /// Returns whether the copy now holds a real picture. `false` means a fresh
    /// duplicator that nobody has drawn on yet: the caller has to fill the frame
    /// itself, and [`Self::adopt`] turns that into our copy.
    fn fill(&mut self, dib: &mut Dib) -> Result<bool> {
        if self.desktop.is_none() {
            self.desktop = Some(Dib::new(dib.width, dib.height).context("desktop copy")?);
        }
        // Taken out of `self` so the monitors can be borrowed at the same time;
        // put back before returning, whatever happens.
        let mut desktop = self.desktop.take().expect("just ensured");
        let result = (|| {
            let mut any = false;
            for monitor in &mut self.monitors {
                any |= monitor.refresh(&self.device, &self.context, &mut desktop)?;
            }
            self.primed |= any;
            if self.primed {
                dib.copy_from(&desktop)?;
            }
            Ok(self.primed)
        })();
        self.desktop = Some(desktop);
        result
    }

    /// Fills the desktop copy from GDI, for when duplication has nothing to say.
    ///
    /// Called once at startup: a screen nobody has touched since the duplication
    /// was created has no frames in it, and the copy has to come from somewhere.
    #[allow(dead_code)]
    fn prime(&mut self) -> Result<()> {
        let screen = crate::win::util::virtual_screen();
        let mut desktop = Dib::new(screen.w, screen.h).context("desktop copy")?;
        crate::win::frame::bitblt_into(&mut desktop, screen.x, screen.y)?;
        self.desktop = Some(desktop);
        self.primed = true;
        Ok(())
    }
}

/// Cached across captures: building it is the expensive part, not using it.
static DUPLICATOR: Mutex<Option<Duplicator>> = Mutex::new(None);

fn lock() -> std::sync::MutexGuard<'static, Option<Duplicator>> {
    DUPLICATOR
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Builds everything and takes a first copy of the desktop, off the hot path.
///
/// Called once at startup. Creating the device and the duplications takes tens of
/// milliseconds, and the first copy has to come from GDI — duplication starts
/// with no frames at all — and neither belongs in the walk from the hotkey to a
/// frozen screen, which is the one thing a user can feel.
#[allow(dead_code)]
/// Kept for the measurement test in rame.rs: the app itself builds the
/// duplicator lazily inside [capture_into], on the first capture after an idle
/// release, and primes it by adopting that capture's GDI fallback.
pub fn warm() {
    let mut cached = lock();
    if cached.is_none() {
        *cached = Duplicator::new();
    }
    if let Some(duplicator) = cached.as_mut()
        && let Err(error) = duplicator.prime()
    {
        crate::trace_log!("xshot: could not prime the desktop copy: {error:#}");
    }
}

/// Gives the cached device and duplications back.
///
/// They are exactly what makes a capture fast — building them costs tens of
/// milliseconds — but they also hold a Direct3D device plus one duplication per
/// monitor, and they live for as long as the process does. An app that sits in the
/// tray for hours between shots should not keep paying for that, so it is dropped
/// once the app has been idle and rebuilt on the next capture.
pub fn release() {
    let mut cached = lock();
    if cached.is_some() {
        *cached = None;
        crate::trace_log!("xshot: released the desktop duplicator while idle");
    }
}

/// Adopts a full-desktop image that GDI just captured.
///
/// The fallback is the only thing that can answer on a still screen before the
/// duplicator has a copy, so what it captured becomes the copy.
pub fn adopt(dib: &Dib) {
    let mut cached = lock();
    let Some(duplicator) = cached.as_mut() else {
        return;
    };
    let same_size = duplicator
        .desktop
        .as_ref()
        .is_some_and(|desktop| desktop.width == dib.width && desktop.height == dib.height);
    if same_size {
        if let Some(desktop) = duplicator.desktop.as_mut() {
            let _ = desktop.copy_from(dib);
        }
    } else if let Ok(copy) = dib.crop(0, 0, dib.width, dib.height) {
        duplicator.desktop = Some(copy);
    }
    // Whatever it took, the copy now holds a picture GDI took: a capture of a
    // still screen can be answered from it.
    duplicator.primed = true;
}

/// Fills `dib` with the desktop. `false` means "ask GDI instead".
///
/// Deliberately total: any problem — no hardware device, no duplication, a lost
/// one, a screen that has not changed since before we had a copy — answers
/// `false` rather than an error, because every caller has a working slow path
/// and none of them can do anything useful with the reason.
pub fn capture_into(dib: &mut Dib) -> bool {
    let mut cached = lock();
    if cached.is_none() {
        *cached = Duplicator::new();
    }
    let Some(duplicator) = cached.as_mut() else {
        return false;
    };
    match duplicator.fill(dib) {
        // `false` here is "the copy is still blank" — a duplicator built moments
        // ago that nobody has drawn on yet. The caller's GDI path fills the frame
        // and hands it back through `adopt`, which is what primes this one.
        Ok(primed) => primed,
        Err(error) => {
            // Whatever went wrong is unlikely to be fixed by trying again with
            // the same objects; the next capture builds fresh ones.
            crate::trace_log!("xshot: desktop duplication gave up: {error:#}");
            *cached = None;
            false
        }
    }
}
