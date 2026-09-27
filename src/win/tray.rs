//! The notification-area icon and the global input hooks.
//!
//! All of it lives on a dedicated thread with its own Win32 message loop. That
//! keeps it independent of GPUI's event loop: the hotkeys, the tray menu and
//! the Ctrl+drag gesture keep working while the UI thread is busy, and nothing
//! here has to know how GPUI pumps messages. The thread talks to the UI only
//! through an async channel.
//!
//! The Ctrl+drag gesture uses a low-level mouse hook that *observes* rather
//! than swallows: it reports the rectangle the user dragged and lets the click
//! continue to whatever is underneath. Nothing the user does in another program
//! is stolen.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use async_channel::Sender;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey, VK_CONTROL, GetAsyncKeyState,
};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::win::hotkey::Hotkey;
use crate::win::icon;

/// Everything the tray thread can ask the UI thread to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Open the selection overlay.
    Capture,
    /// Ctrl + left button went down here (physical screen pixels). The overlay
    /// comes up straight away and follows the drag.
    DragBegin { x: i32, y: i32 },
    /// The drag moved to this point.
    DragUpdate { x: i32, y: i32 },
    /// The button came up here: pin what was dragged.
    DragEnd { x: i32, y: i32 },
    /// Esc while the button was still down: throw the drag away.
    DragCancel,
    /// Re-open the most recently closed pinned shot.
    RestorePin,
    /// Show (or re-focus) the settings window.
    ShowSettings,
    /// Terminate the process.
    Quit,
    /// Whether each configured chord could actually be claimed. Windows lets
    /// only one program own a chord, so this is how a conflict reaches the UI.
    HotkeyStatus { capture: bool, restore: bool },
    /// The session stop key was pressed (Esc while a session is running).
    StopCapture,
}

/// `WM_APP + 1`: the tray icon's callback message.
const WM_TRAY: u32 = WM_APP + 1;
/// `WM_APP + 2`: re-read the hotkey configuration.
const WM_APPLY_HOTKEYS: u32 = WM_APP + 2;
/// `WM_APP + 3`: claim or release the session stop key. `wparam` is the state.
const WM_APP_STOP_HOTKEY: u32 = WM_APP + 3;

const HOTKEY_CAPTURE_ID: i32 = 0xA501;
const HOTKEY_RESTORE_ID: i32 = 0xA502;
/// Claimed only while a capture session is running.
const HOTKEY_STOP_ID: i32 = 0xA503;
const TRAY_ICON_ID: u32 = 1;

const MENU_CAPTURE: usize = 1001;
const MENU_SETTINGS: usize = 1002;
const MENU_EXIT: usize = 1003;
const MENU_RESTORE: usize = 1004;
const MENU_STOP: usize = 1005;

static COMMANDS: OnceLock<Sender<Command>> = OnceLock::new();
static TRAY_HWND: AtomicIsize = AtomicIsize::new(0);
static TRAY_ICON: AtomicIsize = AtomicIsize::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);

/// `modifiers << 8 | virtual_key`, or 0 for "not bound".
static HOTKEY_CAPTURE: AtomicU32 = AtomicU32::new(0);
static HOTKEY_RESTORE: AtomicU32 = AtomicU32::new(0);
static CTRL_DRAG: AtomicBool = AtomicBool::new(false);
static DRAG_START: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// Where the drag was last reported from, so the UI is not flooded with one
/// message per pixel of movement.
static DRAG_SENT: Mutex<Option<(i32, i32)>> = Mutex::new(None);
/// Set when a click closed one of our windows, so the release that follows is
/// eaten instead of reaching the window underneath. See
/// [`swallow_next_button_up`].
static SWALLOW_BUTTON_UP: AtomicBool = AtomicBool::new(false);
/// Which button that press belongs to, so a release of any *other* button is
/// left alone.
static SWALLOW_BUTTON: AtomicU32 = AtomicU32::new(0);
/// When that claim was armed. Only used to report how long the press was held.
static SWALLOW_ARMED_AT: AtomicU64 = AtomicU64::new(0);

fn pack(hotkey: Option<Hotkey>) -> u32 {
    match hotkey {
        Some(hotkey) => ((hotkey.modifiers & 0x0F) << 8) | (hotkey.virtual_key & 0xFF),
        None => 0,
    }
}

fn unpack(packed: u32) -> Option<Hotkey> {
    (packed != 0).then(|| Hotkey {
        modifiers: (packed >> 8) & 0x0F,
        virtual_key: packed & 0xFF,
    })
}

/// Starts the tray thread. Returns immediately.
pub fn spawn(
    commands: Sender<Command>,
    capture: Option<Hotkey>,
    restore: Option<Hotkey>,
    ctrl_drag: bool,
) -> std::io::Result<()> {
    let _ = COMMANDS.set(commands);
    HOTKEY_CAPTURE.store(pack(capture), Ordering::SeqCst);
    HOTKEY_RESTORE.store(pack(restore), Ordering::SeqCst);
    CTRL_DRAG.store(ctrl_drag, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("xshot-tray".into())
        .spawn(|| unsafe { run() })
        .map(|_| ())
}

/// Applies a hotkey change made in the settings window.
pub fn set_hotkeys(capture: Option<Hotkey>, restore: Option<Hotkey>) {
    HOTKEY_CAPTURE.store(pack(capture), Ordering::SeqCst);
    HOTKEY_RESTORE.store(pack(restore), Ordering::SeqCst);
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_APPLY_HOTKEYS,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// Turns the Ctrl+drag capture gesture on or off.
pub fn set_ctrl_drag(enabled: bool) {
    CTRL_DRAG.store(enabled, Ordering::SeqCst);
}

/// Asks the tray thread to tear everything down and exit its message loop.
pub fn shutdown() {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
}

fn send(command: Command) {
    if let Some(sender) = COMMANDS.get() {
        let _ = sender.send_blocking(command);
    }
}

unsafe fn run() {
    unsafe {
        let Ok(hinstance) = GetModuleHandleW(None) else {
            return;
        };

        let class_name = w!("xshot_tray_window");
        let window_class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: hinstance.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&window_class);

        // A plain hidden top-level window (rather than a message-only one) so
        // it still receives the shell's `TaskbarCreated` broadcast and can
        // restore the icon after Explorer restarts.
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("xshot-tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinstance.into()),
            None,
        ) else {
            return;
        };

        TRAY_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::SeqCst);

        add_icon(hwnd);
        apply_hotkeys(hwnd);
        install_mouse_hook(hinstance.into());

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

unsafe fn build_notify_data(hwnd: HWND, icon_handle: HICON) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        hIcon: icon_handle,
        ..Default::default()
    };
    let tip: Vec<u16> = "xshot 截图工具".encode_utf16().collect();
    let room = data.szTip.len() - 1;
    data.szTip[..tip.len().min(room)].copy_from_slice(&tip[..tip.len().min(room)]);
    data
}

unsafe fn add_icon(hwnd: HWND) {
    unsafe {
        let Some(hicon) = icon::create() else {
            return;
        };
        TRAY_ICON.store(hicon.0 as isize, Ordering::SeqCst);

        let mut data = build_notify_data(hwnd, hicon);
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &data);
    }
}

unsafe fn remove_icon(hwnd: HWND) {
    unsafe {
        let hicon = HICON(TRAY_ICON.load(Ordering::SeqCst) as *mut _);
        let data = build_notify_data(hwnd, hicon);
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
        if !hicon.is_invalid() {
            let _ = DestroyIcon(hicon);
        }
        TRAY_ICON.store(0, Ordering::SeqCst);
    }
}

unsafe fn apply_hotkeys(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_CAPTURE_ID);
        let _ = UnregisterHotKey(Some(hwnd), HOTKEY_RESTORE_ID);

        let mut claimed = [false, false];
        for (slot, id, packed, label) in [
            (0usize, HOTKEY_CAPTURE_ID, HOTKEY_CAPTURE.load(Ordering::SeqCst), "截图"),
            (1, HOTKEY_RESTORE_ID, HOTKEY_RESTORE.load(Ordering::SeqCst), "恢复贴图"),
        ] {
            let Some(hotkey) = unpack(packed) else {
                continue;
            };
            match RegisterHotKey(
                Some(hwnd),
                id,
                hotkey.modifiers_for_registration(),
                hotkey.virtual_key,
            ) {
                Ok(()) => {
                    claimed[slot] = true;
                    crate::trace_log!("xshot: {label} hotkey {} 已注册", hotkey.display());
                }
                Err(error) => crate::trace_log!(
                    "xshot: 无法注册{label}热键 {}（{error}）——可能已被其他程序占用",
                    hotkey.display()
                ),
            }
        }
        send(Command::HotkeyStatus {
            capture: claimed[0],
            restore: claimed[1],
        });
    }
}

/// Eats the next mouse-button release.
///
/// Called when a click closes one of our windows: the release that follows would
/// otherwise land on the window underneath and be read as a click there — which
/// is how a right click meant to cancel a screenshot ends up opening the context
/// menu of whatever was behind it.
///
/// Only armed while a button is actually held down. The same windows also close
/// on `Esc` and on Enter, and those gestures leave no release behind — arming the
/// claim anyway means it eats the next click's release, in whatever program the
/// user clicks next. That program is then stuck with a button it thinks is still
/// pressed: left clicks do nothing until a right click resets it.
///
/// `caller` only ever reaches the log, and the log only records the rare events:
/// arming, eating, expiring. If a click ever goes missing, that file says which
/// gesture left the claim behind.
/// A claim lives until that button is released, however long the user holds it.
///
/// It cannot be mistaken for a later click: a press clears it, and it only ever
/// eats the release of the button it was armed for. A *time limit* here is what
/// let a deliberate right click through — press, hold, release — and a release
/// that reaches the window behind is read as a click there. The log records how
/// long the user held it when that happens, which is the only reason the age is
/// kept at all.
pub fn swallow_next_button_up(caller: &str) {
    let Some(button) = held_button() else {
        mouse_log(&format!("{caller}: no button held, claim not armed"));
        return;
    };
    SWALLOW_BUTTON.store(button, Ordering::SeqCst);
    SWALLOW_ARMED_AT.store(now_millis(), Ordering::SeqCst);
    SWALLOW_BUTTON_UP.store(true, Ordering::SeqCst);
    mouse_log(&format!(
        "{caller}: armed for the release of button {button:#04x} of this press"
    ));
}

/// Appends one line to `%TEMP%\xshot-mouse.log`.
///
/// Called a handful of times in a session at most, so it opens the file each
/// time rather than holding a handle open.
pub(crate) fn mouse_log(message: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("xshot-mouse.log");
    let minutes = now_millis() / 60_000;
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "[{:>4}m{:02}s] {message}",
            minutes,
            (now_millis() / 1000) % 60
        );
    }
}

/// Which mouse button is being held right now, as its virtual key.
fn held_button() -> Option<u32> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
    };
    unsafe {
        [VK_LBUTTON, VK_RBUTTON, VK_MBUTTON]
            .into_iter()
            .find(|key| GetAsyncKeyState(key.0 as i32) < 0)
            .map(|key| key.0 as u32)
    }
}

/// The virtual key of the button a mouse message belongs to.
///
/// The second press of a double click arrives as a `DBLCLK` instead of a second
/// `DOWN`, so both count as presses — a claim left over from an earlier gesture
/// must not survive one.
fn button_of(message: u32) -> Option<(u32, bool)> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_LBUTTON, VK_MBUTTON, VK_RBUTTON};
    let pressed = match message {
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => (VK_LBUTTON.0 as u32, true),
        WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => (VK_RBUTTON.0 as u32, true),
        WM_MBUTTONDOWN | WM_MBUTTONDBLCLK => (VK_MBUTTON.0 as u32, true),
        WM_LBUTTONUP => (VK_LBUTTON.0 as u32, false),
        WM_RBUTTONUP => (VK_RBUTTON.0 as u32, false),
        WM_MBUTTONUP => (VK_MBUTTON.0 as u32, false),
        _ => return None,
    };
    Some(pressed)
}

/// Milliseconds since the process started, for the swallow's short lifetime.
fn now_millis() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Watches for Ctrl + left-drag and reports the dragged rectangle.
///
/// The hook never consumes the event, so the drag still reaches whatever window
/// is underneath; only the resulting rectangle is ours.
unsafe fn install_mouse_hook(hinstance: HINSTANCE) {
    unsafe {
        match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), Some(hinstance), 0) {
            Ok(hook) => {
                MOUSE_HOOK.store(hook.0 as isize, Ordering::SeqCst);
                crate::trace_log!("xshot: mouse hook installed");
            }
            Err(error) => crate::trace_log!("xshot: could not install the mouse hook: {error}"),
        }
    }
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        if code >= 0 {
            let message = wparam.0 as u32;

            // A press always means the previous click finished: any release we
            // were still holding a claim on was missed, and swallowing a later
            // one would eat a real click. A double click's second press counts
            // as a press here even though Windows sends it as a `DBLCLK`.
            if let Some((button, pressed)) = button_of(message) {
                if pressed {
                    SWALLOW_BUTTON_UP.store(false, Ordering::SeqCst);
                } else if SWALLOW_BUTTON_UP.load(Ordering::SeqCst)
                    // Only the button that was held when the window closed. A
                    // release of some other button is not part of that gesture.
                    && SWALLOW_BUTTON.load(Ordering::SeqCst) == button
                {
                    // Our windows close on the button *down* — right click
                    // cancels a screenshot, and the toolbar's 复制/钉图 act on
                    // the press too — so the matching release arrives when the
                    // window is already gone. Windows then hands it to whatever
                    // is behind, which is enough for that window to treat the
                    // click as its own and pop a context menu. Eating that one
                    // release here is the only place early enough to stop it.
                    //
                    // No time limit: the release belongs to that press whether
                    // the user holds the button for 50 ms or 5 s, and a limit
                    // only means a slow click gets through.
                    SWALLOW_BUTTON_UP.store(false, Ordering::SeqCst);
                    let armed_at = SWALLOW_ARMED_AT.load(Ordering::SeqCst);
                    let age = now_millis().saturating_sub(armed_at);
                    mouse_log(&format!("ate the release ({age} ms after arming)"));
                    return LRESULT(1);
                }
            }
        }

        if code >= 0 && CTRL_DRAG.load(Ordering::SeqCst) {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            let (x, y) = (info.pt.x, info.pt.y);
            match wparam.0 as u32 {
                WM_LBUTTONDOWN => {
                    // The press goes straight through to whatever is underneath —
                    // this hook only watches — and the overlay comes up on it, so
                    // the user sees the rectangle grow instead of finding out
                    // after the fact.
                    let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
                    let started = ctrl.then_some((x, y));
                    if started.is_some() {
                        *DRAG_SENT.lock().unwrap() = None;
                        send(Command::DragBegin { x, y });
                    }
                    *DRAG_START.lock().unwrap() = started;
                }
                WM_MOUSEMOVE => {
                    let start = *DRAG_START.lock().unwrap();
                    if start.is_some() {
                        // One message per two pixels: every one of them costs a
                        // wake-up of the UI thread and a repaint.
                        let sent = DRAG_SENT.lock().unwrap();
                        let moved = match *sent {
                            Some((last_x, last_y)) => {
                                (x - last_x).abs() + (y - last_y).abs() >= 2
                            }
                            None => true,
                        };
                        drop(sent);
                        if moved {
                            *DRAG_SENT.lock().unwrap() = Some((x, y));
                            send(Command::DragUpdate { x, y });
                        }
                    }
                }
                WM_LBUTTONUP => {
                    let start = DRAG_START.lock().unwrap().take();
                    *DRAG_SENT.lock().unwrap() = None;
                    if start.is_some() {
                        send(Command::DragEnd { x, y });
                    }
                }
                WM_RBUTTONDOWN => {
                    // The other button is the way out of a drag that is not
                    // going anywhere.
                    let start = DRAG_START.lock().unwrap().take();
                    *DRAG_SENT.lock().unwrap() = None;
                    if start.is_some() {
                        send(Command::DragCancel);
                    }
                }
                _ => {}
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Claims or releases `Esc` for the duration of a capture session.
///
/// A bare `Esc` is claimed from every other program while it is held, which is
/// exactly what a running session wants — the user is scrolling or watching, not
/// typing — and it is released the moment the session ends. Claiming only works
/// on the thread that owns the message queue, so this posts the request there.
pub fn arm_stop_hotkey(armed: bool) {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut _)),
            WM_APP_STOP_HOTKEY,
            WPARAM(armed as usize),
            LPARAM(0),
        );
    }
}

/// Shows a notification-area balloon.
///
/// Used for things that finish while our windows are out of the way — a saved
/// recording, or a scrolling shot that found nothing to stitch — which otherwise
/// end in silence.
pub fn notify(message: &str) {
    let hwnd = TRAY_HWND.load(Ordering::SeqCst);
    if hwnd == 0 {
        return;
    }
    unsafe {
        use windows::Win32::UI::Shell::{NIF_INFO, NIM_MODIFY, Shell_NotifyIconW};
        let mut data = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: HWND(hwnd as *mut _),
            uID: TRAY_ICON_ID,
            uFlags: NIF_INFO,
            ..Default::default()
        };
        let title: Vec<u16> = "xshot".encode_utf16().collect();
        copy_into(&mut data.szInfoTitle, &title);
        let body: Vec<u16> = message.encode_utf16().collect();
        copy_into(&mut data.szInfo, &body);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

/// Fills a fixed-size UTF-16 field, leaving the terminator in place.
fn copy_into(field: &mut [u16], text: &[u16]) {
    let room = field.len().saturating_sub(1);
    let length = text.len().min(room);
    field[..length].copy_from_slice(&text[..length]);
    field[length] = 0;
}

/// Claims or releases the session's stop key on the tray thread.
///
/// The claim has to be made from the thread that owns the window the hotkey is
/// registered against, which is the tray thread, so [`arm_stop_hotkey`] posts
/// here rather than calling `RegisterHotKey` itself.
unsafe fn set_stop_hotkey(hwnd: HWND, armed: bool) {
    unsafe {
        use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
        if armed {
            match RegisterHotKey(Some(hwnd), HOTKEY_STOP_ID, HOT_KEY_MODIFIERS(0), VK_ESCAPE.0 as u32)
            {
                Ok(()) => crate::trace_log!("xshot: Esc 已接管，用于结束长截图/录像"),
                Err(error) => {
                    crate::trace_log!("xshot: 无法接管 Esc（{error}），请用托盘菜单结束")
                }
            }
        } else {
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_STOP_ID);
        }
    }
}

/// Shows the tray's context menu and turns the choice into a command.
unsafe fn show_menu(hwnd: HWND) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else {
            return;
        };
        let _ = AppendMenuW(menu, MF_STRING, MENU_CAPTURE, w!("截图"));
        let _ = AppendMenuW(menu, MF_STRING, MENU_RESTORE, w!("恢复上次贴图"));
        // Only while a session is running: it is the way out for anyone the Esc
        // claim did not reach.
        if crate::capture::is_active() {
            let _ = AppendMenuW(menu, MF_STRING, MENU_STOP, w!("结束长截图/录像"));
        }
        let _ = AppendMenuW(menu, MF_STRING, MENU_SETTINGS, w!("打开设置"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, MENU_EXIT, w!("退出程序"));

        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);

        // Required, otherwise the menu stays up when the user clicks away.
        let _ = SetForegroundWindow(hwnd);
        let chosen = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
            cursor.x,
            cursor.y,
            None,
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);
        // Documented workaround for the menu lingering after a dismissal.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));

        match chosen.0 as usize {
            MENU_CAPTURE => send(Command::Capture),
            MENU_RESTORE => send(Command::RestorePin),
            MENU_STOP => send(Command::StopCapture),
            MENU_SETTINGS => send(Command::ShowSettings),
            MENU_EXIT => send(Command::Quit),
            _ => {}
        }
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_TRAY => {
                // NOTIFYICON_VERSION_4 packs the cursor into `wparam` and the
                // event into the low word of `lparam`.
                let event = (lparam.0 as u32) & 0xFFFF;
                match event {
                    WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                    WM_LBUTTONDBLCLK => send(Command::Capture),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                match wparam.0 as i32 {
                    HOTKEY_CAPTURE_ID => send(Command::Capture),
                    HOTKEY_RESTORE_ID => send(Command::RestorePin),
                    HOTKEY_STOP_ID => send(Command::StopCapture),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_APPLY_HOTKEYS => {
                apply_hotkeys(hwnd);
                LRESULT(0)
            }
            WM_APP_STOP_HOTKEY => {
                set_stop_hotkey(hwnd, wparam.0 != 0);
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                remove_icon(hwnd);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_CAPTURE_ID);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_RESTORE_ID);
                let _ = UnregisterHotKey(Some(hwnd), HOTKEY_STOP_ID);
                let hook = MOUSE_HOOK.swap(0, Ordering::SeqCst);
                if hook != 0 {
                    let _ = UnhookWindowsHookEx(HHOOK(hook as *mut _));
                }
                TRAY_HWND.store(0, Ordering::SeqCst);
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => {
                let taskbar_created = TASKBAR_CREATED.load(Ordering::SeqCst);
                if taskbar_created != 0 && message == taskbar_created {
                    // Explorer restarted and dropped our icon.
                    add_icon(hwnd);
                    return LRESULT(0);
                }
                DefWindowProcW(hwnd, message, wparam, lparam)
            }
        }
    }
}
