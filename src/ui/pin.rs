//! A pinned shot: a borderless, always-on-top window holding a finished image.
//!
//! A pin is where the extracted text lives. The frame is handed to the system
//! OCR engine as soon as the pin appears, and once the lines come back they
//! become selectable in place — the pointer turns into an I-beam over any
//! recognised line, clicking a word copies it, and Shift-dragging copies a run.
//!
//! Dragging anywhere else moves the window. The toolkit's own window-drag
//! gesture does nothing for a borderless tool window on Windows, so the move is
//! done here, by hand, from the cursor's screen position.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::*;

use crate::annot::Pt;
use crate::app;
use crate::ui::pin_menu::{MenuChoice, PinMenu};
use crate::ui::theme::{RADIUS_CARD, TEXT_BODY, TEXT_CAPTION, Theme};
use crate::win::frame::Dib;
use crate::win::ocr::{self, OcrLine, OcrWord};
use crate::win::{clip, util as win_util};

const TOAST_LIFETIME: Duration = Duration::from_millis(1800);
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 8.0;
const ZOOM_PER_LINE: f32 = 1.03;
const PIXELS_PER_LINE: f32 = 20.0;
/// Window title of the context-menu popup, used to find its HWND.
const MENU_TITLE: &str = "xshot-pin-menu";
/// A press that travels less than this counts as a click, not a drag.
const CLICK_SLOP: f32 = 4.0;
/// How far off a glyph the pointer may be and still count as "on text".
/// Text is selected by dragging on it, so this doubles as the decision of
/// whether a drag selects or moves the window — it has to be tight.
const HOVER_SLACK: f32 = 3.0;

enum OcrState {
    Running,
    Ready,
    Unavailable,
}

/// A window move in progress.
struct PinDrag {
    cursor: (i32, i32),
    origin: (i32, i32),
    travelled: f32,
}

pub struct Pin {
    pixels: Arc<Dib>,
    image: Arc<RenderImage>,
    title: String,
    zoom: f32,
    scale: f32,
    shadow: bool,
    theme: Theme,
    focus: FocusHandle,

    lines: Vec<OcrLine>,
    ocr_state: OcrState,

    drag: Option<PinDrag>,
    /// Rubber band in image device pixels, while text is being selected.
    band: Option<(Pt, Pt)>,
    /// Set when a double-click selected a word, so the mouse-up that follows
    /// does not treat the fresh highlight as a click and drop it again.
    keep_band: bool,
    /// Pointer is over recognised text, so the cursor becomes an I-beam.
    over_text: bool,
    toast: Option<(String, Instant)>,

    menu: Option<AnyWindowHandle>,
    menu_entity: Option<Entity<PinMenu>>,
    /// Set when the menu asked for the pin to go away; applied on the next
    /// frame, because a window can only be removed while it is being drawn.
    close_requested: bool,
}

impl Pin {
    pub fn new(
        pixels: Arc<Dib>,
        zoom: f32,
        title: String,
        shadow: bool,
        theme: Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let image = pixels.to_render_image();
        let scale = window.scale_factor().max(0.1);

        // Recognising a large image is not instant, so it happens off the UI
        // thread and the pin simply gains selectable text when it is done.
        let (width, height) = (pixels.width, pixels.height);
        let source = Arc::clone(&pixels);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ocr::recognize(source.pixels().to_vec(), width, height) })
                .await;
            let _ = this.update(cx, |pin, cx| {
                match result {
                    Ok(lines) => {
                        crate::trace_log!("xshot: OCR found {} line(s)", lines.len());
                        pin.lines = lines;
                        pin.ocr_state = OcrState::Ready;
                    }
                    Err(error) => {
                        crate::trace_log!("xshot: OCR unavailable: {error:#}");
                        pin.ocr_state = OcrState::Unavailable;
                    }
                }
                cx.notify();
            });
        })
        .detach();

        Self {
            pixels,
            image,
            title,
            zoom,
            scale,
            shadow,
            theme,
            focus: cx.focus_handle(),
            lines: Vec::new(),
            ocr_state: OcrState::Running,
            drag: None,
            band: None,
            keep_band: false,
            over_text: false,
            toast: None,
            menu: None,
            menu_entity: None,
            close_requested: false,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    fn image_scale(&self) -> f32 {
        self.scale / self.zoom.max(0.01)
    }

    fn to_image(&self, position: Point<Pixels>) -> Pt {
        let factor = self.image_scale();
        Pt::new(position.x.as_f32() * factor, position.y.as_f32() * factor)
    }

    fn word_at(&self, point: Pt) -> Option<&OcrWord> {
        self.lines
            .iter()
            .find(|line| line.contains(point.x, point.y))
            .and_then(|line| line.words.iter().find(|word| word.contains(point.x, point.y)))
    }

    /// The word nearest to `point`, allowing a few pixels of grace.
    ///
    /// Both the I-beam and the "does a drag select or move the window?"
    /// decision use this. Judging by the whole *line* box instead was a
    /// mistake: a recognised line spans from its first word to its last, so a
    /// drag started in the empty space beside a line was treated as text and
    /// the window refused to move.
    fn word_near(&self, point: Pt) -> Option<&OcrWord> {
        self.lines.iter().flat_map(|line| line.words.iter()).find(|word| {
            point.x >= word.x - HOVER_SLACK
                && point.x <= word.right() + HOVER_SLACK
                && point.y >= word.y - HOVER_SLACK
                && point.y <= word.bottom() + HOVER_SLACK
        })
    }

    fn apply_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = zoom;
        if let Some(spec) = app::state_mut(cx).last_pin.as_mut() {
            spec.zoom = zoom;
        }

        let origin = win_util::client_origin(&self.title).unwrap_or((0, 0));
        win_util::place_client_exact(
            &self.title,
            origin.0,
            origin.1,
            (self.pixels.width as f32 * zoom).round() as i32,
            (self.pixels.height as f32 * zoom).round() as i32,
        );
        cx.notify();
    }

    /// The text covered by the current rubber band.
    ///
    /// Selection is per line and fills the gaps between words, so a band that
    /// lands between two words still copies the phrase rather than dropping the
    /// word in the middle.
    fn selected_text(&self) -> String {
        let Some((from, to)) = self.band else {
            return String::new();
        };
        let (x0, x1) = (from.x.min(to.x), from.x.max(to.x));
        let (y0, y1) = (from.y.min(to.y), from.y.max(to.y));
        let mut parts = Vec::new();
        for line in &self.lines {
            if !line.overlaps_rows(y0, y1) {
                continue;
            }
            let words = line.words_in(x0, x1);
            if !words.is_empty() {
                parts.push(line.join(&words));
            }
        }
        parts.join("\n")
    }

    fn copy_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
        self.toast = Some((
            format!("已复制 {} 个字", text.chars().count()),
            Instant::now(),
        ));
        cx.notify();
    }

    fn copy_selection(&mut self, cx: &mut Context<Self>) -> bool {
        let text = self.selected_text();
        if text.is_empty() {
            return false;
        }
        self.copy_text(&text, cx);
        true
    }

    fn copy_image(&mut self, cx: &mut Context<Self>) {
        match clip::copy_image(&self.pixels) {
            Ok(()) => self.toast = Some(("已复制图像".into(), Instant::now())),
            Err(error) => {
                eprintln!("xshot: clipboard write failed: {error:#}");
                self.toast = Some(("复制失败".into(), Instant::now()));
            }
        }
        cx.notify();
    }

    fn save_image(&mut self, cx: &mut Context<Self>) {
        let directory = app::state(cx).config.save_dir.clone();
        let name = format!("xshot_{}.png", chrono::Local::now().format("%Y%m%d%H%M%S"));
        match self.pixels.save_png(&directory.join(name)) {
            Ok(()) => self.toast = Some((format!("已保存到 {}", directory.display()), Instant::now())),
            Err(error) => {
                eprintln!("xshot: save failed: {error:#}");
                self.toast = Some(("保存失败".into(), Instant::now()));
            }
        }
        cx.notify();
    }

    fn toggle_shadow(&mut self, cx: &mut Context<Self>) {
        self.shadow = !self.shadow;
        let shadow = self.shadow;
        app::update_config(cx, |config| config.pin_shadow = shadow);
        win_util::set_window_shadow(&self.title, shadow);
        self.toast = Some((
            if shadow { "已打开窗口阴影" } else { "已取消窗口阴影" }.into(),
            Instant::now(),
        ));
        cx.notify();
    }

    // -- context menu ------------------------------------------------------

    fn open_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let theme = self.theme;
        let shadow = self.shadow;
        let has_text = !self.lines.is_empty();

        let menu = cx.new(|cx| PinMenu::new(shadow, has_text, theme, cx));
        let (menu_width, menu_height) = menu.read(cx).size();

        // Anchor at the pointer, but keep the whole menu on screen even when
        // the pin itself sits against an edge. `position` is local to the pin,
        // so the window's own origin has to be added before clamping.
        let screen = win_util::virtual_screen();
        let scale = self.scale.max(0.1);
        let (client_x, client_y) = win_util::client_origin(&self.title).unwrap_or((0, 0));
        let anchor_x = (client_x as f32 + position.x.as_f32() * scale) / scale;
        let anchor_y = (client_y as f32 + position.y.as_f32() * scale) / scale;
        let screen_w = screen.w as f32 / scale;
        let screen_h = screen.h as f32 / scale;
        let origin_x = anchor_x.min(screen_w - menu_width).max(0.0);
        let origin_y = anchor_y.min(screen_h - menu_height).max(0.0);

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: point(px(origin_x), px(origin_y)),
                size: size(px(menu_width), px(menu_height)),
            })),
            titlebar: Some(TitlebarOptions {
                title: Some(MENU_TITLE.into()),
                appears_transparent: true,
                traffic_light_position: None,
            }),
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            is_resizable: false,
            is_movable: false,
            // The menu is a rounded card, so its window has to be transparent
            // for the same reason a pin's does: an opaque one fills the corners
            // with its clear colour.
            window_background: WindowBackgroundAppearance::Transparent,
            ..Default::default()
        };

        let root = menu.clone();
        match cx.open_window(options, move |window, cx| {
            window.focus(&root.read(cx).focus_handle(), cx);
            root
        }) {
            Ok(handle) => {
                self.menu = Some(handle.into());
                self.menu_entity = Some(menu);
                self.watch_menu(cx);
            }
            Err(error) => eprintln!("xshot: could not open the pin menu: {error:#}"),
        }
    }

    fn close_menu(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.menu.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
        self.menu_entity = None;
    }

    /// Follows the menu until it is used or dismissed.
    ///
    /// A native menu closes when it loses activation; GPUI has no hook for that
    /// on the window we just opened, so the foreground window is polled
    /// instead. It is a handful of checks per second while a menu is open.
    fn watch_menu(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(70))
                    .await;
                let finished = this
                    .update(cx, |pin, cx| pin.poll_menu(cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Returns `true` once the menu is gone.
    fn poll_menu(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(menu) = self.menu_entity.clone() else {
            return true;
        };
        let choice = menu.read(cx).take_choice();
        let dismissed = menu.read(cx).was_dismissed();

        if let Some(choice) = choice {
            self.close_menu(cx);
            match choice {
                MenuChoice::CopyImage => self.copy_image(cx),
                MenuChoice::CopyText => {
                    let text: Vec<String> = self.lines.iter().map(|line| line.text.clone()).collect();
                    let joined = text.join("\n");
                    if joined.is_empty() {
                        self.toast = Some(("没有识别到文字".into(), Instant::now()));
                        cx.notify();
                    } else {
                        self.copy_text(&joined, cx);
                    }
                }
                MenuChoice::ToggleShadow => self.toggle_shadow(cx),
                MenuChoice::Save => self.save_image(cx),
                MenuChoice::Close => {
                    self.close_requested = true;
                    cx.notify();
                }
            }
            return true;
        }

        // Focus moved elsewhere: treat it as a dismissal, like a native menu.
        let menu_hwnd = win_util::find_window(MENU_TITLE);
        let foreground = win_util::foreground_window();
        let lost_focus = match (menu_hwnd, foreground) {
            (Some(menu_hwnd), Some(foreground)) => menu_hwnd != foreground,
            _ => true,
        };
        if dismissed || lost_focus {
            self.close_menu(cx);
            return true;
        }
        false
    }

    // -- input -------------------------------------------------------------

    fn on_mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.is_some() {
            self.close_menu(cx);
            return;
        }

        match event.button {
            MouseButton::Right => self.open_menu(event.position, cx),
            MouseButton::Left => {
                let point = self.to_image(event.position);
                if event.click_count >= 2 {
                    // On text a double-click means "select this word", like every
                    // other text surface — closing the pin would throw away the
                    // thing the user was reading. Anywhere else it keeps the
                    // QQ-style "double-click copies and closes".
                    if let Some(word) = self.word_near(point) {
                        let box_ = (
                            Pt::new(word.x, word.y),
                            Pt::new(word.right(), word.bottom()),
                        );
                        self.band = Some(box_);
                        self.keep_band = true;
                        self.toast = Some(("已选中该词".into(), Instant::now()));
                        cx.notify();
                        return;
                    }
                    self.copy_image(cx);
                    window.remove_window();
                    return;
                }
                // The previous selection stays highlighted for a moment, but a
                // new press always starts from scratch.
                self.band = None;
                if self.word_near(point).is_some() {
                    // On text: dragging selects it, exactly like any other text
                    // surface. A plain click picks the single word under the
                    // pointer.
                    self.band = Some((point, point));
                    cx.notify();
                    return;
                }
                // Anywhere else the pin behaves like a window: drag it around.
                let cursor = win_util::cursor_position();
                let origin = win_util::client_origin(&self.title).unwrap_or(cursor);
                self.drag = Some(PinDrag {
                    cursor,
                    origin,
                    travelled: 0.0,
                });
                cx.notify();
            }
            _ => {}
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let point = self.to_image(event.position);
        // A drag only continues while the button is actually held. Without this
        // the selection kept chasing the pointer after the button came up,
        // because the band was left in place so the highlight could stay
        // visible.
        let held = event.pressed_button == Some(MouseButton::Left);

        if held && self.band.is_some() {
            if let Some((from, _)) = self.band {
                self.band = Some((from, point));
                cx.notify();
            }
            return;
        }

        let drag = self.drag.take();
        if let Some(mut drag) = drag {
            if held {
                let cursor = win_util::cursor_position();
                let dx = cursor.0 - drag.cursor.0;
                let dy = cursor.1 - drag.cursor.1;
                drag.travelled += (dx.abs() + dy.abs()) as f32;
                win_util::move_window_to(&self.title, drag.origin.0 + dx, drag.origin.1 + dy);
                self.drag = Some(drag);
            }
            return;
        }

        // Idle: the pointer only changes shape when it is over real text.
        let over_text = self.word_near(point).is_some();
        if over_text != self.over_text {
            self.over_text = over_text;
            cx.notify();
        }
    }

    fn on_mouse_up(&mut self, _event: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        // The release that follows a double-click belongs to the word that
        // double-click just selected: it must not be read as a click and drop
        // the highlight again.
        if self.keep_band {
            self.keep_band = false;
            return;
        }
        if let Some((from, to)) = self.band {
            let travelled = (to.x - from.x).abs() + (to.y - from.y).abs();
            if travelled < CLICK_SLOP {
                // A click, not a drag: take the single word under the pointer.
                let word = self.word_at(from).map(|word| word.text.clone());                self.band = None;
                match word {
                    Some(text) => self.copy_text(&text, cx),
                    None => cx.notify(),
                }
            } else {
                let text = self.selected_text();
                if text.is_empty() {
                    self.band = None;
                    cx.notify();
                } else {
                    self.copy_text(&text, cx);
                }
            }
            return;
        }

        // The drag moved the window; nothing else to do.
        self.drag = None;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            if self.band.take().is_some() {
                cx.notify();
            } else {
                window.remove_window();
            }
            return;
        }
        if key == "c" && event.keystroke.modifiers.control {
            if !self.copy_selection(cx) {
                self.copy_image(cx);
            }
        }
    }

    // -- rendering ---------------------------------------------------------

    fn highlights(&self, theme: Theme) -> Vec<AnyElement> {
        let factor = 1.0 / self.image_scale();
        let mut elements = Vec::new();

        // Everything selected, word by word.
        let selected: Vec<&OcrWord> = match self.band {
            Some((from, to)) => {
                let (x0, x1) = (from.x.min(to.x), from.x.max(to.x));
                let (y0, y1) = (from.y.min(to.y), from.y.max(to.y));
                self.lines
                    .iter()
                    .filter(|line| line.overlaps_rows(y0, y1))
                    .flat_map(|line| line.words_in(x0, x1))
                    .collect()
            }
            None => Vec::new(),
        };
        for word in selected {
            elements.push(
                div()
                    .absolute()
                    .left(px(word.x * factor))
                    .top(px(word.y * factor))
                    .w(px(word.w * factor))
                    .h(px(word.h * factor))
                    .bg(theme.selection)
                    .into_any_element(),
            );
        }
        elements
    }

    fn toast_badge(&self, theme: Theme) -> Option<AnyElement> {
        let (message, at) = self.toast.as_ref()?;
        if at.elapsed() >= TOAST_LIFETIME {
            return None;
        }
        Some(
            div()
                .absolute()
                .left(px(10.0))
                .top(px(10.0))
                .px(px(10.0))
                .py(px(4.0))
                .rounded(px(RADIUS_CARD))
                .bg(theme.window)
                .border_1()
                .border_color(theme.stroke_strong)
                .text_size(px(TEXT_CAPTION))
                .text_color(theme.text)
                .shadow_md()
                .child(message.clone())
                .into_any_element(),
        )
    }
}

impl Render for Pin {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.scale = window.scale_factor().max(0.1);
        if self.close_requested {
            self.close_requested = false;
            window.remove_window();
        }
        let theme = self.theme;
        let _ = &self.ocr_state;
        // The finished shot is rounded in device pixels at capture time, so the
        // pin draws the same shape rather than a square window around it. Zoom
        // scales the corners with everything else, which is what keeps them
        // matching; `scale` cancels out because the radius is logical here.
        let radius = app::state(cx).config.corner_radius.clamp(0.0, 96.0) * self.zoom;

        let mut root = div()
            .id("xshot-pin")
            .relative()
            .size_full()
            .overflow_hidden()
            // No fill: the window is transparent, and the only part of it that
            // should show through is the corner the shot's alpha cuts away.
            .cursor(if self.over_text {
                CursorStyle::IBeam
            } else {
                CursorStyle::Arrow
            })
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .child(
                img(self.image.clone())
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h_full()
                    .rounded(px(radius))
                    .object_fit(ObjectFit::Fill),
            )
            .children(self.highlights(theme));

        if let Some(badge) = self.toast_badge(theme) {
            root = root.child(badge);
        }

        root = root
            .child(
                // Hover-revealed close button; the context menu has one too.
                div()
                    .id("pin-close")
                    .absolute()
                    .right(px(6.0))
                    .top(px(6.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(22.0))
                    .h(px(22.0))
                    .rounded(px(11.0))
                    .bg(rgba(0x0000_00A6))
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text)
                    .opacity(0.0)
                    .hover(|style| style.opacity(1.0))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, |_event, window, cx| {
                        cx.stop_propagation();
                        window.remove_window();
                    })
                    .child("✕"),
            )
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(
                |this, event: &ScrollWheelEvent, _window, cx| {
                    let lines = match event.delta {
                        ScrollDelta::Lines(point) => point.y,
                        ScrollDelta::Pixels(point) => point.y.as_f32() / PIXELS_PER_LINE,
                    };
                    if lines.abs() < f32::EPSILON {
                        return;
                    }
                    let next = this.zoom * ZOOM_PER_LINE.powf(lines);
                    this.apply_zoom(next, cx);
                },
            ));

        root
    }
}
