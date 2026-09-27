//! xshot — a small, fast Windows screenshot tool built on GPUI.
//!
//! GPUI owns every window and all interaction. Win32 is used only where a UI
//! toolkit cannot reach: grabbing screen pixels, the notification area, the
//! global hotkey and the clipboard bitmap formats.

// No console window in release builds; debug builds keep it for diagnostics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Progress logging that is compiled out of release builds entirely.
#[macro_export]
macro_rules! trace_log {
    ($($arg:tt)*) => {
        if cfg!(debug_assertions) {
            eprintln!($($arg)*);
        }
    };
}

/// Phase timing for the capture path, off unless `XSHOT_TIMING` is set.
///
/// The walk from hotkey to the frozen screen appearing is the one thing a user
/// actually feels, and it is made of several parts that live in different
/// threads. Setting `XSHOT_TIMING=1` appends a timestamped line per phase to
/// `%TEMP%\xshot-timing.log`, which works on a release build too — the whole
/// point, since that is the build whose latency matters.
pub fn timing(label: &str) {    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    // The handle is opened once and kept: opening, writing and closing a file
    // per mark cost more than the phases being measured.
    static SINK: OnceLock<Option<Mutex<std::fs::File>>> = OnceLock::new();
    static BASE: OnceLock<std::time::Instant> = OnceLock::new();
    let sink = SINK.get_or_init(|| {
        if std::env::var_os("XSHOT_TIMING").is_none() {
            return None;
        }
        let path = std::env::temp_dir().join("xshot-timing.log");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
            .map(Mutex::new)
    });
    let Some(sink) = sink else {
        return;
    };
    let base = *BASE.get_or_init(std::time::Instant::now);
    if let Ok(mut file) = sink.lock() {
        let _ = writeln!(file, "{:>9.2}ms  {label}", base.elapsed().as_secs_f64() * 1000.0);
    }
}

mod annot;
mod app;
mod capture;
mod config;
mod ui;
mod win;

/// The output path when started as `xshot --export-icon <path>`.
///
/// Only used when preparing a release; the app never looks at its arguments
/// otherwise.
fn icon_export_path() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--export-icon" {
            return Some(args.next().unwrap_or_else(|| "assets/icon.ico".to_string()));
        }
    }
    None
}

fn main() {
    install_panic_log();

    // Build-time utility: materialise the icon that is otherwise only ever drawn
    // at runtime, for the installer to embed. `xshot --export-icon assets/icon.ico`
    if let Some(path) = icon_export_path() {
        match win::icon::write_ico(std::path::Path::new(&path)) {
            Ok(()) => eprintln!("xshot: wrote {path}"),
            Err(error) => eprintln!("xshot: could not write {path}: {error:#}"),
        }
        return;
    }

    // Must happen before the first window exists, otherwise Windows reports a
    // scaled-down virtual desktop and screenshots come back blurry.
    win::util::set_dpi_awareness();

    let configuration = config::Config::load();

    // Keep the login entry pointing at wherever this copy actually lives; an
    // update or reinstall moves the executable.
    win::autostart::sync(configuration.autostart);

    let (commands, receiver) = async_channel::unbounded::<win::tray::Command>();
    let capture_hotkey = win::hotkey::parse(&configuration.hotkey_capture);
    let restore_hotkey = win::hotkey::parse(&configuration.hotkey_restore_pin);
    if let Err(error) = win::tray::spawn(
        commands,
        capture_hotkey,
        restore_hotkey,
        configuration.ctrl_drag_pin,
    ) {
        eprintln!("xshot: tray thread failed to start: {error}");
    }

    // GPUI's default quit mode on Windows is `LastWindowClosed`, which would
    // end the process the moment the settings window is closed — that is
    // precisely the "minimize to tray" case, and tearing the platform down
    // under a pending task is not a survivable state. xshot decides for itself
    // when to exit, so the mode is explicit.
    gpui_kit::application()
        .with_quit_mode(gpui_kit::QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            // The icon font has to be registered before the first window draws,
            // or every icon comes out as a fallback glyph.
            ui::icons::load(cx);
            let silent = configuration.silent_start;
            cx.set_global(app::AppState::new(configuration));

            // The tray icon is the app. Unless the user asked for the window,
            // starting up means starting quietly.
            if !silent {
                app::open_settings(cx);
            }

            // Get the capture path ready while nobody is waiting: building the
            // Direct3D device and the per-monitor duplications takes tens of
            // milliseconds and the first frame has to come from GDI, and both
            // belong at startup rather than in the walk from the hotkey to a
            // frozen screen.
            cx.background_executor()
                .spawn(async { win::duplication::warm() })
                .detach();

            // The capture window is the other half of that walk: building it costs
            // about 18 ms (the window, its Direct3D swapchain and its first frame),
            // and none of it depends on the screen. Building it here means the
            // first F1 — or the first Ctrl+drag — only has to place and show it.
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(150))
                    .await;
                let _ = cx.update(|cx| app::prepare_overlay(cx));
            })
            .detach();

            // The tray thread never touches the UI directly; it posts commands
            // here and this task wakes the moment one arrives.
            cx.spawn(async move |cx| {
                loop {
                    let Ok(command) = receiver.recv().await else {
                        break;
                    };
                    let quitting = command == win::tray::Command::Quit;
                    if command == win::tray::Command::Capture {
                        crate::timing("1 command received");
                    }
                    cx.update(|cx| match command {
                        win::tray::Command::Capture => app::trigger_capture(cx),
                        win::tray::Command::DragBegin { x, y } => app::drag_begin(cx, x, y),
                        win::tray::Command::DragUpdate { x, y } => app::drag_update(cx, x, y),
                        win::tray::Command::DragEnd { x, y } => app::drag_end(cx, x, y),
                        win::tray::Command::DragCancel => app::drag_cancel(cx),
                        win::tray::Command::RestorePin => app::restore_last_pin(cx),
                        win::tray::Command::ShowSettings => app::open_settings(cx),
                        win::tray::Command::HotkeyStatus { capture, restore } => {
                            app::state_mut(cx).hotkey_status = Some((capture, restore));
                        }
                        win::tray::Command::Quit => app::quit(cx),
                        // A session that is already running is finished by its
                        // own worker; one that is still waiting for a start
                        // button has none, so the UI thread takes it down here.
                        win::tray::Command::StopCapture => capture::request_stop(cx),
                    });
                    if quitting {
                        break;
                    }
                }
            })
            .detach();
        });

    // Make sure no ghost icon is left behind in the notification area.
    win::tray::shutdown();
}

/// Records panics to `%TEMP%\xshot-panic.log`.
///
/// Release builds have no console attached, so without this a crash inside the
/// window procedure would be completely silent.
fn install_panic_log() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write as _;
        let path = std::env::temp_dir().join("xshot-panic.log");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = writeln!(file, "--- xshot panic ---");
            let _ = writeln!(file, "{info}");
            let _ = writeln!(file, "{}", std::backtrace::Backtrace::force_capture());
        }
        previous(info);
    }));
}
