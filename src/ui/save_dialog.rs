//! The "where should this go?" window for `保存`.
//!
//! The shot is already finished when this opens, so the window is there to
//! answer one question: which folder. The image sits on the left so the user can
//! confirm what they are saving, and the folder on the right starts at the
//! desktop, which is where a screenshot is easiest to find again.
//!
//! Windows' own save dialog cannot be used: it has no room for a preview, and
//! the preview is the point — the annotations are the whole reason to look
//! before saving.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::*;

use crate::app;
use crate::config;
use crate::ui::theme::{CONTROL_HEIGHT, RADIUS_CARD, RADIUS_CONTROL, TEXT_BODY, TEXT_CAPTION, Theme};
use crate::win::frame::Dib;
use crate::win::util as win_util;

/// One row in the shortcut list.
struct Place {
    label: &'static str,
    path: PathBuf,
}

/// The folders offered as one-click destinations.
fn places() -> Vec<Place> {
    let home = std::env::var_os("USERPROFILE").map(PathBuf::from);
    let mut out = Vec::new();
    if let Some(home) = home {
        out.push(Place { label: "桌面", path: home.join("Desktop") });
        out.push(Place { label: "图片", path: home.join("Pictures") });
        out.push(Place { label: "文档", path: home.join("Documents") });
        out.push(Place { label: "下载", path: home.join("Downloads") });
    }
    out.push(Place { label: "临时目录", path: std::env::temp_dir() });
    out.retain(|place| place.path.is_dir());
    out
}

pub struct SaveDialog {
    /// What gets written.
    pixels: Arc<Dib>,
    /// What is shown, already scaled down to the preview box.
    preview: Arc<RenderImage>,
    folder: PathBuf,
    name: String,
    /// Set when a write failed, so the window can say so instead of vanishing.
    error: Option<String>,
    /// Cursor and window origin while the title bar is being dragged.
    dragging: Option<((i32, i32), (i32, i32))>,
    focus: FocusHandle,
    theme: Theme,
}

impl SaveDialog {
    pub fn new(pixels: Dib, folder: PathBuf, theme: Theme, cx: &mut Context<Self>) -> Self {
        let name = format!("xshot_{}.png", chrono::Local::now().format("%Y%m%d_%H%M%S"));
        let preview = preview_image(&pixels);
        Self {
            pixels: Arc::new(pixels),
            preview,
            folder,
            name,
            error: None,
            dragging: None,
            focus: cx.focus_handle(),
            theme,
        }
    }

    /// The toolkit's drag gesture does nothing for a window with no native
    /// frame, so the title bar moves the window itself.
    fn drag_window(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        let start_cursor = win_util::cursor_position();
        let start_origin = win_util::client_origin(app::SAVE_TITLE).unwrap_or(start_cursor);
        self.dragging = Some((start_cursor, start_origin));
        cx.notify();
    }

    fn drag_move(&mut self, cx: &mut Context<Self>) {
        let Some((start_cursor, start_origin)) = self.dragging else {
            return;
        };
        let cursor = win_util::cursor_position();
        win_util::move_window_to(
            app::SAVE_TITLE,
            start_origin.0 + (cursor.0 - start_cursor.0),
            start_origin.1 + (cursor.1 - start_cursor.1),
        );
        cx.notify();
    }

    fn end_drag(&mut self, cx: &mut Context<Self>) {
        if self.dragging.take().is_some() {
            cx.notify();
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Writes the file and closes the whole capture session.
    fn save(&mut self, cx: &mut Context<Self>) {
        let target = self.folder.join(&self.name);
        if let Err(error) = self.pixels.save_png(&target) {
            self.error = Some(format!("保存失败：{error}"));
            cx.notify();
            return;
        }
        // Remember where it went, so the next save starts there.
        let folder = self.folder.clone();
        app::update_config(cx, |config| config.save_dir = folder);
        app::close_capture(cx);
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        app::close_save_dialog(cx);
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        if let Some(folder) = win_util::pick_folder(&self.folder) {
            self.folder = folder;
            cx.notify();
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => self.cancel(cx),
            "enter" => self.save(cx),
            _ => {}
        }
    }
}

impl Render for SaveDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        let shortcuts: Vec<AnyElement> = places()
            .into_iter()
            .map(|place| {
                let active = place.path == self.folder;
                let path = place.path.clone();
                div()
                    .id(place.label)
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(CONTROL_HEIGHT))
                    .px(px(10.0))
                    .rounded(px(RADIUS_CONTROL))
                    .cursor_pointer()
                    .bg(if active { theme.card_hover } else { theme.subtle_pressed })
                    .border_1()
                    .border_color(if active { theme.accent } else { theme.stroke })
                    .hover(|style| style.bg(theme.card_hover))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |dialog, _event, _window, cx| {
                            dialog.folder = path.clone();
                            dialog.error = None;
                            cx.notify();
                        }),
                    )
                    .child(div().text_size(px(TEXT_BODY)).text_color(theme.text).child(place.label))
                    .into_any_element()
            })
            .collect();

        div()
            .id("xshot-save-root")
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme.window)
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_move(cx.listener(|dialog, _event, _window, cx| dialog.drag_move(cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|dialog, _event, _window, cx| dialog.end_drag(cx)),
            )
            .child(
                // Title bar, dragged by hand: these windows have no system frame.
                div()
                    .id("xshot-save-titlebar")
                    .flex()
                    .items_center()
                    .h(px(38.0))
                    .px(px(14.0))
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text)
                    .child("保存截图")
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|dialog, event, _window, cx| dialog.drag_window(event, cx)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(14.0))
                    .flex_1()
                    .px(px(14.0))
                    .pb(px(14.0))
                    .min_h(px(0.0))
                    // Left: what is being saved.
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_center()
                            .flex_1()
                            .min_w(px(0.0))
                            .p(px(8.0))
                            .rounded(px(RADIUS_CARD))
                            .bg(theme.card)
                            .border_1()
                            .border_color(theme.stroke)
                            .child(
                                img(self.preview.clone())
                                    .max_w_full()
                                    .max_h_full()
                                    .object_fit(ObjectFit::Contain),
                            ),
                    )
                    // Right: where it goes.
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .w(px(300.0))
                            .child(
                                div()
                                    .text_size(px(TEXT_CAPTION))
                                    .text_color(theme.text_secondary)
                                    .child("保存位置"),
                            )
                            .children(shortcuts)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .p(px(8.0))
                                    .rounded(px(RADIUS_CONTROL))
                                    .bg(theme.subtle_pressed)
                                    .border_1()
                                    .border_color(theme.stroke)
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w(px(0.0))
                                            .overflow_hidden()
                                            .text_size(px(TEXT_CAPTION))
                                            .text_color(theme.text)
                                            .child(self.folder.display().to_string()),
                                    )
                                    .child(
                                        div()
                                            .id("xshot-save-browse")
                                            .px(px(8.0))
                                            .h(px(24.0))
                                            .flex()
                                            .items_center()
                                            .rounded(px(RADIUS_CONTROL))
                                            .cursor_pointer()
                                            .bg(theme.card_hover)
                                            .text_size(px(TEXT_CAPTION))
                                            .text_color(theme.text)
                                            .hover(|style| style.bg(theme.card_pressed))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|dialog, _event, _window, cx| {
                                                    dialog.browse(cx)
                                                }),
                                            )
                                            .child("浏览…"),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(TEXT_CAPTION))
                                    .text_color(theme.text_secondary)
                                    .child(format!("文件名：{}", self.name)),
                            )
                            .child(
                                div()
                                    .text_size(px(TEXT_CAPTION))
                                    .text_color(theme.text_secondary)
                                    .child("Enter 保存 · Esc 取消"),
                            )
                            .children(self.error.clone().map(|message| {
                                div()
                                    .text_size(px(TEXT_CAPTION))
                                    .text_color(theme.danger)
                                    .child(message)
                            }))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .id("xshot-save-cancel")
                                            .flex_1()
                                            .h(px(CONTROL_HEIGHT))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(RADIUS_CONTROL))
                                            .cursor_pointer()
                                            .bg(theme.subtle_pressed)
                                            .border_1()
                                            .border_color(theme.stroke)
                                            .text_color(theme.text)
                                            .hover(|style| style.bg(theme.card_hover))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|dialog, _event, _window, cx| {
                                                    dialog.cancel(cx)
                                                }),
                                            )
                                            .child("取消"),
                                    )
                                    .child(
                                        div()
                                            .id("xshot-save-confirm")
                                            .flex_1()
                                            .h(px(CONTROL_HEIGHT))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(px(RADIUS_CONTROL))
                                            .cursor_pointer()
                                            .bg(theme.accent)
                                            .text_color(theme.on_accent)
                                            .hover(|style| style.bg(theme.accent_hover))
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|dialog, _event, _window, cx| {
                                                    dialog.save(cx)
                                                }),
                                            )
                                            .child("保存"),
                                    ),
                            ),
                    ),
            )
    }
}

/// Scales the shot down to something that fits a preview box.
///
/// `Dib` holds BGRA, which is exactly what `RenderImage` wants, so this is a
/// copy either way; doing it once here keeps the render path free of pixel work.
fn preview_image(pixels: &Dib) -> Arc<RenderImage> {
    const BOX: f32 = 520.0;
    let (width, height) = (pixels.width as f32, pixels.height as f32);
    let scale = (BOX / width).min(BOX / height).min(1.0);
    let target_w = (width * scale).round().max(1.0) as u32;
    let target_h = (height * scale).round().max(1.0) as u32;

    let source = pixels.pixels();
    let stride = pixels.width as usize * 4;
    let mut buffer = image::RgbaImage::new(target_w, target_h);
    for y in 0..target_h {
        let source_y = ((y as f32 / scale) as usize).min(pixels.height as usize - 1);
        for x in 0..target_w {
            let source_x = ((x as f32 / scale) as usize).min(pixels.width as usize - 1);
            let index = source_y * stride + source_x * 4;
            buffer.put_pixel(
                x,
                y,
                image::Rgba([source[index], source[index + 1], source[index + 2], 0xFF]),
            );
        }
    }
    Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]))
}

/// Opens the save window over the finished shot.
pub fn open(cx: &mut App, pixels: Dib) {
    close(cx);
    let folder = {
        let configured = app::state(cx).config.save_dir.clone();
        if configured.is_dir() {
            configured
        } else {
            config::default_save_dir()
        }
    };
    let theme = app::state(cx).config.resolved_theme();
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(880.0), px(560.0)),
        })),
        titlebar: Some(TitlebarOptions {
            title: Some(app::SAVE_TITLE.into()),
            appears_transparent: true,
            traffic_light_position: None,
        }),
        kind: WindowKind::PopUp,
        focus: true,
        show: true,
        is_resizable: false,
        is_movable: true,
        is_minimizable: false,
        window_background: theme.backdrop(),
        ..Default::default()
    };
    match cx.open_window(options, move |window, cx| {
        let view = cx.new(|cx| SaveDialog::new(pixels, folder, theme, cx));
        window.focus(&view.read(cx).focus_handle(), cx);
        view
    }) {
        Ok(handle) => {
            // Centre it on the screen the user is working on.
            let screen = win_util::virtual_screen();
            let x = screen.x + (screen.w - 880) / 2;
            let y = screen.y + (screen.h - 560) / 2;
            win_util::place_client_exact(app::SAVE_TITLE, x, y, 880, 560);
            if let Some(hwnd) = win_util::find_window(app::SAVE_TITLE) {
                win_util::force_foreground(hwnd);
            }
            app::state_mut(cx).save_dialog = Some(handle.into());
        }
        Err(error) => eprintln!("xshot: could not open the save window: {error:#}"),
    }
}

/// Closes the save window if it is open.
pub fn close(cx: &mut App) {
    if let Some(handle) = app::state_mut(cx).save_dialog.take() {
        // `remove_window` only sets a flag that a later frame drains, and after
        // a save the window has to be gone *now* — a dialog that lingers even
        // for a frame reads as "the save did not work".
        win_util::hide_window(app::SAVE_TITLE);
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}
