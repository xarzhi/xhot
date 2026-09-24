//! Thin, focused Win32 helpers.
//!
//! Everything here is deliberately small: the GPUI layer owns the UI, and this
//! module only covers the few things a window toolkit cannot do for us —
//! process DPI awareness, the virtual-desktop geometry, forcing our overlay to
//! the foreground, and locating our own HWNDs.

use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::BOOL;

/// Pins this process to per-monitor-v2 DPI awareness.
///
/// Must run before the first window is created. Without it Windows lies about
/// the virtual desktop size and `BitBlt` returns a blurry, upscaled screen, so
/// screenshots taken on a scaled display would be wrong. Failure is fine: a
/// manifest may already have set the awareness.
pub fn set_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// The bounding box of every monitor, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct VirtualScreen {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl VirtualScreen {
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
}

pub fn virtual_screen() -> VirtualScreen {
    unsafe {
        VirtualScreen {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// UTF-16, NUL-terminated. The allocation must outlive the call that uses it.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Finds a top-level window by its exact title.
///
/// GPUI gives each window the title from `TitlebarOptions`, and we hand out
/// unique titles, so this is a reliable way back to our own HWNDs without
/// depending on platform internals.
pub fn find_window(title: &str) -> Option<HWND> {
    let title = wide(title);
    unsafe { FindWindowW(None, windows::core::PCWSTR(title.as_ptr())).ok() }
}

/// Brings `hwnd` to the foreground even though we are a background process.
///
/// Windows refuses `SetForegroundWindow` from a process that does not own the
/// current foreground window, which is exactly our situation when F1 fires
/// while the user is typing in another app. Attaching to the foreground
/// thread's input queue lifts that restriction for the duration of the call.
pub fn force_foreground(hwnd: HWND) {
    unsafe {
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.is_invalid() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let current_thread = GetCurrentThreadId();
        let attached = foreground_thread != 0 && foreground_thread != current_thread;
        if attached {
            let _ = AttachThreadInput(foreground_thread, current_thread, true);
        }

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);

        if attached {
            let _ = AttachThreadInput(foreground_thread, current_thread, false);
        }
    }
}

/// Shows the four-way "move" pointer directly through Win32.
///
/// GPUI's Windows backend maps only a fixed handful of cursor styles and sends
/// everything else to the plain arrow, so the move pointer a draggable object
/// should show cannot be requested through it. Setting the cursor ourselves
/// after the toolkit has had its turn wins, because the listener that calls
/// this runs later in the same mouse-move dispatch.
pub fn show_move_cursor() {
    use windows::Win32::UI::WindowsAndMessaging::{IDC_SIZEALL, LoadCursorW, SetCursor};
    unsafe {
        if let Ok(cursor) = LoadCursorW(None, IDC_SIZEALL) {
            let _ = SetCursor(Some(cursor));
        }
    }
}

/// Restores a maximized window to the size it had before.
///
/// GPUI's `zoom_window` is documented as a toggle but its Windows backend calls
/// `ShowWindow(SW_MAXIMIZE)` unconditionally, so the other half of the toggle
/// has to happen here.
pub fn restore_window(title: &str) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SW_RESTORE, ShowWindowAsync};
    match find_window(title) {
        Some(hwnd) => unsafe { ShowWindowAsync(hwnd, SW_RESTORE).ok().is_ok() },
        None => false,
    }
}

/// Asks the user for a folder, starting at `start`.
///
/// `SHBrowseForFolder` is the old shell picker, but it is one call plus one path
/// lookup, it needs no COM apartment setup, and it is still the dialog Windows
/// itself shows for "choose a folder" in plenty of places.
pub fn pick_folder(start: &std::path::Path) -> Option<std::path::PathBuf> {
    use windows::Win32::UI::Shell::{
        BIF_NEWDIALOGSTYLE, BIF_RETURNONLYFSDIRS, BROWSEINFOW, SHBrowseForFolderW,
        SHGetPathFromIDListW,
    };
    use windows::core::{PCWSTR, PWSTR};

    let title = wide("选择保存位置");
    let mut initial = wide(&start.display().to_string());
    let mut info = BROWSEINFOW {
        hwndOwner: unsafe { GetForegroundWindow() },
        pszDisplayName: PWSTR(initial.as_mut_ptr()),
        lpszTitle: PCWSTR(title.as_ptr()),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        ..Default::default()
    };
    unsafe {
        let id_list = SHBrowseForFolderW(&mut info);
        if id_list.is_null() {
            return None; // cancelled
        }
        let mut buffer = [0u16; 260];
        if !SHGetPathFromIDListW(id_list, &mut buffer).as_bool() {
            return None;
        }
        let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(buffer.len());
        Some(std::path::PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
    }
}

/// Whether a virtual key is physically held down right now.
///
/// GPUI reports nothing at all for a bare modifier press — a lone Shift simply
/// produces no key event — so a feature that hangs off "the user pressed Shift"
/// has to ask Windows directly. The low bit of `GetAsyncKeyState` (pressed since
/// the previous call) is ignored and the high bit (currently down) is used, so
/// this is a level reading that the caller edge-detects.
pub fn key_down(virtual_key: u8) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    unsafe { (GetAsyncKeyState(virtual_key as i32) as u16) & 0x8000 != 0 }
}

/// The top-level windows a capture could select, in z-order, with their real
/// frame bounds already resolved.
///
/// Walking the window list on every mouse move is what made the overlay crawl
/// whenever the pointer was over the desktop: with no window under the pointer
/// the walk never short-circuits, so *every* mouse sample enumerated every
/// top-level window on the machine, and every one that contained the point paid
/// for a `DwmGetWindowAttribute` round trip. Nothing on screen moves while a
/// capture is open, so the geometry is collected once and then hit-tested in
/// memory, which is a handful of integer comparisons.
#[derive(Default)]
pub struct HoverIndex {
    windows: Vec<(i32, i32, i32, i32)>,
}

impl HoverIndex {
    /// Collects the candidate windows. Costs one enumeration plus one DWM query
    /// per visible top-level window, so it is meant to be called rarely.
    pub fn build() -> Self {
        let mut collect = HoverCollect {
            pid: std::process::id(),
            windows: Vec::new(),
        };
        unsafe {
            let _ = EnumWindows(
                Some(collect_window_for_hover),
                LPARAM(&mut collect as *mut HoverCollect as isize),
            );
        }
        Self {
            windows: collect.windows,
        }
    }

    /// The bounds of the topmost candidate containing the point.
    pub fn hit(&self, x: i32, y: i32) -> Option<(i32, i32, i32, i32)> {
        self.windows
            .iter()
            .find(|(left, top, right, bottom)| {
                x >= *left && x < *right && y >= *top && y < *bottom
            })
            .map(|(left, top, right, bottom)| (*left, *top, right - left, bottom - top))
    }
}

/// State carried through the `EnumWindows` callback while collecting.
struct HoverCollect {
    pid: u32,
    windows: Vec<(i32, i32, i32, i32)>,
}

unsafe extern "system" fn collect_window_for_hover(hwnd: HWND, lparam: LPARAM) -> BOOL {
    use windows::Win32::Graphics::Dwm::{
        DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GA_ROOT, GWL_EXSTYLE, GetAncestor, GetWindowLongPtrW, IsIconic, IsWindowVisible,
        WS_EX_TOOLWINDOW,
    };

    unsafe {
        let state = &mut *(lparam.0 as *mut HoverCollect);

        // Only real top-level windows; never our own overlay or the pins.
        let root = GetAncestor(hwnd, GA_ROOT);
        if root.is_invalid() || root != hwnd {
            return BOOL(1);
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == state.pid {
            return BOOL(1);
        }
        // A debug build's console is hosted by conhost.exe, so the process id
        // test above does not catch it — and it sits right in front of
        // everything else. It is exactly `GetConsoleWindow` for our process.
        let console = windows::Win32::System::Console::GetConsoleWindow();
        if !console.is_invalid() && console == hwnd {
            return BOOL(1);
        }
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return BOOL(1);
        }
        // Tool windows are palettes and tray helpers, not things a user points
        // at and means to capture.
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
            return BOOL(1);
        }

        let mut coarse = RECT::default();
        let _ = GetWindowRect(hwnd, &mut coarse);
        if coarse.right - coarse.left < 16 || coarse.bottom - coarse.top < 16 {
            return BOOL(1);
        }

        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return BOOL(1);
        }

        // The DWM frame bounds exclude the drop shadow, which `GetWindowRect`
        // includes — capturing a shadow border is not what anyone wants.
        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut std::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            rect = coarse;
        }
        if rect.right - rect.left < 16 || rect.bottom - rect.top < 16 {
            return BOOL(1);
        }
        state.windows.push((rect.left, rect.top, rect.right, rect.bottom));
        BOOL(1)
    }
}
/// The window that currently has the foreground, if any.
pub fn foreground_window() -> Option<HWND> {
    let hwnd = unsafe { GetForegroundWindow() };
    (!hwnd.is_invalid()).then_some(hwnd)
}

/// Where the mouse pointer is, in physical screen pixels.
pub fn cursor_position() -> (i32, i32) {
    let mut point = POINT { x: 0, y: 0 };
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// Moves a window's client area to a physical position, without resizing it.
///
/// The toolkit's own "start a window drag" gesture only works where the window
/// manager runs the drag for us, which on Windows it does not for a borderless
/// tool window. Moving the window ourselves is both reliable and immediate.
pub fn move_window_to(title: &str, x: i32, y: i32) -> bool {
    let Some(hwnd) = find_window(title) else {
        return false;
    };
    unsafe {
        let mut window_rect = RECT::default();
        let mut client_rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut window_rect);
        let _ = GetClientRect(hwnd, &mut client_rect);
        let mut origin = POINT { x: 0, y: 0 };
        let _ = ClientToScreen(hwnd, &mut origin);
        let inset_left = origin.x - window_rect.left;
        let inset_top = origin.y - window_rect.top;

        let _ = SetWindowPos(
            hwnd,
            None,
            x - inset_left,
            y - inset_top,
            window_rect.right - window_rect.left,
            window_rect.bottom - window_rect.top,
            SWP_NOACTIVATE | SWP_NOZORDER,
        );
    }
    true
}

/// Whether Windows is currently set to a light app theme.
pub fn system_uses_light_theme() -> bool {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };
    use windows::core::w;

    unsafe {
        let mut key = Default::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            None,
            KEY_READ,
            &mut key,
        )
        .is_err()
        {
            return false;
        }
        let mut value: u32 = 0;
        let mut size = std::mem::size_of::<u32>() as u32;
        let mut kind = Default::default();
        let result = RegQueryValueExW(
            key,
            w!("AppsUseLightTheme"),
            None,
            Some(&mut kind),
            Some(&mut value as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(key);
        result.is_ok() && value != 0
    }
}

/// Pins a window's *client area* to an exact physical rectangle and raises it.
///
/// GPUI sizes windows by their outer rectangle, but every drawing surface and
/// every mouse coordinate it reports is relative to the client area. For a
/// borderless window those differ by the invisible frame the window manager
/// still reserves — measured at 16x8 physical pixels on the test machine —
/// which is enough to stretch a screenshot by half a percent and shift clicks
/// sideways. Asserting the client rectangle directly is the only way to get a
/// pixel-exact overlay.
pub fn place_client_exact(title: &str, x: i32, y: i32, w: i32, h: i32) -> Option<HWND> {
    let hwnd = find_window(title)?;
    unsafe {
        let mut window_rect = RECT::default();
        let mut client_rect = RECT::default();
        let _ = GetWindowRect(hwnd, &mut window_rect);
        let _ = GetClientRect(hwnd, &mut client_rect);
        let mut origin = POINT { x: 0, y: 0 };
        let _ = ClientToScreen(hwnd, &mut origin);

        let inset_left = origin.x - window_rect.left;
        let inset_top = origin.y - window_rect.top;
        let frame_width =
            (window_rect.right - window_rect.left) - (client_rect.right - client_rect.left);
        let frame_height =
            (window_rect.bottom - window_rect.top) - (client_rect.bottom - client_rect.top);

        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            x - inset_left,
            y - inset_top,
            w + frame_width,
            h + frame_height,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    }
    Some(hwnd)
}

/// Where a window's client area currently starts, in physical screen pixels.
pub fn client_origin(title: &str) -> Option<(i32, i32)> {
    let hwnd = find_window(title)?;
    let mut origin = POINT { x: 0, y: 0 };
    unsafe {
        let _ = ClientToScreen(hwnd, &mut origin);
    }
    Some((origin.x, origin.y))
}

/// Turns the desktop-compositor drop shadow on or off for a window.
///
/// A borderless window has no frame, and it is the frame that normally earns a
/// shadow, so one is requested through DWM instead. The call is best-effort:
/// on systems that do not support the attribute the window simply stays flat.
pub fn set_window_shadow(title: &str, enabled: bool) -> bool {
    use windows::Win32::Graphics::Dwm::{
        DWMNCRP_DISABLED, DWMNCRP_ENABLED, DWMWA_NCRENDERING_POLICY, DwmSetWindowAttribute,
    };
    let Some(hwnd) = find_window(title) else {
        return false;
    };
    let policy = if enabled {
        DWMNCRP_ENABLED
    } else {
        DWMNCRP_DISABLED
    };
    let value = policy.0 as u32;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &value as *const u32 as *const std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
    }
}

/// Whether a top-level window with this title is currently on screen.
pub fn is_window_visible(title: &str) -> bool {    find_window(title)
        .map(|hwnd| unsafe { IsWindowVisible(hwnd).as_bool() })
        .unwrap_or(false)
}

/// Hides a window by title, synchronously.
///
/// This deliberately bypasses the UI toolkit. A toolkit's "close this window"
/// is usually just a flag drained on the next frame, which is not soon enough
/// when the very next thing we do is photograph the screen — the settings
/// window would still be in the shot.
pub fn hide_window(title: &str) -> bool {
    let Some(hwnd) = find_window(title) else {
        return false;
    };
    unsafe { ShowWindow(hwnd, SW_HIDE).as_bool() }
}

/// Shows and raises a window by title, taking the foreground.
pub fn show_window(title: &str) -> bool {
    let Some(hwnd) = find_window(title) else {
        return false;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    force_foreground(hwnd);
    true
}

/// Hides a window and waits until Windows agrees it is off screen.
///
/// Returns whether anything actually had to be hidden, so callers can skip
/// compensating work in the common case where our UI was already out of the way.
pub fn hide_window_and_wait(title: &str, timeout: std::time::Duration) -> bool {
    if !is_window_visible(title) {
        return false;
    }
    hide_window(title);

    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !is_window_visible(title) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(4));
    }

    // `ShowWindow` is synchronous on the window's own thread, but the desktop
    // compositor still holds the previous frame for a moment. `DwmFlush` is the
    // documented way to wait for exactly that — it returns once the compositor
    // has finished the frame that drops our window — and it costs one vsync
    // instead of the flat 32 ms this used to sleep. That sleep was the whole
    // cost of the first capture after the settings window had been open.
    flush_composition();

    // A short margin on top: `DwmFlush` can return just before the frame is on
    // screen if the queue was already empty.
    std::thread::sleep(std::time::Duration::from_millis(8));
    true
}

/// Blocks until the desktop compositor has finished its next frame.
fn flush_composition() {
    use windows::Win32::Graphics::Dwm::DwmFlush;
    unsafe {
        let _ = DwmFlush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of the index is that building it is expensive and
    /// hitting it is nearly free, because the hit is what runs on every mouse
    /// sample. This measures both on the real window list.
    #[test]
    fn hitting_the_hover_index_is_much_cheaper_than_building_it() {
        let build_start = std::time::Instant::now();
        let index = HoverIndex::build();
        let build = build_start.elapsed();

        let (x, y) = cursor_position();
        let hit_start = std::time::Instant::now();
        for _ in 0..10_000 {
            let _ = index.hit(x, y);
        }
        let hits = hit_start.elapsed();

        println!(
            "hover index: {} windows, build {build:?}, 10k hits {hits:?}",
            index.windows.len()
        );
        // Loose on purpose — the point is the ratio, not the absolute speed of
        // whatever machine this runs on.
        assert!(
            hits.as_nanos() < build.as_nanos().max(1) * 100,
            "10k hits took {hits:?} against a {build:?} build"
        );
    }
}
