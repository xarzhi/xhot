//! Shared application state and the two entry points every trigger funnels
//! through: open the settings window, or take a shot.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;

use crate::config::{CloseBehavior, Config};
use crate::ui::overlay::Overlay;
use crate::ui::pin::Pin;
use crate::ui::settings::Settings;
use crate::win::frame::{self, Dib};
use crate::win::util::{self, VirtualScreen};
use crate::win::{tray, util as win_util};

/// Window titles double as stable handles: `win::util::find_window` uses them
/// to recover the HWND of a window GPUI created for us.
pub const SETTINGS_TITLE: &str = "xshot 设置";
pub const OVERLAY_TITLE: &str = "xshot-overlay";
pub const PIN_TITLE_PREFIX: &str = "xshot-pin-";

/// Everything needed to put a pinned shot back on screen later.
///
/// Kept in the app state rather than in the pin window so that F2 can restore
/// the last pin after the user has closed it.
#[derive(Clone)]
pub struct PinSpec {
    pub pixels: Arc<Dib>,
    /// Where the shot sat on screen, in logical pixels.
    pub origin: (f32, f32),
    /// Display zoom chosen with the wheel.
    pub zoom: f32,
}

/// Process-wide state, stored as a GPUI global so any view can reach it.
pub struct AppState {
    pub config: Config,
    pub settings: Option<AnyWindowHandle>,
    pub overlay: Option<WindowHandle<Overlay>>,
    pub pins: Vec<AnyWindowHandle>,
    pub next_pin_id: u32,
    /// The most recent pin, for the restore hotkey.
    pub last_pin: Option<PinSpec>,
    /// Whether the tray thread managed to claim each global chord. `None`
    /// until it has reported in.
    pub hotkey_status: Option<(bool, bool)>,
}

impl Global for AppState {}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            settings: None,
            overlay: None,
            pins: Vec::new(),
            next_pin_id: 1,
            last_pin: None,
            hotkey_status: None,
        }
    }
}

pub fn state(cx: &App) -> &AppState {
    cx.global::<AppState>()
}

pub fn state_mut(cx: &mut App) -> &mut AppState {
    cx.global_mut::<AppState>()
}

/// Records a configuration change and writes it straight to disk, so the
/// setting survives even if the process is killed rather than closed.
pub fn update_config(cx: &mut App, change: impl FnOnce(&mut Config)) {
    let snapshot = {
        let state = state_mut(cx);
        change(&mut state.config);
        state.config.clone()
    };
    snapshot.store();
    // Hotkeys and the Ctrl+drag gesture live on the tray thread, so a change
    // has to be pushed across rather than picked up lazily.
    sync_global_input(cx);
}

/// Shows the settings window, re-focusing it when it already exists.
pub fn open_settings(cx: &mut App) {
    if let Some(handle) = state(cx).settings {
        if handle.update(cx, |_, window, _| window.activate_window()).is_ok() {
            // The window still exists; it may just be hidden in the tray.
            win_util::show_window(SETTINGS_TITLE);
            return;
        }
        // The window is gone; drop the stale handle and build a new one.
        state_mut(cx).settings = None;
    }

    let bounds = Bounds::centered(None, size(px(860.0), px(560.0)), cx);
    let theme = state(cx).config.resolved_theme();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(SETTINGS_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        is_resizable: false,
        is_minimizable: true,
        window_min_size: Some(size(px(860.0), px(560.0))),
        // The Mica material is the Fluent window backdrop on Windows 11.
        window_background: theme.backdrop(),
        ..Default::default()
    };

    match cx.open_window(options, |window, cx| {
        let view = cx.new(|cx| Settings::new(window, cx));
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    }) {
        Ok(handle) => state_mut(cx).settings = Some(handle.into()),
        Err(error) => eprintln!("xshot: could not open settings: {error:#}"),
    }
}

/// Handles the settings window's close button according to the configured
/// behaviour.
///
/// "Minimise to tray" hides the window at the Win32 level rather than asking
/// GPUI to close it. The window is the app's main surface and is cheap to keep
/// around, and hiding it is instantaneous — a closed window would have to be
/// rebuilt, and GPUI only drains a close request on a later frame.
pub fn close_settings(cx: &mut App) {
    let behavior = state(cx).config.close_behavior;
    crate::trace_log!("xshot: settings closed, behaviour = {behavior:?}");
    match behavior {
        CloseBehavior::Exit => quit(cx),
        CloseBehavior::MinimizeToTray => {
            win_util::hide_window(SETTINGS_TITLE);
        }
    }
}

/// Handles a `截图` request from the hotkey or the tray menu.
///
/// Two things are worth more than anything else here, because they are the only
/// ones the user can feel: the pixels have to be grabbed, and a full-screen
/// window with a Direct3D swapchain has to be built. They are independent, so
/// they run at the same time — the grab starts on a worker thread and the window
/// is built (hidden) while it is in flight. Measured on this machine, that takes
/// the hotkey-to-visible walk from ~50 ms to ~33 ms.
pub fn trigger_capture(cx: &mut App) {
    if state(cx).overlay.is_some() {
        let handle = state(cx).overlay.unwrap();
        if handle.update(cx, |_, _, _| {}).is_ok() {
            // A selection is already in progress; a second F1 must not stack
            // another overlay on top of it.
            return;
        }
        state_mut(cx).overlay = None;
    }

    // Synchronous and verified: by the time this returns the settings window is
    // off screen, so no guessed delay is needed before the capture.
    win_util::hide_window_and_wait(SETTINGS_TITLE, Duration::from_millis(250));
    crate::timing("2 own windows hidden");

    // Start the grab first, then spend the window build on top of it.
    let started = std::time::Instant::now();
    let grabbed = cx.background_executor().spawn(async move {
        frame::capture_virtual_screen().map(|dib| {
            // The display image is built here too: it is a 14 MB copy, and the
            // main thread has a window to create.
            let image = dib.to_render_image();
            (dib, image)
        })
    });

    let screen = util::virtual_screen();
    let Some(handle) = open_overlay_window(cx, screen) else {
        eprintln!("xshot: could not open overlay");
        return;
    };
    crate::timing("w overlay window built");

    cx.spawn(async move |cx| {
        let captured = grabbed.await;
        crate::trace_log!("xshot: captured the desktop in {:?}", started.elapsed());
        crate::timing("3 desktop captured");
        let _ = cx.update(|cx| match captured {
            Ok((dib, image)) => install_overlay(cx, handle, screen, dib, image),
            Err(error) => {
                eprintln!("xshot: capture failed: {error:#}");
                // The empty window must not be left behind.
                let _ = handle.update(cx, |_, window, _| window.remove_window());
                state_mut(cx).overlay = None;
            }
        });
    })
    .detach();
}

/// Builds the overlay window, hidden, over a placeholder frame.
///
/// It stays invisible until [`install_overlay`] has both the real pixels and a
/// drawn frame holding them, so the user never sees a half-built overlay.
fn open_overlay_window(cx: &mut App, screen: VirtualScreen) -> Option<WindowHandle<Overlay>> {
    let scale = estimate_scale(cx, screen);
    let logical_width = screen.w as f32 / scale;
    let logical_height = screen.h as f32 / scale;
    let bounds = Bounds {
        origin: point(px(screen.x as f32 / scale), px(screen.y as f32 / scale)),
        size: size(px(logical_width), px(logical_height)),
    };

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(TitlebarOptions {
            title: Some(OVERLAY_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        // `PopUp` gives us a tool window that is topmost and absent from the
        // taskbar and Alt+Tab, which is exactly what a selection overlay is.
        kind: WindowKind::PopUp,
        focus: false,
        // Shown from the start, but transparent and drawing nothing until the
        // capture lands: the user sees the desktop through it, so the window
        // being built early costs nothing visually. A *hidden* window would be
        // no good here — the platform frame loop never delivers it a frame, so
        // it could not be filled in before being revealed.
        show: true,
        is_resizable: false,
        is_movable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };

    // A one-pixel frame. GDI hands back a zeroed section, so it is black, and
    // the window is hidden anyway.
    let placeholder = Dib::new(1, 1).ok()?;
    let origin = (screen.x, screen.y);
    let handle = cx
        .open_window(options, move |window, cx| {
            cx.new(|cx| Overlay::new(placeholder, origin, window, cx))
        })
        .ok()?;
    state_mut(cx).overlay = Some(handle);
    Some(handle)
}

/// Hands the freshly captured desktop to the window built for it and reveals it.
fn install_overlay(
    cx: &mut App,
    handle: WindowHandle<Overlay>,
    screen: VirtualScreen,
    dib: Dib,
    image: Arc<RenderImage>,
) {
    let (x, y, w, h) = (screen.x, screen.y, screen.w, screen.h);
    // Fix the geometry before the frame is drawn, so what is presented matches
    // the client area exactly.
    if util::place_client_exact(OVERLAY_TITLE, x, y, w, h).is_none() {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
        state_mut(cx).overlay = None;
        return;
    }
    crate::timing("6 placed");

    let _ = handle.update(cx, |overlay, window, cx| {
        overlay.set_source(dib, (x as f32, y as f32), image);
        window.focus(&overlay.focus_handle(), cx);
        // Hand the frozen desktop to the compositor before taking the
        // foreground, so the overlay is never on screen without it.
        window.refresh();
    });
    crate::timing("7 painted");

    if let Some(hwnd) = util::find_window(OVERLAY_TITLE) {
        win_util::force_foreground(hwnd);
    }
    crate::timing("8 foreground");
}

/// Opens a borderless, always-on-top window holding a finished shot.
///
/// `scale` is the display's DPI scale, so the window can be sized in logical
/// pixels to exactly cover the image's physical pixels. Deriving it from the
/// image instead would be circular — a small crop would look like a huge zoom
/// factor and the pin would come out far too large.
pub fn open_pin(cx: &mut App, pixels: Dib, origin: (f32, f32), scale: f32) {
    let spec = PinSpec {
        pixels: Arc::new(pixels),
        origin,
        zoom: 1.0,
    };
    state_mut(cx).last_pin = Some(spec.clone());
    show_pin(cx, spec, scale);
}

/// Puts a pin on screen. Shared by the pin action and by F2's restore.
pub fn show_pin(cx: &mut App, spec: PinSpec, scale: f32) {
    let scale = if scale > 0.0 { scale } else { 1.0 };
    let id = {
        let state = state_mut(cx);
        let id = state.next_pin_id;
        state.next_pin_id = state.next_pin_id.wrapping_add(1).max(1);
        id
    };
    let title = format!("{PIN_TITLE_PREFIX}{id}");

    let device_width = spec.pixels.width as f32 * spec.zoom;
    let device_height = spec.pixels.height as f32 * spec.zoom;
    let logical_width = device_width / scale;
    let logical_height = device_height / scale;

    // Keep the restored pin on screen even if the layout changed since.
    let screen = util::virtual_screen();
    let screen_w = screen.w as f32 / scale;
    let screen_h = screen.h as f32 / scale;
    let origin_x = spec.origin.0.clamp(0.0, (screen_w - logical_width).max(0.0));
    let origin_y = spec.origin.1.clamp(0.0, (screen_h - logical_height).max(0.0));

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(origin_x), px(origin_y)),
            size: size(px(logical_width), px(logical_height)),
        })),
        titlebar: Some(TitlebarOptions {
            title: Some(title.clone().into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_resizable: false,
        is_movable: true,
        window_background: WindowBackgroundAppearance::Opaque,
        ..Default::default()
    };

    let shadow = state(cx).config.pin_shadow;
    let theme = state(cx).config.resolved_theme();
    let pixels = spec.pixels.clone();
    let zoom = spec.zoom;
    let view_title = title.clone();
    match cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| Pin::new(pixels, zoom, view_title, shadow, theme, window, cx));
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    }) {
        Ok(handle) => {
            let handle: AnyWindowHandle = handle.into();
            state_mut(cx).pins.push(handle);
        }
        Err(error) => eprintln!("xshot: could not open pin: {error:#}"),
    }

    // Match the client area to the image so the pin shows 1:1 pixels.
    util::place_client_exact(
        &title,
        (origin_x * scale).round() as i32,
        (origin_y * scale).round() as i32,
        device_width.round() as i32,
        device_height.round() as i32,
    );
    win_util::set_window_shadow(&title, shadow);
}

/// Pushes the configured hotkeys and gestures to the tray thread.
pub fn sync_global_input(cx: &App) {
    let config = &state(cx).config;
    tray::set_hotkeys(
        crate::win::hotkey::parse(&config.hotkey_capture),
        crate::win::hotkey::parse(&config.hotkey_restore_pin),
    );
    tray::set_ctrl_drag(config.ctrl_drag_pin);
}

/// Ctrl + left-drag: capture exactly what was dragged and pin it.
///
/// The rectangle comes from the mouse hook in physical screen pixels, so it is
/// cropped straight out of a fresh capture — no overlay, no second gesture.
pub fn capture_region_and_pin(cx: &mut App, x: i32, y: i32, w: i32, h: i32) {
    // Our own window must not end up inside the shot.
    win_util::hide_window_and_wait(SETTINGS_TITLE, Duration::from_millis(200));

    cx.spawn(async move |cx| {
        let captured = cx
            .background_executor()
            .spawn(async move {
                let screen = crate::win::util::virtual_screen();
                let frame = frame::capture_virtual_screen()?;
                let left = (x - screen.x).clamp(0, frame.width - 1);
                let top = (y - screen.y).clamp(0, frame.height - 1);
                let width = w.clamp(1, frame.width - left);
                let height = h.clamp(1, frame.height - top);
                frame.crop(left, top, width, height)
            })
            .await;

        let _ = cx.update(|cx| match captured {
            Ok(dib) => {
                let scale = estimate_scale(cx, util::virtual_screen());
                let origin = (x as f32 / scale, y as f32 / scale);
                open_pin(cx, dib, origin, scale);
            }
            Err(error) => eprintln!("xshot: region capture failed: {error:#}"),
        });
    })
    .detach();
}

/// Brings back the shot that was pinned (and possibly closed) last.
pub fn restore_last_pin(cx: &mut App) {
    let Some(spec) = state(cx).last_pin.clone() else {
        crate::trace_log!("xshot: F2 with nothing to restore");
        return;
    };
    let scale = estimate_scale(cx, util::virtual_screen());
    show_pin(cx, spec, scale);
}

/// Best-effort scale factor for the virtual desktop, derived from the primary
/// display's logical bounds.
fn estimate_scale(cx: &App, screen: VirtualScreen) -> f32 {
    cx.primary_display()
        .map(|display| {
            let logical = display.bounds().size.width.as_f32();
            if logical > 1.0 && screen.w > 0 {
                (screen.w as f32 / logical).clamp(1.0, 4.0)
            } else {
                1.0
            }
        })
        .unwrap_or(1.0)
}

/// Shuts the process down, tearing the tray icon down first so no ghost icon
/// is left in the notification area.
pub fn quit(cx: &mut App) {
    tray::shutdown();
    cx.quit();
}
