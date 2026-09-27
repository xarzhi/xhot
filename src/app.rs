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
pub const SAVE_TITLE: &str = "xshot 保存";
/// A session's own toolbar (recording), in a window of its own.
pub const RECORD_BAR_TITLE: &str = "xshot-record-bar";
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
    /// The capture overlay. Built once (see [`prepare_overlay`]) and kept hidden
    /// between captures, so a capture never pays for the window again.
    pub overlay: Option<WindowHandle<Overlay>>,
    /// Whether that window is on screen for a capture right now.
    pub capture_active: bool,
    /// The "where should this go?" window, while one is open.
    pub save_dialog: Option<AnyWindowHandle>,
    /// A session's toolbar, while a recording is being set up or run.
    pub record_bar: Option<AnyWindowHandle>,
    pub pins: Vec<AnyWindowHandle>,
    pub next_pin_id: u32,
    /// The most recent pin, for the restore hotkey.
    pub last_pin: Option<PinSpec>,
    /// Whether the tray thread managed to claim each global chord. `None`
    /// until it has reported in.
    pub hotkey_status: Option<(bool, bool)>,
    /// The window the user was in when we took the foreground, as a raw HWND.
    ///
    /// A capture starts by stealing the foreground, and destroying the window
    /// that owns it leaves *no* foreground window at all. Our next capture could
    /// then not take it back, and the overlay would receive no keystroke —
    /// Enter, Esc and the tool shortcuts all dead, which reads as the program
    /// having stopped responding. So it is remembered, and handed back when the
    /// capture ends.
    pub previous_foreground: isize,
    /// The Ctrl+drag in flight, if there is one.
    ///
    /// The gesture starts on a window that is not ours — the press belongs to
    /// whatever is underneath, and that window keeps the mouse for the whole
    /// drag — so the overlay never sees a mouse event of its own. This is what it
    /// draws instead, and what it pins when the button comes up. It lives here
    /// rather than in the overlay because the drag can be over before the frozen
    /// desktop has even been grabbed.
    pub drag: Option<LiveDrag>,
}

/// A Ctrl+drag, in physical screen pixels.
#[derive(Clone, Copy, Debug)]
pub struct LiveDrag {
    pub start: (i32, i32),
    pub current: (i32, i32),
    /// The button has come up: the drag is over, whether or not the window that
    /// draws it is ready yet.
    pub released: bool,
}

impl LiveDrag {
    pub fn rect(&self) -> (i32, i32, i32, i32) {
        let (x0, y0) = self.start;
        let (x1, y1) = self.current;
        (
            x0.min(x1),
            y0.min(y1),
            (x1 - x0).abs(),
            (y1 - y0).abs(),
        )
    }

    /// Whether the drag is big enough to be a shot rather than a slip of the
    /// hand. Same threshold the gesture has always used.
    pub fn is_usable(&self) -> bool {
        let (_, _, w, h) = self.rect();
        w >= 8 && h >= 8
    }
}

impl Global for AppState {}

impl AppState {
    /// Takes the remembered foreground window, if there is one left to restore.
    fn take_previous_foreground(&mut self) -> Option<isize> {
        (self.previous_foreground != 0).then(|| std::mem::take(&mut self.previous_foreground))
    }

    pub fn new(config: Config) -> Self {
        Self {
            config,
            settings: None,
            overlay: None,
            capture_active: false,
            save_dialog: None,
            record_bar: None,
            pins: Vec::new(),
            next_pin_id: 1,
            last_pin: None,
            hotkey_status: None,
            previous_foreground: 0,
            drag: None,
        }
    }
}

/// Remembers who owns the foreground before a capture takes it.
pub fn note_foreground(cx: &mut App) {
    state_mut(cx).previous_foreground = util::foreground_window()
        .map(|hwnd| hwnd.0 as isize)
        .unwrap_or(0);
}

/// Hands the foreground back to whoever had it before the capture.
///
/// Called as a capture session closes, and it has to happen *before* the window
/// that currently owns the foreground is destroyed: once there is no foreground
/// window, Windows no longer lets us give it to anyone.
pub fn restore_foreground(cx: &mut App) {
    if let Some(previous) = state_mut(cx).take_previous_foreground() {
        util::restore_foreground(previous);
    }
}

/// Device pixels per logical pixel on the primary display.
pub fn scale_factor(cx: &App) -> f32 {
    estimate_scale(cx, crate::win::util::virtual_screen())
}

pub fn state(cx: &App) -> &AppState {
    cx.global::<AppState>()
}

pub fn state_mut(cx: &mut App) -> &mut AppState {    cx.global_mut::<AppState>()
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

    // Tall enough for the general page's four cards without scrolling.
    let bounds = Bounds::centered(None, size(px(860.0), px(680.0)), cx);
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
        window_min_size: Some(size(px(860.0), px(680.0))),
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

/// Takes one of our windows off the screen for good.
///
/// `Window::remove_window` only asks the toolkit to drop the window, and the
/// toolkit does that on a later frame — of which there may be none, because
/// these windows are usually the last ones the app has. Until that frame, the
/// Win32 window is still there: whatever it has already painted may be gone
/// while the window itself still takes every click that lands on it. The user
/// then sees clicks do nothing anywhere on screen, and the only way out is
/// another click landing on that window itself.
///
/// So Win32 is told first. The window is hidden and its region given back —
/// a region is what decides which clicks the window can take — and only then is
/// the toolkit asked to drop it.
///
/// The title is the only handle Win32-side; every close path here knows the
/// title of the window it is closing.
pub fn retire_window(title: &str, window: &mut Window) {
    util::set_window_region(title, &[]);
    util::hide_window(title);
    window.remove_window();
}

/// Closes the capture session: the save window (if any) and the overlay.
///
/// A save finishes the session, so both windows go together. The overlay window
/// itself is kept — it is hidden, not dropped, so the next capture does not have
/// to build it again.
pub fn close_capture(cx: &mut App) {
    crate::ui::save_dialog::close(cx);
    crate::ui::record_bar::close(cx);
    // The save window's button fires on the press, and the overlay may have gone
    // the same way earlier; either of them could still be holding the capture.
    util::release_capture();
    // Before the overlay goes: while we still own the foreground, give it back.
    restore_foreground(cx);
    state_mut(cx).drag = None;
    hide_overlay(cx);
}

/// Closes just the save window, leaving the capture open to keep editing.
pub fn close_save_dialog(cx: &mut App) {
    crate::ui::save_dialog::close(cx);
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
/// The press has to be answered immediately, and the two things standing in the
/// way are the grab and the window. The grab runs on a worker thread; the window
/// is *not built here at all* — there is one already, kept hidden between shots,
/// because building it costs about 18 ms on this machine (measured; see
/// `tools/probe_drag_latency.ps1`). What is left on the critical path is placing
/// it, showing it, and the frame after that.
pub fn trigger_capture(cx: &mut App) {
    // A scrolling shot or a recording has the screen to itself; a new overlay
    // dropped on top of one would end up in its own output.
    if crate::capture::is_active() || state(cx).capture_active {
        return;
    }
    state_mut(cx).capture_active = true;

    // Synchronous and verified: by the time this returns the settings window is
    // off screen, so no guessed delay is needed before the capture.
    win_util::hide_window_and_wait(SETTINGS_TITLE, Duration::from_millis(250));
    crate::timing("2 own windows hidden");

    // Remember who had the foreground before the overlay takes it, so it can be
    // handed back when the capture ends.
    note_foreground(cx);

    // Start the grab first: the window is up long before it lands.
    let started = std::time::Instant::now();
    let grabbed = cx.background_executor().spawn(async move {
        frame::capture_virtual_screen().map(|dib| {
            // The display image is built here too: it is a 14 MB copy, and the
            // main thread has a window to show.
            let image = dib.to_render_image();
            (dib, image)
        })
    });

    let screen = util::virtual_screen();
    let Some(handle) = ensure_overlay_window(cx, screen) else {
        eprintln!("xshot: could not prepare the overlay");
        state_mut(cx).capture_active = false;
        state_mut(cx).drag = None;
        return;
    };
    // This is the part the user feels: place it, arm it, show it. The window draws
    // the dimming and the selection straight away, over the live desktop, and
    // swaps in the frozen picture when the grab lands.
    let drag = state(cx).drag;
    if drag.is_some() {
        // A Ctrl+drag reports itself through the mouse hook, and the press belongs
        // to the window underneath — that window is running the drag. Taking the
        // foreground would interrupt it, and a Chromium window activated in the
        // middle of a drag takes the mouse and holds on to it.
        win_util::set_no_activate(OVERLAY_TITLE, true);
    }
    let _ = handle.update(cx, |overlay, window, cx| {
        overlay.begin_capture(drag.map(|drag| drag.rect()), cx);
        window.refresh();
    });
    reveal_overlay(cx, screen, drag.is_none());
    crate::timing("w overlay shown");

    cx.spawn(async move |cx| {
        let captured = grabbed.await;
        crate::trace_log!("xshot: captured the desktop in {:?}", started.elapsed());
        crate::timing("3 desktop captured");
        let _ = cx.update(|cx| match captured {
            Ok((dib, image)) => install_overlay(cx, handle, screen, dib, image),
            Err(error) => {
                eprintln!("xshot: capture failed: {error:#}");
                // Nothing may be left on screen.
                hide_overlay(cx);
                state_mut(cx).drag = None;
            }
        });
    })
    .detach();
}

/// Builds the overlay window once, hidden, and keeps it.
///
/// Building it is the single most expensive step of a capture on this machine
/// (~18 ms: the window, its Direct3D swapchain and its first frame), and it does
/// not depend on the screen at all. So it is built ahead of time — at startup,
/// in the background — and every capture after that only has to place and show
/// it.
pub fn prepare_overlay(cx: &mut App) {
    if state(cx).overlay.is_some() {
        return;
    }
    let screen = util::virtual_screen();
    let _ = ensure_overlay_window(cx, screen);
}

/// The overlay window, created if this is the first capture of the session.
fn ensure_overlay_window(cx: &mut App, screen: VirtualScreen) -> Option<WindowHandle<Overlay>> {
    if let Some(handle) = state(cx).overlay {
        return Some(handle);
    }
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
        // Kept hidden between captures, and shown by `reveal_overlay`. It holds no
        // pixels while it is hidden (`clear_source`), so what it would draw is
        // nothing at all until a capture arms it.
        show: false,
        is_resizable: false,
        is_movable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };

    // A one-pixel frame, for the window to hold before its first capture.
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

/// Puts the overlay on screen, sized for the virtual desktop.
///
/// Deliberately the cheap half of showing a window: place it (a no-op when it is
/// already there, which it is after the first capture) and `ShowWindow`. Taking
/// the foreground is a separate step, and one that a Ctrl+drag must not take —
/// the press belongs to the window underneath, which is in the middle of a drag.
fn reveal_overlay(_cx: &mut App, screen: VirtualScreen, take_foreground: bool) {
    util::place_client_exact(OVERLAY_TITLE, screen.x, screen.y, screen.w, screen.h);
    win_util::reveal_window(OVERLAY_TITLE);
    if take_foreground {
        if let Some(hwnd) = util::find_window(OVERLAY_TITLE) {
            win_util::force_foreground(hwnd);
        }
    }
    crate::timing("8 foreground");
}

/// Takes the overlay off the screen and forgets its pixels, keeping the window.
///
/// `hide_window_and_wait`, not `remove_window`: the window is reused, and hiding
/// has to be past the compositor before anything photographs the screen.
pub fn hide_overlay(cx: &mut App) {
    state_mut(cx).capture_active = false;
    if state(cx).overlay.is_none() {
        return;
    }
    win_util::hide_window_and_wait(OVERLAY_TITLE, Duration::from_millis(200));
    if let Some(handle) = state(cx).overlay {
        let _ = handle.update(cx, |overlay, _, _| overlay.clear_source());
    }
    // A capture that came up without activation has to be reset, or the *next*
    // one would not be able to take the foreground either.
    win_util::set_no_activate(OVERLAY_TITLE, false);
}

/// Hands the freshly captured desktop to the window built for it and reveals it.
fn install_overlay(
    cx: &mut App,
    handle: WindowHandle<Overlay>,
    screen: VirtualScreen,
    dib: Dib,
    image: Arc<RenderImage>,
) {
    let (x, y) = (screen.x, screen.y);
    let drag = state(cx).drag;

    // A Ctrl+drag can be over before the frozen desktop has landed — a flick of a
    // gesture, or a press that never moved. There is nothing to show in that
    // case, and no window to show it in: a usable rectangle is cropped straight
    // out of the capture that is already in hand, and a press that never moved is
    // dropped without pinning anything.
    if let Some(drag) = drag.filter(|drag| drag.released) {
        state_mut(cx).drag = None;
        hide_overlay(cx);
        crate::timing("d drag over before the window was ready");
        if drag.is_usable() {
            let (dx, dy, dw, dh) = drag.rect();
            let left = (dx - x).clamp(0, dib.width - 1);
            let top = (dy - y).clamp(0, dib.height - 1);
            let width = dw.clamp(1, dib.width - left);
            let height = dh.clamp(1, dib.height - top);
            match dib.crop(left, top, width, height) {
                Ok(output) => {
                    let scale = estimate_scale(cx, screen);
                    open_pin(
                        cx,
                        output,
                        ((x + left) as f32 / scale, (y + top) as f32 / scale),
                        scale,
                    );
                    crate::timing("d pinned");
                }
                Err(error) => eprintln!("xshot: Ctrl+drag crop failed: {error:#}"),
            }
        }
        return;
    }

    // The window is already up (it was shown on the press, drawing the dimming and
    // the selection): all that is missing is the pixels.
    let _ = handle.update(cx, |overlay, window, cx| {
        overlay.set_source(dib, (x as f32, y as f32), image);
        // The rectangle the Ctrl+drag has drawn so far, in physical screen pixels.
        if let Some(drag) = drag {
            overlay.set_live_rect(Some(drag.rect()), cx);
        }
        window.focus(&overlay.focus_handle(), cx);
        window.refresh();
    });
    crate::timing("7 painted");
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
        // Transparent rather than opaque: a finished shot can have rounded
        // corners cut into its alpha, and an opaque window shows its own clear
        // colour — black — in all four of them.
        window_background: WindowBackgroundAppearance::Transparent,
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

/// Ctrl + left button went down: put the framing rectangle on screen.
///
/// The press belongs to the window underneath — the hook only watches, so that
/// Ctrl + left button went down: bring the screenshot window up on the drag.
///
/// The press itself belongs to the window underneath — the hook only watches, so
/// that program runs its own drag, exactly as it would without us. What is added
/// is the overlay: the screen is frozen on the press, and the selection follows
/// the drag from the hook's reports. The window never takes the foreground, so
/// the drag below is not interrupted.
pub fn drag_begin(cx: &mut App, x: i32, y: i32) {
    if crate::capture::is_active() || state(cx).capture_active {
        crate::timing("d drag begin ignored");
        return;
    }
    crate::timing("d drag begin");
    state_mut(cx).drag = Some(LiveDrag {
        start: (x, y),
        current: (x, y),
        released: false,
    });
    // The whole overlay, not a bare frame: the press is the moment the screen is
    // frozen, so the picture the user frames is the picture they saw, and the
    // tools are there if they want them. It has to come up on the *press*.
    trigger_capture(cx);
}

/// The drag moved: redraw the selection in the overlay, if it is up yet.
pub fn drag_update(cx: &mut App, x: i32, y: i32) {
    let Some(mut drag) = state(cx).drag else {
        return;
    };
    drag.current = (x, y);
    state_mut(cx).drag = Some(drag);
    draw_drag(cx);
}

/// The button came up: pin what the drag drew — or, if it never moved, just
/// close the overlay and pin nothing.
pub fn drag_end(cx: &mut App, x: i32, y: i32) {
    let Some(mut drag) = state(cx).drag else {
        crate::timing("d drag end with no drag in flight");
        return;
    };
    drag.current = (x, y);
    drag.released = true;
    state_mut(cx).drag = Some(drag);
    crate::timing("d drag end");

    // No window yet means the grab is still in flight; `install_overlay` takes it
    // from there, out of the capture already in hand.
    let Some(handle) = state(cx).overlay else {
        return;
    };
    if !drag.is_usable() {
        // A press that never moved: the gesture was a stray Ctrl+click, so the
        // overlay goes away again without pinning anything.
        crate::timing("d drag too small, closing without pinning");
        let _ = handle.update(cx, |overlay, window, cx| overlay.cancel(window, cx));
        return;
    }
    // Still holding its placeholder frame: there are no pixels in it to crop yet.
    if !handle
        .update(cx, |overlay, _, _| overlay.is_ready())
        .unwrap_or(false)
    {
        return;
    }
    let _ = handle.update(cx, |overlay, window, cx| {
        overlay.set_live_rect(Some(drag.rect()), cx);
        overlay.finish_live_drag(window, cx);
    });
}

/// Esc, or the other button, while a Ctrl+drag is still held: drop it.
pub fn drag_cancel(cx: &mut App) {
    if state_mut(cx).drag.take().is_none() {
        return;
    }
    if let Some(handle) = state(cx).overlay {
        let _ = handle.update(cx, |overlay, window, cx| overlay.cancel(window, cx));
    }
}

/// Shows the drag's current rectangle in the overlay, when there is one.
fn draw_drag(cx: &mut App) {
    let Some(drag) = state(cx).drag else {
        return;
    };
    let Some(handle) = state(cx).overlay else {
        return;
    };
    let _ = handle.update(cx, |overlay, _, cx| {
        overlay.set_live_rect(Some(drag.rect()), cx);
    });
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
