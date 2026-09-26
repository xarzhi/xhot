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

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
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
    /// Capture an exact rectangle, in physical screen pixels, and pin it. Sent
    /// by the Ctrl+drag gesture.
    CaptureRegion { x: i32, y: i32, w: i32, h: i32 },
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
        if code >= 0 && CTRL_DRAG.load(Ordering::SeqCst) {
            let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
            match wparam.0 as u32 {
                WM_LBUTTONDOWN => {
                    let ctrl = GetAsyncKeyState(VK_CONTROL.0 as i32) < 0;
                    let mut start = DRAG_START.lock().unwrap();
                    *start = ctrl.then_some((info.pt.x, info.pt.y));
                }
                WM_LBUTTONUP => {
                    let start = DRAG_START.lock().unwrap().take();
                    if let Some((x0, y0)) = start {
                        let (x1, y1) = (info.pt.x, info.pt.y);
                        let w = (x1 - x0).abs();
                        let h = (y1 - y0).abs();
                        if w >= 8 && h >= 8 {
                            send(Command::CaptureRegion {
                                x: x0.min(x1),
                                y: y0.min(y1),
                                w,
                                h,
                            });
                        }
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
