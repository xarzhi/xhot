//! The recording toolbar: pick a format and a frame rate, start, stop.
//!
//! While a recording runs the user is working in the application being
//! recorded, so the frame that marks the region gives up everything except
//! itself — its window region is cut back to the frame alone, and only those
//! few pixels still take a click. A window that thin cannot host a button, so
//! the controls live here instead, in a second window pinned near the region:
//! the only thing on screen that still takes a click.
//!
//! Two phases, one window. It opens in *ready*, showing the format and frame
//! rate the recording would use and a start button; pressing start switches it
//! to *recording*, showing a stop button and a running clock. The settings are
//! fixed once recording begins — a container cannot be changed halfway through
//! a file without either splitting the file or lying about it.
//!
//! Both settings are dropdowns. The list opens inside this same window, which
//! grows downwards to make room for it: a second popup window would have to
//! fight the frame for the foreground, and this one is already topmost and
//! already excluded from screen capture.
//!
//! The window is excluded from screen capture, so the toolbar the user sees is
//! never part of the video, even when the region covers the whole desktop.

use gpui_kit::*;

use crate::app;
use crate::ui::theme::{
    CONTROL_HEIGHT, RADIUS_CARD, RADIUS_CONTROL, TEXT_BODY, TEXT_CAPTION, Theme,
};
use crate::win::encode::{DEFAULT_RATE, Format, RATES};
use crate::win::util as win_util;

/// Logical size of the toolbar window, closed and open.
///
/// The width is the sum of the row, spelled out rather than left to the layout
/// engine: a toolbar whose buttons move when the clock appears is a toolbar you
/// misclick.
const BAR_WIDTH: f32 = 548.0;
const BAR_HEIGHT: f32 = 52.0;
/// Gap between the row and an open list, and the list's own padding.
const LIST_GAP: f32 = 4.0;
const LIST_PADDING: f32 = 6.0;
const ITEM_HEIGHT: f32 = 30.0;

/// Width of the two dropdowns, the start/stop button, and the close button.
const FORMAT_SELECT: f32 = 96.0;
const RATE_SELECT: f32 = 88.0;
const ACTION_WIDTH: f32 = 132.0;
const CLOSE_WIDTH: f32 = 36.0;

/// Gap between the region and the toolbar, in physical pixels.
const GAP: i32 = 10;

/// Which of the two things the toolbar is doing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Framed and waiting: format and rate are still changeable.
    Ready,
    /// Frames are going into a file.
    Recording,
}

/// Which dropdown is open, if either.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Format,
    Rate,
}

pub struct RecordBar {
    region: (i32, i32, i32, i32),
    format: Format,
    fps: u32,
    phase: Phase,
    open_menu: Option<Menu>,
    /// Where the window sits, in physical pixels, and the scale it was placed
    /// at. Kept because both have to be re-stated whenever the window changes
    /// height for a dropdown.
    origin: (i32, i32),
    scale: f32,
    theme: Theme,
    focus: FocusHandle,
}

impl RecordBar {
    fn new(
        region: (i32, i32, i32, i32),
        format: Format,
        fps: u32,
        origin: (i32, i32),
        scale: f32,
        theme: Theme,
        cx: &mut Context<Self>,
    ) -> Self {
        // The clock is the only thing on screen that moves while a recording
        // runs, so it asks for its own repaints instead of waiting for a mouse
        // move to bring one along. The loop ends when the window is gone:
        // `update` fails once the entity has been released.
        cx.spawn(async move |bar, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(180))
                    .await;
                let alive = bar
                    .update(cx, |bar, cx| {
                        if bar.phase == Phase::Recording {
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        Self {
            region,
            format,
            fps,
            phase: Phase::Ready,
            open_menu: None,
            origin,
            scale,
            theme,
            focus: cx.focus_handle(),
        }
    }

    /// Resizes the window to fit the row, plus a list if one is open.
    fn fit(&self) {
        let height = match self.open_menu {
            None => BAR_HEIGHT,
            Some(_) => BAR_HEIGHT + LIST_GAP + list_height(),
        };
        win_util::place_client_exact(
            app::RECORD_BAR_TITLE,
            self.origin.0,
            self.origin.1,
            (BAR_WIDTH * self.scale).round() as i32,
            (height * self.scale).round() as i32,
        );
        // A taller window must not start eating the desktop beyond the row and
        // the list, so it is narrowed to those two shapes.
        let width = BAR_WIDTH * self.scale;
        let mut bands = vec![(0, 0, width.round() as i32, (BAR_HEIGHT * self.scale).round() as i32)];
        if self.open_menu.is_some() {
            let top = ((BAR_HEIGHT + LIST_GAP) * self.scale).round() as i32;
            let bottom = ((BAR_HEIGHT + LIST_GAP + list_height()) * self.scale).round() as i32;
            for (x, w) in self.list_bounds() {
                bands.push((
                    (x * self.scale).round() as i32,
                    top,
                    (w * self.scale).round() as i32,
                    bottom - top,
                ));
            }
        }
        win_util::set_window_region(app::RECORD_BAR_TITLE, &bands);
    }

    /// The left edge and width of the open list, in logical pixels.
    fn list_bounds(&self) -> Vec<(f32, f32)> {
        match self.open_menu {
            None => Vec::new(),
            Some(Menu::Format) => vec![(12.0, FORMAT_SELECT)],
            Some(Menu::Rate) => vec![(12.0 + FORMAT_SELECT + 10.0, RATE_SELECT)],
        }
    }

    /// Chooses the container. Only before recording starts.
    fn choose_format(&mut self, format: Format, cx: &mut Context<Self>) {
        self.open_menu = None;
        self.fit();
        if self.phase == Phase::Ready && self.format != format {
            self.format = format;
            self.remember(cx);
        }
        cx.notify();
    }

    /// Chooses the frame rate. Only before recording starts.
    fn choose_fps(&mut self, fps: u32, cx: &mut Context<Self>) {
        self.open_menu = None;
        self.fit();
        if self.phase == Phase::Ready && self.fps != fps {
            self.fps = fps;
            self.remember(cx);
        }
        cx.notify();
    }

    /// Opens one dropdown, or closes it if it was already open.
    fn toggle_menu(&mut self, menu: Menu, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready {
            return;
        }
        self.open_menu = (self.open_menu != Some(menu)).then_some(menu);
        self.fit();
        cx.notify();
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.open_menu.take().is_some() {
            self.fit();
            cx.notify();
        }
    }

    /// Stores the pair, so the next recording opens the way this one was set up.
    fn remember(&self, cx: &mut Context<Self>) {
        let (format, fps) = (self.format, self.fps);
        app::update_config(cx, |config| {
            config.record_format = format;
            config.record_fps = fps;
        });
    }

    fn start(&mut self, cx: &mut Context<Self>) {
        if self.phase != Phase::Ready {
            return;
        }
        self.remember(cx);
        self.open_menu = None;
        if crate::capture::begin_recording(cx, self.format, self.fps) {
            self.phase = Phase::Recording;
            self.fit();
            cx.notify();
        }
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        crate::capture::stop();
        cx.notify();
    }

    /// Gives up: the frame and the toolbar go, the overlay comes back.
    fn cancel(&mut self, cx: &mut Context<Self>) {
        crate::capture::cancel_recording(cx);
    }

    // -- pieces -----------------------------------------------------------

    /// A dropdown: the current value, and the list it opens.
    fn select(
        &self,
        id: &'static str,
        label: String,
        width: f32,
        menu: Menu,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = self.theme;
        let open = self.open_menu == Some(menu);
        let mut head = div()
            .id(id)
            .flex()
            .items_center()
            .justify_between()
            .h(px(CONTROL_HEIGHT))
            .w(px(width))
            .px(px(10.0))
            .flex_shrink_0()
            .rounded(px(RADIUS_CONTROL))
            .bg(if open { theme.card_hover } else { theme.subtle_pressed })
            .border_1()
            .border_color(if open { theme.accent } else { theme.stroke });
        if enabled {
            head = head
                .cursor_pointer()
                .hover(move |s| s.bg(theme.card_hover))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |bar, _event, _window, cx| bar.toggle_menu(menu, cx)),
                );
        }
        head.child(
            div()
                .text_size(px(TEXT_CAPTION))
                .text_color(if enabled { theme.text } else { theme.text_disabled })
                .child(label),
        )
        .child(
            div()
                .text_size(px(TEXT_CAPTION))
                .text_color(if enabled {
                    theme.text_secondary
                } else {
                    theme.text_disabled
                })
                .child("⌄"),
        )
        .into_any_element()
    }

    /// The list hanging under an open dropdown.
    fn select_list(&self, menu: Menu, options: Vec<(String, bool)>, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        let (x, width) = self.list_bounds().into_iter().next().unwrap_or((0.0, 0.0));
        let items: Vec<AnyElement> = options
            .into_iter()
            .map(|(label, active)| {
                let picked = label.clone();
                div()
                    .id(SharedString::from(format!("{label}-{}", active as u8)))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(ITEM_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(RADIUS_CONTROL))
                    .cursor_pointer()
                    .bg(if active {
                        theme.selection
                    } else {
                        rgba(0x0000_0000)
                    })
                    .hover(move |s| s.bg(theme.card_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |bar, _event, _window, cx| {
                            match menu {
                                Menu::Format => {
                                    if let Some(format) =
                                        Format::ALL.into_iter().find(|f| f.label() == picked)
                                    {
                                        bar.choose_format(format, cx);
                                    } else {
                                        bar.close_menu(cx);
                                    }
                                }
                                Menu::Rate => match picked.parse::<u32>() {
                                    Ok(fps) => bar.choose_fps(fps, cx),
                                    Err(_) => bar.close_menu(cx),
                                },
                            }
                        }),
                    )
                    .child(
                        div()
                            .text_size(px(TEXT_CAPTION))
                            .text_color(if active { theme.accent } else { theme.text })
                            .child(label),
                    )
                    .into_any_element()
            })
            .collect();

        div()
            .absolute()
            .left(px(x))
            .top(px(BAR_HEIGHT + LIST_GAP))
            .w(px(width))
            .p(px(LIST_PADDING))
            .flex()
            .flex_col()
            .rounded(px(RADIUS_CARD))
            .bg(theme.card)
            .border_1()
            .border_color(theme.stroke_strong)
            .children(items)
            .into_any_element()
    }

    /// The running clock, shown only while frames are being written.
    fn clock(&self) -> AnyElement {
        let theme = self.theme;
        let (elapsed, frames) = crate::capture::progress().unwrap_or_default();
        let seconds = elapsed.as_secs();
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                // The dot pulses rather than merely sitting there: a still
                // window over a still desktop otherwise gives no sign that
                // anything is being written.
                div()
                    .w(px(8.0))
                    .h(px(8.0))
                    .rounded_full()
                    .bg(theme.danger)
                    .opacity(if (elapsed.as_millis() / 500) % 2 == 0 {
                        1.0
                    } else {
                        0.25
                    }),
            )
            .child(
                div()
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text)
                    .child(format!("{:02}:{:02}", seconds / 60, seconds % 60)),
            )
            .child(
                div()
                    .text_size(px(TEXT_CAPTION))
                    .text_color(theme.text_secondary)
                    .child(format!("{frames} 帧")),
            )
            .into_any_element()
    }

    /// The way out of a recording that has not started.
    fn close_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = self.theme;
        div()
            .id("xshot-record-cancel")
            .flex()
            .items_center()
            .justify_center()
            .h(px(CONTROL_HEIGHT))
            .w(px(CLOSE_WIDTH))
            .flex_shrink_0()
            .rounded(px(RADIUS_CONTROL))
            .cursor_pointer()
            .hover(move |s| s.bg(theme.card_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|bar, _event, _window, cx| bar.cancel(cx)),
            )
            .child(
                div()
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text_secondary)
                    .child("\u{2715}"),
            )
            .into_any_element()
    }

    fn action_button(
        &self,
        id: &'static str,
        label: &'static str,
        background: Rgba,
        hover: Rgba,
        text: Rgba,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .justify_center()
            .h(px(CONTROL_HEIGHT))
            .w(px(ACTION_WIDTH))
            .flex_shrink_0()
            .rounded(px(RADIUS_CONTROL))
            .cursor_pointer()
            .bg(background)
            .hover(move |s| s.bg(hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |bar, _event, _window, cx| on_click(bar, cx)),
            )
            .child(div().text_size(px(TEXT_BODY)).text_color(text).child(label))
            .into_any_element()
    }
}

/// How tall the inside of an open list is.
fn list_height() -> f32 {
    LIST_PADDING * 2.0 + ITEM_HEIGHT * 3.0
}

impl Render for RecordBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;
        let ready = self.phase == Phase::Ready;

        let format_select = self.select(
            "xshot-record-format",
            self.format.label().to_string(),
            FORMAT_SELECT,
            Menu::Format,
            ready,
            cx,
        );
        let rate_select = self.select(
            "xshot-record-rate",
            format!("{} fps", self.fps),
            RATE_SELECT,
            Menu::Rate,
            ready,
            cx,
        );

        let action = if ready {
            self.action_button(
                "xshot-record-start",
                "● 开始录制",
                theme.danger,
                theme.danger_hover,
                theme.on_accent,
                |bar, cx| bar.start(cx),
                cx,
            )
        } else {
            self.action_button(
                "xshot-record-stop",
                "■ 停止",
                theme.card_hover,
                theme.subtle_hover,
                theme.text,
                |bar, cx| bar.stop(cx),
                cx,
            )
        };

        let list = self.open_menu.map(|menu| {
            let options: Vec<(String, bool)> = match menu {
                Menu::Format => Format::ALL
                    .into_iter()
                    .map(|format| (format.label().to_string(), format == self.format))
                    .collect(),
                Menu::Rate => RATES
                    .into_iter()
                    .map(|fps| (fps.to_string(), fps == self.fps))
                    .collect(),
            };
            self.select_list(menu, options, cx)
        });

        let row = div()
            .id("xshot-record-row")
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(BAR_HEIGHT))
            .w_full()
            .px(px(12.0))
            .rounded(px(RADIUS_CARD))
            .bg(theme.card)
            .border_1()
            .border_color(theme.stroke_strong)
            .child(format_select)
            .child(rate_select)
            .child(action)
            // The clock keeps its own stretch of the row whether or not it is
            // showing anything, so nothing shifts when a recording starts.
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .children((!ready).then(|| self.clock())),
            )
            // A way out before anything is written. Once frames are going to a
            // file the only sensible button is stop, so this one is gone.
            .children(ready.then(|| self.close_button(cx)));

        div()
            .id("xshot-record-bar-root")
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .child(row)
            // Everything below is the open dropdown: a full-width backdrop that
            // closes it, then the list itself on top of that backdrop.
            .children(self.open_menu.is_some().then(|| {
                div()
                    .id("xshot-record-menu-backdrop")
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|bar, _event, _window, cx| bar.close_menu(cx)),
                    )
                    .into_any_element()
            }))
            .children(list)
    }
}

/// Where the toolbar goes: under the region, or above it when the region
/// reaches the bottom of the desktop.
fn place(region: (i32, i32, i32, i32), scale: f32) -> (i32, i32) {
    let (x, y, w, h) = region;
    let screen = win_util::virtual_screen();
    let bar_w = (BAR_WIDTH * scale).round() as i32;
    let bar_h = (BAR_HEIGHT * scale).round() as i32;
    let left = (x + (w - bar_w) / 2).clamp(screen.x, (screen.x + screen.w - bar_w).max(screen.x));
    let below = y + h + GAP;
    let top = if below + bar_h <= screen.y + screen.h {
        below
    } else {
        (y - GAP - bar_h).max(screen.y)
    };
    (left, top)
}

/// Opens the toolbar for a region: framed, clickable, not yet recording.
pub fn open(cx: &mut App, region: (i32, i32, i32, i32)) {
    close(cx);
    let (format, fps) = {
        let config = &app::state(cx).config;
        (config.record_format, normalise_rate(config.record_fps))
    };
    let theme = app::state(cx).config.resolved_theme();
    let scale = app::scale_factor(cx);
    let origin = place(region, scale);

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(BAR_WIDTH), px(BAR_HEIGHT)),
        })),
        titlebar: Some(TitlebarOptions {
            title: Some(app::RECORD_BAR_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        // Topmost and out of the taskbar and Alt+Tab: it has to stay reachable
        // while the user works in whatever is being recorded.
        kind: WindowKind::PopUp,
        focus: false,
        show: true,
        is_resizable: false,
        is_movable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        ..Default::default()
    };

    match cx.open_window(options, move |_window, cx| {
        cx.new(|cx| RecordBar::new(region, format, fps, origin, scale, theme, cx))
    }) {
        Ok(handle) => {
            app::state_mut(cx).record_bar = Some(handle.into());
            win_util::place_client_exact(
                app::RECORD_BAR_TITLE,
                origin.0,
                origin.1,
                (BAR_WIDTH * scale).round() as i32,
                (BAR_HEIGHT * scale).round() as i32,
            );
            // Only the row may take a click; the window is exactly that tall.
            win_util::set_window_region(
                app::RECORD_BAR_TITLE,
                &[(
                    0,
                    0,
                    (BAR_WIDTH * scale).round() as i32,
                    (BAR_HEIGHT * scale).round() as i32,
                )],
            );
            // Never steal the foreground from the application underneath, and
            // never appear in the recording.
            win_util::set_no_activate(app::RECORD_BAR_TITLE, true);
            win_util::set_capture_excluded(app::RECORD_BAR_TITLE, true);
        }
        Err(error) => eprintln!("xshot: could not open the recording toolbar: {error:#}"),
    }
}

/// Moves the toolbar, so it stays with a frame that is being dragged around.
pub fn reposition(cx: &mut App, region: (i32, i32, i32, i32)) {
    let Some(handle) = app::state(cx).record_bar.and_then(|handle| handle.downcast::<RecordBar>())
    else {
        return;
    };
    let scale = app::scale_factor(cx);
    let (x, y) = place(region, scale);
    let _ = handle.update(cx, |bar, _window, cx| {
        bar.region = region;
        bar.origin = (x, y);
        bar.fit();
        cx.notify();
    });
}

/// Closes the toolbar if it is open.
pub fn close(cx: &mut App) {
    if let Some(handle) = app::state_mut(cx).record_bar.take() {
        // Hide through Win32 first: `remove_window` only sets a flag that a
        // later frame drains, and a toolbar left on screen for even one frame
        // after a recording ends would land in the next screenshot. Hiding also
        // hands back the mouse capture, which this window took when its button
        // was pressed.
        win_util::hide_window(app::RECORD_BAR_TITLE);
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

/// A frame rate out of the config could in principle be anything, since the
/// file is editable by hand.
fn normalise_rate(fps: u32) -> u32 {
    if RATES.contains(&fps) { fps } else { DEFAULT_RATE }
}
