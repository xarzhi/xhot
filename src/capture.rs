//! Capture sessions that outlive the overlay: scrolling screenshots and screen
//! recording.
//!
//! A scrolling shot hides the overlay while it runs, because the thing being
//! captured is the live screen behind it and the stitched result must not
//! contain our own frame. A recording cannot do that: the user needs to see the
//! region, and needs to stop it with a button, so the overlay stays on screen
//! in a stripped-down state — the frame, and nothing else, drawn *outside* the
//! captured rectangle so it never lands in the video — with `WS_EX_TRANSPARENT`
//! set so every click still belongs to the application underneath. The controls
//! live in their own window ([`crate::ui::record_bar`]), which is the only thing
//! on screen that takes a click.
//!
//! Either way a worker grabs the region again and again while the user drives
//! the application underneath, and `Esc` ends it: a global hotkey is claimed
//! for the duration, since our own windows are out of the way and there is
//! nothing else left to hear the key.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use gpui_kit::*;

use crate::app;
use crate::win::encode::{self, Format};
use crate::win::frame::{self, Dib};
use crate::win::tray;
use crate::win::util as win_util;

/// How often the region is grabbed while a scrolling shot is running.
const INTERVAL: Duration = Duration::from_millis(120);
/// Longest a session may run before it stops itself, so a forgotten one cannot
/// run for ever.
const MAX_SECONDS: u64 = 300;
/// Recording ceiling for the one format that is buffered in memory before it is
/// written, so a forgotten recording cannot eat all the memory on the machine.
const MAX_RECORDING_BYTES: usize = 400 * 1024 * 1024;
/// A scrolling shot stops growing here; past it there is nothing useful left.
const MAX_STITCH_HEIGHT: i32 = 20_000;
/// Average per-channel difference below which two rows count as the same row.
const MATCH_TOLERANCE: u64 = 8;

/// Set while a session is running, so a second one cannot start on top of it.
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// Set only while frames are actually going into a file, which is what makes
/// the toolbar's stop button meaningful: before that the session is framed and
/// waiting for the user to press start.
static RECORDING: AtomicBool = AtomicBool::new(false);
/// Set while a recording session exists but no frames are being written yet:
/// the toolbar is up and the start button has not been pressed.
static AWAITING_START: AtomicBool = AtomicBool::new(false);
/// The framed region a session was started with, kept so the toolbar can begin
/// the recording after the overlay has handed it over.
static REGION: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);
/// When the current recording started, and how many frames have gone in. Read
/// by the toolbar's clock, which is on another thread.
static STARTED: Mutex<Option<Instant>> = Mutex::new(None);
static FRAMES: AtomicUsize = AtomicUsize::new(0);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

/// Whether frames are currently being written.
pub fn is_recording() -> bool {
    RECORDING.load(Ordering::SeqCst)
}

/// How long the current recording has been running, and how many frames it has.
pub fn progress() -> Option<(Duration, usize)> {
    let started = *STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    started.map(|started| (started.elapsed(), FRAMES.load(Ordering::SeqCst)))
}

/// Ends whatever session is running. Called when the stop key arrives.
pub fn stop() {
    crate::timing("s stop requested");
    ACTIVE.store(false, Ordering::SeqCst);
}

/// Ends a session from outside its own worker: the stop key, the tray menu.
///
/// A recording that is already writing frames stops itself — its worker has a
/// file to finish and a toolbar to close. One that is still waiting for the
/// start button has no worker at all, so if this only cleared the flag the
/// frame and the toolbar would sit there for ever, click-through and with no
/// way left to dismiss them. That case is torn down here instead.
pub fn request_stop(cx: &mut App) {
    stop();
    if AWAITING_START.swap(false, Ordering::SeqCst) {
        crate::ui::record_bar::close(cx);
        end_session(cx, true);
    }
}

/// Abandons a recording that has not started yet.
///
/// The toolbar's close button: the user framed a region, opened the toolbar and
/// changed their mind. Everything goes back exactly as it was.
pub fn cancel_recording(cx: &mut App) {
    AWAITING_START.store(false, Ordering::SeqCst);
    stop();
    crate::ui::record_bar::close(cx);
    end_session(cx, true);
}

/// Registers the region a session will use.
///
/// The frame can still be dragged around before a recording starts, so the
/// region is kept here and read when the recording actually begins rather than
/// being fixed when the toolbar opened.
pub fn set_region(region: (i32, i32, i32, i32)) {
    *REGION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(region);
}

/// Starts stitching a scrolling window into one tall shot.
///
/// The overlay stays, reduced to its frame: the user has to be able to see
/// which part of the window is being stitched while scrolling it.
pub fn start_scrolling(cx: &mut App, region: (i32, i32, i32, i32)) {
    if !begin() {
        return;
    }
    set_region(region);
    crate::timing("s scrolling session started");
    cx.spawn(async move |cx| {
        let stitched = cx
            .background_executor()
            .spawn(async move { scroll_loop(region) })
            .await;
        let _ = cx.update(|cx| match stitched {
            // The shot is finished, so the capture is over: hand it to the save
            // window and let the overlay go.
            Some(shot) => {
                crate::timing("s stitching done");
                end_session(cx, false);
                crate::ui::save_dialog::open(cx, shot);
            }
            // Nothing usable: put the user back where they were.
            None => {
                crate::timing("s nothing to stitch");
                end_session(cx, true);
                tray::notify("长截图没有采集到可拼接的内容");
            }
        });
    })
    .detach();
}

/// Hands the framed region over to a recording: the frame stays on screen and
/// the toolbar opens to ask how to record it.
///
/// Nothing is captured yet. This is deliberate — the format and the frame rate
/// have to be settled before the first frame, a file cannot change container
/// halfway through, and the frame can still be dragged somewhere else until the
/// recording begins.
///
/// Not reachable yet: the capture toolbar has the long-shot button but no record
/// button, so nothing calls this. The rest of the recording path is here and
/// working, waiting for that button.
#[allow(dead_code)]
pub fn start_recording(cx: &mut App, region: (i32, i32, i32, i32)) {
    if !begin() {
        return;
    }
    set_region(region);
    // The overlay has already put itself into frame-only mode — it owns that
    // flag, and it cannot be reached from here at this point, since this call
    // arrives from inside its own event handling. Its window region is what
    // makes the rest of the desktop clickable, and it keeps that up itself.
    AWAITING_START.store(true, Ordering::SeqCst);
    crate::ui::record_bar::open(cx, region);
}

/// Begins writing frames to a file. Called by the toolbar's start button.
///
/// The region is taken from the session rather than from the toolbar, which
/// opened before the frame was dragged to where it belongs.
///
/// Returns whether the recording actually started, so the toolbar can stay in
/// its ready phase when it did not.
pub fn begin_recording(cx: &mut App, format: Format, fps: u32) -> bool {
    if !is_active() || is_recording() {
        return false;
    }
    let Some(region) = *REGION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    else {
        return false;
    };
    // H.264 codes colour at half resolution, so its frames have to be an even
    // number of pixels across and down. A drag that ends on an odd pixel — which
    // is every drag that ends wherever the pointer happens to be — hands the
    // encoder a media type it refuses outright (MF_E_INVALIDMEDIATYPE), and the
    // recording then fails before a single frame: a 0-byte file and a "save
    // failed" balloon. One pixel off the right and bottom edges costs nothing.
    let region = (
        region.0,
        region.1,
        (region.2 - region.2.rem_euclid(2)).max(2),
        (region.3 - region.3.rem_euclid(2)).max(2),
    );
    lock_frame(cx, true);
    let directory = {
        let configured = app::state(cx).config.save_dir.clone();
        if configured.is_dir() {
            configured
        } else {
            crate::config::default_save_dir()
        }
    };
    let target = directory.join(format!(
        "xshot_record_{}.{}",
        chrono::Local::now().format("%Y%m%d_%H%M%S"),
        format.extension()
    ));

    *STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    FRAMES.store(0, Ordering::SeqCst);
    AWAITING_START.store(false, Ordering::SeqCst);
    RECORDING.store(true, Ordering::SeqCst);

    cx.spawn(async move |cx| {
        let path = target.clone();
        let written = cx
            .background_executor()
            .spawn(async move { record_loop(region, format, fps, &path) })
            .await;
        RECORDING.store(false, Ordering::SeqCst);

        // A recording ends while nothing of ours is on screen but the toolbar,
        // so a balloon is the only place the result can be reported.
        let message = match &written {
            Ok((count, None)) => format!("已保存 {count} 帧到 {}", target.display()),
            // It still saved: say what went wrong and that the rest is in the
            // file, rather than claiming the whole thing failed.
            Ok((count, Some(trouble))) => format!(
                "录像中途出错，已保存前 {count} 帧到 {}（{trouble}）",
                target.display()
            ),
            Err(error) => format!("录像保存失败：{error}"),
        };
        crate::trace_log!("xshot: recording outcome: {message} / {written:?}");
        tray::notify(&message);
        let _ = cx.update(|cx| {
            crate::ui::record_bar::close(cx);
            // The recording is written, so the session is over: the overlay goes
            // with it. Leaving it up in framing mode would put a full-screen
            // window back over a desktop the user is done with, and every click
            // would land on it.
            end_session(cx, false);
        });
    })
    .detach();
    true
}

// -- session plumbing --------------------------------------------------------

/// Takes the session slot and gets the stop key ready.
///
/// The overlay is *not* hidden any more, for either kind of session: it keeps
/// the frame up, and its window region is cut back to that frame alone, so the
/// desktop outside it — the part being photographed — belongs to whoever is
/// underneath. Putting the overlay away would also mean the user could not see
/// what is being captured, which is the whole complaint this answers.
fn begin() -> bool {
    if ACTIVE.swap(true, Ordering::SeqCst) {
        return false;
    }
    tray::arm_stop_hotkey(true);
    true
}

/// Ends the session and either restores or disposes of the overlay.
fn end_session(cx: &mut App, restore_overlay: bool) {
    ACTIVE.store(false, Ordering::SeqCst);
    RECORDING.store(false, Ordering::SeqCst);
    AWAITING_START.store(false, Ordering::SeqCst);
    *REGION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    *STARTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    tray::arm_stop_hotkey(false);
    // A session left the overlay on screen as a bare frame; give the framing
    // tools back — and the whole window region with them — before the user can
    // use them again.
    set_overlay_frame_only(cx, false);
    if restore_overlay {
        if app::state(cx).overlay.is_some() {
            win_util::show_window(app::OVERLAY_TITLE);
            if let Some(hwnd) = win_util::find_window(app::OVERLAY_TITLE) {
                win_util::force_foreground(hwnd);
            }
        }
    } else if let Some(handle) = app::state_mut(cx).overlay.take() {
        win_util::release_capture();
        // The session is over and the overlay is going: give the foreground back
        // before the window that owns it is destroyed. See
        // [`crate::win::util::restore_foreground`].
        app::restore_foreground(cx);
        remove_overlay(cx, handle);
    }
}

/// Takes the overlay off the screen, through Win32 first.
///
/// `remove_window` only sets a flag that GPUI drains on a later frame, and this
/// window is the one thing that must not outlive the session: by then its region
/// has been given back — the whole screen again — while what it last painted is
/// a bare frame over a transparent surface. A window in that state is invisible
/// and eats every click on the desktop, and the only way out of it is a right
/// click landing on the window itself to close it. Hiding it here removes any
/// dependence on when that later frame arrives.
fn remove_overlay(cx: &mut App, handle: gpui_kit::WindowHandle<crate::ui::overlay::Overlay>) {
    win_util::set_window_region(app::OVERLAY_TITLE, &[]);
    win_util::hide_window(app::OVERLAY_TITLE);
    let _ = handle.update(cx, |_, window, _| window.remove_window());
}

/// Stops the frame from being dragged any more, once frames are being captured.
fn lock_frame(cx: &mut App, locked: bool) {
    if let Some(handle) = app::state(cx).overlay {
        let _ = handle.update(cx, |overlay, _window, cx| {
            overlay.set_frame_locked(locked);
            cx.notify();
        });
    }
}

/// Switches the overlay between its normal framing state and the stripped-down
/// frame a session leaves on screen.
///
/// In frame-only mode the overlay draws no frozen desktop and no dimming, only
/// the outline of the region — and its window region is cut back to that
/// outline, so the user can work in the application being captured while still
/// seeing exactly what is being captured.
fn set_overlay_frame_only(cx: &mut App, frame_only: bool) {
    if let Some(handle) = app::state(cx).overlay {
        let _ = handle.update(cx, |overlay, _window, cx| {
            overlay.set_frame_only(frame_only);
            cx.notify();
        });
    }
}

// -- scrolling shots ---------------------------------------------------------

fn scroll_loop(region: (i32, i32, i32, i32)) -> Option<Dib> {
    let started = Instant::now();
    let mut stitched: Option<Dib> = None;
    let mut last: Option<Dib> = None;

    while ACTIVE.load(Ordering::SeqCst) && started.elapsed().as_secs() < MAX_SECONDS {
        std::thread::sleep(INTERVAL);
        let Some(frame) = grab(region) else {
            continue;
        };
        let Some(previous) = last.take() else {
            // Two copies: one becomes the stitched image, the other tracks how
            // far the window has scrolled since.
            let Some(base) = frame.crop(0, 0, frame.width, frame.height).ok() else {
                continue;
            };
            stitched = Some(frame);
            last = Some(base);
            continue;
        };
        match scroll_offset(&previous, &frame) {
            // The window scrolled: keep the rows below the overlap.
            Some(delta) => {
                if let Some(total) = stitched.as_mut() {
                    append(total, &frame, delta);
                }
                last = Some(frame);
            }
            // Same picture, or a jump too large to place: wait for a frame that
            // can be aligned instead of stitching something wrong.
            None => last = Some(frame),
        }
    }
    stitched
}

/// How far the content scrolled between two frames, in pixels.
///
/// Row signatures are compared rather than pixels: 64 samples per row is plenty
/// to tell rows apart, and it turns the search into something that finishes in a
/// couple of milliseconds per frame.
fn scroll_offset(previous: &Dib, next: &Dib) -> Option<i32> {
    let height = previous.height.min(next.height);
    let width = previous.width.min(next.width);
    if height < 64 || width < 8 {
        return None;
    }
    // Nothing moved. This has to be checked before the search rather than
    // inferred from it: with repeated content — a list, a table, a page of
    // identical rows — *every* offset that lands on a repeat matches about as
    // well as the right one, and the search would happily append rows that are
    // already in the shot.
    if !differs(previous, next) {
        return None;
    }
    let columns: Vec<i32> = (0..64).map(|i| (i * width / 64).min(width - 1)).collect();
    let signature = |dib: &Dib, row: i32| -> Vec<u8> {
        let pixels = dib.pixels();
        columns
            .iter()
            .flat_map(|column| {
                let index = (row as usize * dib.width as usize + *column as usize) * 4;
                [pixels[index], pixels[index + 1], pixels[index + 2]]
            })
            .collect()
    };
    let previous_rows: Vec<Vec<u8>> = (0..height).map(|row| signature(previous, row)).collect();
    let next_rows: Vec<Vec<u8>> = (0..height).map(|row| signature(next, row)).collect();

    // The content moved up by `delta`, so the top of the new frame matches the
    // previous frame `delta` rows further down.
    let per_row = columns.len() as u64 * 3;
    let mut best: Option<(i32, u64)> = None;
    for delta in 1..height / 2 {
        let overlap = height - delta;
        let mut cost = 0u64;
        let mut rows = 0u64;
        let mut row = 0;
        while row < overlap {
            let a = &previous_rows[(delta + row) as usize];
            let b = &next_rows[row as usize];
            cost += a
                .iter()
                .zip(b.iter())
                .map(|(x, y)| x.abs_diff(*y) as u64)
                .sum::<u64>();
            rows += 1;
            // Every other row is enough, and halves the work.
            row += 2;
        }
        let normalized = cost / rows.max(1) / per_row;
        if best.map(|(_, cost)| normalized < cost).unwrap_or(true) {
            best = Some((delta, normalized));
        }
    }

    let (delta, cost) = best?;
    (cost <= MATCH_TOLERANCE).then_some(delta)
}

/// Whether two frames differ enough to be worth aligning.
///
/// A *significant* difference, not any difference: a blinking text caret makes
/// consecutive frames technically different, and feeding those to the aligner on
/// a page of repeated rows is exactly how a shot ends up with duplicated lines.
/// Sampling every 16th pixel and asking for a couple of percent of them to have
/// moved ignores a caret while still catching a one-pixel scroll.
fn differs(a: &Dib, b: &Dib) -> bool {
    if a.width != b.width || a.height != b.height {
        return true;
    }
    let stride = 16 * 4;
    let mut samples = 0u64;
    let mut moved = 0u64;
    for (x, y) in a
        .pixels()
        .iter()
        .step_by(stride)
        .zip(b.pixels().iter().step_by(stride))
    {
        samples += 1;
        if x.abs_diff(*y) > 3 {
            moved += 1;
        }
    }
    moved * 100 > samples * 2
}

/// Appends the rows of `frame` that are new, growing `total` downwards.
///
/// If the window scrolled by `delta`, the frame shows the same rows `delta`
/// further down plus `delta` brand new rows at the bottom — and those new rows
/// are the last `delta` rows of the frame, which is the whole reason the amount
/// is `delta` and not `height - delta`.
fn append(total: &mut Dib, frame: &Dib, delta: i32) {
    let new_rows = delta;
    if new_rows <= 0 || total.height + new_rows > MAX_STITCH_HEIGHT {
        return;
    }
    let width = total.width.min(frame.width);
    let Ok(extra) = frame.crop(0, frame.height - new_rows, width, new_rows) else {
        return;
    };
    if let Ok(combined) = concat_vertical(total, &extra) {
        *total = combined;
    }
}

/// Stacks two bitmaps, which is all a vertical stitch needs.
fn concat_vertical(top: &Dib, bottom: &Dib) -> Result<Dib> {
    let mut out = Dib::new(top.width, top.height + bottom.height)
        .context("allocating the stitched shot")?;
    let stride = top.width as usize * 4;
    {
        let pixels = out.pixels_mut();
        for row in 0..top.height as usize {
            pixels[row * stride..(row + 1) * stride]
                .copy_from_slice(&top.pixels()[row * stride..(row + 1) * stride]);
        }
        for row in 0..bottom.height as usize {
            let target = (top.height as usize + row) * stride;
            pixels[target..target + stride]
                .copy_from_slice(&bottom.pixels()[row * stride..(row + 1) * stride]);
        }
    }
    Ok(out)
}

// -- recording ---------------------------------------------------------------

/// Grabs the region at the requested rate and feeds the frames to `encode`.
///
/// The loop keeps to a cadence rather than running flat out: a recording made
/// at whatever rate the machine happens to manage plays back at the wrong
/// speed. Missing a tick is fine — the timestamp written with each frame is the
/// real elapsed time, so a dropped frame becomes a longer sample rather than a
/// jump.
fn record_loop(
    region: (i32, i32, i32, i32),
    format: Format,
    fps: u32,
    path: &std::path::Path,
) -> Result<(usize, Option<String>)> {
    let (x, y, w, h) = region;
    let interval = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
    let started = Instant::now();
    let mut next = started;
    let mut writer: Option<encode::Writer> = None;
    // One bitmap for the whole recording, refilled in place: at sixty frames a
    // second a fresh allocation per frame would cost as much as the capture.
    let mut buffer = Dib::new(w, h).context("allocating the recording buffer")?;
    // Set if a frame could not be handed over. The recording is still finished
    // from what did go in: a 0-byte file and a "save failed" balloon is the
    // worst possible answer, and it is what a full-screen recording used to get.
    let mut trouble: Option<String> = None;

    while ACTIVE.load(Ordering::SeqCst) && started.elapsed().as_secs() < MAX_SECONDS {
        let now = Instant::now();
        if now < next {
            std::thread::sleep(next - now);
        } else if now > next + interval {
            // Fell behind by more than a frame: skip the ticks we missed
            // instead of trying to catch up in a burst.
            next = now;
        }
        next += interval;

        let elapsed = started.elapsed();
        if buffer.grab_from_screen(x, y).is_err() {
            continue;
        }
        let writer = match &mut writer {
            Some(writer) => writer,
            None => match encode::Writer::open(format, path, w as u32, h as u32, fps) {
                Ok(opened) => writer.insert(opened),
                // Nothing can be recorded, so say so plainly instead of leaving
                // the caller to describe an empty file.
                Err(error) => return Err(error.context("opening the recording")),
            },
        };
        if let Err(error) = writer.push(&buffer, elapsed) {
            trouble = Some(format!("{error:#}"));
            break;
        }
        FRAMES.store(FRAMES.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
        if writer.bytes() > MAX_RECORDING_BYTES {
            break;
        }
    }

    match writer {
        Some(writer) => {
            let frames = writer.finish(path)?;
            Ok((frames, trouble))
        }
        None => anyhow::bail!("没有采集到任何画面"),
    }
}

/// One frame of a session: the region, straight off the desktop.
fn grab(region: (i32, i32, i32, i32)) -> Option<Dib> {
    let (x, y, w, h) = region;
    frame::capture_region(x, y, w, h).ok()
}