//! The settings window, in Fluent Design.
//!
//! A navigation pane on the left and a page of cards on the right, following
//! the Fluent 2 / Windows 11 conventions: Mica behind everything, layered card
//! fills instead of borders, 4 px control radii, 32 px controls, and the Segoe
//! UI Variable text ramp. Every colour comes from [`crate::ui::theme`] so a new
//! setting cannot drift away from the system.

use gpui_kit::*;

// The dropdown is the kit's own widget, not ours and not a raw primitive:
// `gpui_kit::component` is gpui-component, the styled component library that
// ships with the kit, and its `Select` comes with the trigger, the list, the
// caret, the check marks, the keyboard and the accessibility built in.
use gpui_kit::component::{
    IndexPath, Theme as ComponentTheme, ThemeMode,
    select::{Select as ComponentSelect, SelectEvent, SelectState, SearchableVec},
};

use crate::app;
use std::path::PathBuf;

use crate::config::{CloseBehavior, FinishAction, ThemeChoice, TrayClick};
use crate::win::util as win_util;
use crate::ui::icons::{self, glyph};
use crate::ui::theme::{
    CONTROL_HEIGHT, FONT_FAMILY, NAV_ROW_HEIGHT, RADIUS_CARD, RADIUS_CONTROL, TEXT_BODY,
    TEXT_CAPTION, TEXT_TITLE, Theme,
};
use crate::win::hotkey;

/// The pages listed in the navigation pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    General,
    Appearance,
    Screenshot,
    Shortcuts,
    About,
}

impl Page {
    const ALL: [Page; 5] = [
        Page::General,
        Page::Appearance,
        Page::Screenshot,
        Page::Shortcuts,
        Page::About,
    ];

    fn label(self) -> &'static str {
        match self {
            Page::General => "常规设置",
            Page::Appearance => "外观设置",
            Page::Screenshot => "截图设置",
            Page::Shortcuts => "快捷键设置",
            Page::About => "关于",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Page::General => "⚙",
            Page::Appearance => "🎨",
            Page::Screenshot => "✂",
            Page::Shortcuts => "⌨",
            Page::About => "ℹ",
        }
    }
}

/// Which hotkey field is being captured from the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recording {
    None,
    Capture,
    Restore,
}

/// One of the settings that is picked from a list.
///
/// Every choice setting in this window is one of these, so they all share the same
/// dropdown: the labels its list shows, which one is in force, and what applying it
/// means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Select {
    /// What closing the settings window does.
    CloseBehavior,
    /// What a single left click on the tray icon does.
    TrayClick,
    /// Where 保存 starts.
    SaveDir,
    Theme,
    /// What Enter, double-click and 完成 produce.
    FinishAction,
    /// How dark everything outside the selection is.
    Dim,
}

impl Select {
    /// Every dropdown in the window, in the order their focus handles are kept.
    const ALL: [Select; 6] = [
        Select::CloseBehavior,
        Select::TrayClick,
        Select::SaveDir,
        Select::Theme,
        Select::FinishAction,
        Select::Dim,
    ];

    /// How many dropdowns there are.
    const COUNT: usize = Self::ALL.len();

    /// The labels its list shows, and which of them is the value in force.
    ///
    /// `None` means "nothing in the list is in force", which is a real case for
    /// the save folder: the configured one is usually not one of the four
    /// offered, and then the dropdown shows it as a placeholder instead of
    /// pretending one of the four is selected.
    fn options(self, cx: &App) -> (Vec<String>, Option<usize>) {
        let config = &app::state(cx).config;
        match self {
            Self::CloseBehavior => (
                CloseBehavior::ALL.iter().map(|o| o.label().to_string()).collect(),
                CloseBehavior::ALL
                    .iter()
                    .position(|o| *o == config.close_behavior),
            ),
            Self::TrayClick => (
                TrayClick::ALL.iter().map(|o| o.label().to_string()).collect(),
                TrayClick::ALL
                    .iter()
                    .position(|o| *o == config.tray_left_click),
            ),
            Self::Theme => (
                ThemeChoice::ALL.iter().map(|o| o.label().to_string()).collect(),
                ThemeChoice::ALL
                    .iter()
                    .position(|o| *o == config.theme),
            ),
            Self::FinishAction => (
                FinishAction::ALL.iter().map(|o| o.label().to_string()).collect(),
                FinishAction::ALL
                    .iter()
                    .position(|o| *o == config.finish_action),
            ),
            Self::Dim => (
                DIM_LEVELS.iter().map(|(_, label)| (*label).to_string()).collect(),
                DIM_LEVELS
                    .iter()
                    .position(|(value, _)| (value - config.dim_strength).abs() < 0.01),
            ),
            Self::SaveDir => {
                let dirs = save_dir_choices();
                let index = dirs
                    .iter()
                    .position(|(_, path)| *path == config.save_dir);
                let mut labels: Vec<String> = dirs.into_iter().map(|(label, _)| label).collect();
                labels.push("浏览…".into());
                (labels, index)
            }
        }
    }

    /// What the dropdown shows when nothing in its list is in force.
    fn placeholder(self, cx: &App) -> Option<String> {
        let (_, index) = self.options(cx);
        if index.is_some() {
            return None;
        }
        // Only the save folder can be outside its list; name it rather than
        // pretend one of the four is in force.
        (self == Self::SaveDir).then(|| app::state(cx).config.save_dir.display().to_string())
    }

    /// The one-line explanation shown under the dropdown, for the value in force.
    fn hint(self, cx: &App) -> Option<&'static str> {
        let (_, index) = self.options(cx);
        let index = index?;
        match self {
            Self::CloseBehavior => CloseBehavior::ALL.get(index).map(|o| o.hint()),
            Self::TrayClick => TrayClick::ALL.get(index).map(|o| o.hint()),
            _ => None,
        }
    }

    /// Applies the row at `index`.
    fn apply(self, index: usize, cx: &mut App) {
        match self {
            Self::CloseBehavior => {
                if let Some(option) = CloseBehavior::ALL.get(index) {
                    app::update_config(cx, |config| config.close_behavior = *option);
                }
            }
            Self::TrayClick => {
                if let Some(option) = TrayClick::ALL.get(index) {
                    app::update_config(cx, |config| config.tray_left_click = *option);
                }
            }
            Self::Theme => {
                if let Some(option) = ThemeChoice::ALL.get(index) {
                    app::update_config(cx, |config| config.theme = *option);
                }
            }
            Self::FinishAction => {
                if let Some(option) = FinishAction::ALL.get(index) {
                    app::update_config(cx, |config| config.finish_action = *option);
                }
            }
            Self::Dim => {
                if let Some((value, _)) = DIM_LEVELS.get(index) {
                    app::update_config(cx, |config| config.dim_strength = *value);
                }
            }
            Self::SaveDir => {
                let dirs = save_dir_choices();
                if let Some((_, path)) = dirs.get(index) {
                    let path = path.clone();
                    app::update_config(cx, |config| config.save_dir = path);
                } else {
                    // The last row: ask the user for a folder.
                    let start = app::state(cx).config.save_dir.clone();
                    if let Some(picked) = win_util::pick_folder(&start) {
                        app::update_config(cx, |config| config.save_dir = picked);
                    }
                }
            }
        }
    }
}

/// How dark everything outside the selection is, and what to call each level.
const DIM_LEVELS: [(f32, &str); 3] = [(0.2, "浅"), (0.42, "中"), (0.62, "深")];

/// Width of a dropdown trigger, in logical pixels. The widget sizes itself; this
/// only keeps the right-hand column of every card the same width.
const DROPDOWN_WIDTH: f32 = 180.0;

/// The folders offered for 保存, as (label, path).
///
/// The physical `%USERPROFILE%` paths rather than the shell's display names: those
/// are localised, these are not.
fn save_dir_choices() -> Vec<(String, PathBuf)> {
    let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
        return Vec::new();
    };
    [
        ("桌面", "Desktop"),
        ("图片", "Pictures"),
        ("下载", "Downloads"),
        ("文档", "Documents"),
    ]
    .into_iter()
    .map(|(label, folder)| (label.to_string(), home.join(folder)))
    .collect()
}

pub struct Settings {
    focus: FocusHandle,
    page: Page,
    /// Where the current page is scrolled to.
    ///
    /// A page can easily be taller than the window — the general one is — and the
    /// window is fixed-size, so the only way to reach the bottom cards is to
    /// scroll. Kept per-window rather than per-page because switching pages
    /// resets it.
    content_scroll: ScrollHandle,
    recording: Recording,
    /// Keystrokes seen while recording, so a chord can be assembled.
    pending_modifiers: Modifiers,
    /// Title-bar drag in progress: the cursor and window origin at press time.
    dragging: Option<((i32, i32), (i32, i32))>,
    /// One `Select` state per choice setting, in `Select::ALL` order.
    ///
    /// gpui-component's dropdown keeps its own open state, its list, its cursor
    /// and its search box; the view only keeps the entity and subscribes to what
    /// the user confirms.
    selects: [Entity<SelectState<SelectList>>; Select::COUNT],
}

/// The item type the dropdowns are built from: a plain list of labels.
type SelectList = SearchableVec<String>;

impl Settings {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Start with keyboard focus so Escape and the hotkey recorder work
        // without the user having to click first.
        let focus = cx.focus_handle();
        window.focus(&focus, cx);

        // Each dropdown is a state entity of its own, built from the labels of
        // its `Select` and seeded with the value in force. The subscription is
        // what turns a confirmed row into a configuration change; it lives on the
        // view, so it goes away with the window.
        let selects: [Entity<SelectState<SelectList>>; Select::COUNT] =
            std::array::from_fn(|index| {
                let select = Select::ALL[index];
                let (labels, selected) = select.options(cx);
                let selected = selected.map(IndexPath::new);
                cx.new(|cx| SelectState::new(SearchableVec::new(labels), selected, window, cx))
            });
        for (index, state) in selects.iter().enumerate() {
            let select = Select::ALL[index];
            cx.subscribe(state, move |_settings, _state, event: &SelectEvent<SelectList>, cx| {
                // Only a confirmed row changes anything: dismissing the list
                // leaves the value alone, which is what Escape should mean.
                let SelectEvent::Confirm(value) = event;
                let Some(value) = value else {
                    return;
                };
                let (labels, _) = select.options(cx);
                if let Some(row) = labels.iter().position(|label| label == value) {
                    select.apply(row, cx);
                    cx.notify();
                }
            })
            .detach();
        }

        Self {
            focus,
            page: Page::General,
            content_scroll: ScrollHandle::new(),
            recording: Recording::None,
            pending_modifiers: Modifiers::default(),
            dragging: None,
            selects,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    // -- title bar dragging ------------------------------------------------

    /// Moves the window by hand.
    ///
    /// The toolkit's own window-drag gesture does nothing for a window without
    /// a native frame, so the title bar drives the move itself.
    fn drag_window(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left {
            return;
        }
        let start_cursor = crate::win::util::cursor_position();
        let start_origin =
            crate::win::util::client_origin(app::SETTINGS_TITLE).unwrap_or(start_cursor);
        self.dragging = Some((start_cursor, start_origin));
        cx.notify();
    }

    fn drag_move(&mut self, cx: &mut Context<Self>) {
        let Some((start_cursor, start_origin)) = self.dragging else {
            return;
        };
        let cursor = crate::win::util::cursor_position();
        crate::win::util::move_window_to(
            app::SETTINGS_TITLE,
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

    // -- hotkey recording --------------------------------------------------

    fn start_recording(&mut self, which: Recording, cx: &mut Context<Self>) {
        self.recording = which;
        self.pending_modifiers = Modifiers::default();
        cx.notify();
    }

    /// Turns the keystroke that just arrived into a binding.
    fn record_hotkey(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.recording == Recording::None {
            return false;
        }
        let key = event.keystroke.key.as_str();
        if key == "escape" {
            self.recording = Recording::None;
            cx.notify();
            return true;
        }
        // Modifiers on their own are recorded as they arrive; the binding is
        // committed by the first real key.
        if matches!(key, "control" | "alt" | "shift" | "platform" | "function") {
            self.pending_modifiers = event.keystroke.modifiers;
            return true;
        }
        let modifiers = event.keystroke.modifiers;
        let spec = chord_string(modifiers, &event.keystroke.key);
        if let Some(parsed) = hotkey::parse(&spec) {
            if parsed.is_bound() {
                let target = self.recording;
                app::update_config(cx, |config| match target {
                    Recording::Capture => config.hotkey_capture = parsed.display(),
                    Recording::Restore => config.hotkey_restore_pin = parsed.display(),
                    Recording::None => {}
                });
            }
        }
        self.recording = Recording::None;
        cx.notify();
        true
    }
}

/// Builds a `Ctrl+Alt+A` style string from a keystroke.
fn chord_string(modifiers: Modifiers, key: &str) -> String {
    let mut parts = Vec::new();
    if modifiers.control {
        parts.push("Ctrl");
    }
    if modifiers.alt {
        parts.push("Alt");
    }
    if modifiers.shift {
        parts.push("Shift");
    }
    if modifiers.platform {
        parts.push("Win");
    }
    let named = match key {
        "enter" => "Enter",
        "space" => "Space",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" => "Delete",
        "insert" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "printscreen" => "PrintScreen",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        other => other,
    };
    parts.push(named);
    parts.join("+")
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = app::state(cx).config.clone();
        let theme = config.resolved_theme();
        let page = self.page;

        // gpui-component carries a theme of its own, and its widgets are painted
        // from it. Left alone it follows the system, which would put a light
        // dropdown in a dark window whenever the two disagree; this keeps it in
        // step with the palette the rest of the window is drawn in.
        let mode = if theme.is_dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        if ComponentTheme::global(cx).mode != mode {
            ComponentTheme::change(mode, Some(window), cx);
        }

        let title_bar = self.title_bar(window, theme, cx);
        let sidebar = self.sidebar(theme, cx);
        let body = match page {
            Page::General => general_page(self, &config, theme, cx),
            Page::Appearance => appearance_page(self, &config, theme, cx),
            Page::Screenshot => screenshot_page(self, &config, theme, cx),
            Page::Shortcuts => shortcuts_page(self, &config, theme, cx),
            Page::About => about_page(theme, cx),
        };

        div()
            .id("settings-root")
            .flex()
            .flex_col()
            .size_full()
            .font_family(FONT_FAMILY)
            .text_size(px(TEXT_BODY))
            .text_color(theme.text)
            .bg(theme.window)
            .track_focus(&self.focus)
            .on_mouse_move(cx.listener(|this, _event, _window, cx| this.drag_move(cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event, _window, cx| this.end_drag(cx)),
            )
            .on_key_down(
                cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                    let _ = this.record_hotkey(event, cx);
                }),
            )
            .child(title_bar)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .overflow_hidden()
                    .child(sidebar)
                    .child(
                        // The page scrolls inside this column, and the scrollbar
                        // is a *sibling* of the scroller rather than a child of
                        // it: a child is painted with the scroll offset applied,
                        // so the bar would slide off with the content.
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            .child(
                                div()
                                    .id("settings-content")
                                    .size_full()
                                    .overflow_y_scroll()
                                    .track_scroll(&self.content_scroll)
                                    .child(
                                        div()
                                            .flex()
                                            .flex_col()
                                            .gap(px(8.0))
                                            .px(px(28.0))
                                            .py(px(20.0))
                                            .child(
                                                div()
                                                    .text_size(px(TEXT_TITLE))
                                                    .text_color(theme.text)
                                                    .pb(px(8.0))
                                                    .child(page.label()),
                                            )
                                            .child(body),
                                    ),
                            )
                            .child(
                                gpui_kit::base::Scrollbar::vertical(&self.content_scroll)
                                    .id("settings-scrollbar")
                                    .viewport_from_layout(),
                            ),
                    ),
            )
    }
}

impl Settings {
    fn title_bar(&self, window: &Window, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("settings-titlebar")
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(44.0))
            .pl(px(16.0))
            .pr(px(8.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event, _window, cx| this.drag_window(event, cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .child(
                        div()
                            .w(px(18.0))
                            .h(px(18.0))
                            .rounded(px(RADIUS_CONTROL))
                            .bg(theme.accent),
                    )
                    .child(
                        div()
                            .text_size(px(TEXT_CAPTION))
                            .text_color(theme.text_secondary)
                            .child("xshot 截图工具"),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(2.0))
                    .child(window_button("settings-minimize", glyph::MINIMIZE, theme, cx, |window, _cx| {
                        window.minimize_window();
                    }))
                    .child(window_button(
                        "settings-maximize",
                        // Two overlapping squares once it is maximized: the icon
                        // font's `maximize` is the "make it big" square, and its
                        // `fuzhi` is the conventional restore shape.
                        if window.is_maximized() { glyph::RESTORE } else { glyph::MAXIMIZE },
                        theme,
                        cx,
                        |window, _cx| {
                            if window.is_maximized() {
                                crate::win::util::restore_window(app::SETTINGS_TITLE);
                            } else {
                                window.zoom_window();
                            }
                        },
                    ))
                    .child(window_button("settings-close", glyph::CLOSE, theme, cx, |_window, cx| {
                        app::close_settings(cx);
                    })),
            )
            .into_any_element()
    }

    fn sidebar(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.page;
        let rows: Vec<AnyElement> = Page::ALL
            .into_iter()
            .map(|page| {
                let active = page == current;
                let mut row = div()
                    .id(page.label())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .h(px(NAV_ROW_HEIGHT))
                    .px(px(12.0))
                    .rounded(px(RADIUS_CONTROL))
                    .cursor_pointer()
                    .text_size(px(TEXT_BODY))
                    .text_color(if active { theme.text } else { theme.text_secondary })
                    .bg(if active { theme.card } else { rgba(0x0000_0000) })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, _window, cx| {
                            this.page = page;
                            // A new page starts at its top; keeping the old offset
                            // would drop the reader into the middle of it.
                            this.content_scroll.set_offset(point(px(0.0), px(0.0)));
                            cx.notify();
                        }),
                    );
                if !active {
                    row = row.hover(move |style| style.bg(theme.subtle_hover));
                }
                row.child(
                    div()
                        .w(px(18.0))
                        .text_size(px(TEXT_BODY))
                        .child(page.glyph()),
                )
                .child(page.label())
                .into_any_element()
            })
            .collect();

        div()
            .id("settings-sidebar")
            .flex()
            .flex_col()
            .gap(px(2.0))
            .w(px(196.0))
            .h_full()
            .p(px(10.0))
            .bg(theme.sidebar)
            .children(rows)
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// pages

fn card(theme: Theme) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(16.0))
        .rounded(px(RADIUS_CARD))
        .bg(theme.card)
        .border_1()
        .border_color(theme.stroke)
}

fn caption(text: String, theme: Theme) -> AnyElement {
    div()
        .text_size(px(TEXT_CAPTION))
        .text_color(theme.text_secondary)
        .child(text)
        .into_any_element()
}

fn general_page(state: &Settings, config: &crate::config::Config, theme: Theme, cx: &mut Context<Settings>) -> AnyElement {
    let save_dir = config.save_dir.display().to_string();

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("窗口关闭行为"))
                .child(setting_row_dropdown(state, Select::CloseBehavior, "关闭设置窗口时", theme, cx)),
        )
        .child(
            card(theme)
                .child(div().child("截图保存目录"))
                .child(setting_row_dropdown(state, Select::SaveDir, "保存到", theme, cx))
                .child(caption(format!("当前：{save_dir}"), theme))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(8.0))
                        .child(button("打开目录", false, theme, cx, |_window, cx| {
                            let directory = app::state(cx).config.save_dir.clone();
                            let _ = std::process::Command::new("explorer").arg(directory).spawn();
                        }))
                        .child(button("立即截图", true, theme, cx, |_window, cx| {
                            app::trigger_capture(cx);
                        })),
                ),
        )
        .child(
            card(theme)
                .child(div().child("完成后同时复制到剪贴板"))
                .child(caption(
                    "关闭后，保存或钉图不会顺带写入剪贴板".into(),
                    theme,
                ))
                .child(toggle(config.copy_on_finish, "copy-toggle", theme, cx, |config| {
                    config.copy_on_finish = !config.copy_on_finish;
                })),
        )
        .child(
            card(theme)
                .child(div().child("启动"))
                .child(setting_row(
                    "开机启动",
                    "登录 Windows 后自动运行 xshot",
                    config.autostart,
                    "autostart-toggle",
                    theme,
                    cx,
                    |config| {
                        config.autostart = !config.autostart;
                        // The registry is the real state, so write it as soon as
                        // the switch moves rather than at some later exit.
                        if let Err(error) = crate::win::autostart::set_enabled(config.autostart) {
                            eprintln!("xshot: could not update startup: {error:#}");
                        }
                    },
                ))
                .child(setting_row(
                    "静默启动",
                    "启动时不打开这个窗口，只驻留托盘图标",
                    config.silent_start,
                    "silent-toggle",
                    theme,
                    cx,
                    |config| config.silent_start = !config.silent_start,
                ))
                .child(setting_row_dropdown(state, Select::TrayClick, "左键点击托盘图标", theme, cx)),
        )
        .child(
            card(theme)
                .child(div().child("截图"))
                .child(setting_row(
                    "全屏窗口时不截图",
                    "前台窗口铺满整个显示器时（游戏、视频、演示），快捷键和 Ctrl+拖动都不响应",
                    config.skip_when_fullscreen,
                    "fullscreen-toggle",
                    theme,
                    cx,
                    |config| config.skip_when_fullscreen = !config.skip_when_fullscreen,
                )),
        )
        .into_any_element()
}

/// One of the three caption buttons in the title bar.
///
/// They are ordinary buttons in the title bar rather than a native caption: the
/// window has no system frame, so Windows has nothing to draw them for us.
fn window_button(
    id: &'static str,
    art: &'static str,
    theme: Theme,
    cx: &mut Context<Settings>,
    act: fn(&mut Window, &mut App),
) -> AnyElement {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(38.0))
        .h(px(30.0))
        .rounded(px(RADIUS_CONTROL))
        .cursor_pointer()
        .hover(move |style| {
            // Closing is the one that is allowed to look destructive.
            if id == "settings-close" {
                style.bg(theme.danger)
            } else {
                style.bg(theme.card_hover)
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_this, _event, window, cx| {
                cx.stop_propagation();
                act(window, cx);
            }),
        )
        .child(icons::icon(art, 15.0, theme.text_secondary))
        .into_any_element()
}

/// One "label + hint on the left, switch on the right" settings row.
fn setting_row(
    title: &'static str,
    hint: &'static str,
    value: bool,
    id: &'static str,
    theme: Theme,
    cx: &mut Context<Settings>,
    mutate: fn(&mut crate::config::Config),
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .flex_1()
                .child(div().text_size(px(TEXT_BODY)).text_color(theme.text).child(title))
                .child(caption(hint.into(), theme)),
        )
        .child(toggle(value, id, theme, cx, mutate))
        .into_any_element()
}

/// One "label on the left, dropdown on the right" settings row.
///
/// The dropdown is gpui-component's `Select` — the styled widget from the kit
/// (`gpui_kit::component`) — which brings its own trigger, list, caret, check
/// marks, keyboard handling and accessibility. What is left here is the label
/// beside it and the width it takes.
///
/// The widget owns its state: the view builds one [`SelectState`] per choice
/// setting in [`Settings::new`] (that is also where a confirmed row becomes a
/// configuration change) and hands the entity to the widget here.
fn setting_row_dropdown(
    state: &Settings,
    select: Select,
    title: &'static str,
    theme: Theme,
    cx: &mut Context<Settings>,
) -> AnyElement {
    let mut column = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .flex_1()
        .child(
            div()
                .text_size(px(TEXT_BODY))
                .text_color(theme.text)
                .child(title),
        );
    if let Some(hint) = select.hint(cx) {
        column = column.child(caption(hint.into(), theme));
    }

    let mut dropdown = ComponentSelect::new(&state.selects[select as usize])
        .accessibility_label(title)
        .w(px(DROPDOWN_WIDTH));
    // Only the save folder can be something the list does not offer; then the
    // widget shows it as its placeholder instead of a wrong row.
    if let Some(placeholder) = select.placeholder(cx) {
        dropdown = dropdown.placeholder(placeholder);
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(column)
        // The widget's own root is `size_full`, so it is given a box of its own:
        // otherwise it would fill the row and leave the trigger sitting against
        // the label instead of at the right-hand edge every other control uses.
        .child(
            div()
                .flex_shrink_0()
                .w(px(DROPDOWN_WIDTH))
                .child(dropdown),
        )
        .into_any_element()
}
fn appearance_page(state: &Settings, config: &crate::config::Config, theme: Theme, cx: &mut Context<Settings>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("主题"))
                .child(caption("设置窗口与右键菜单使用的配色".into(), theme))
                .child(setting_row_dropdown(state, Select::Theme, "配色", theme, cx)),
        )
        .child(
            card(theme)
                .child(div().child("钉图窗口阴影"))
                .child(caption("为贴在桌面上的图片加一层投影".into(), theme))
                .child(toggle(config.pin_shadow, "shadow-toggle", theme, cx, |config| {
                    config.pin_shadow = !config.pin_shadow;
                })),
        )
        .into_any_element()
}

fn screenshot_page(state: &Settings, _config: &crate::config::Config, theme: Theme, cx: &mut Context<Settings>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("Enter / 双击选区 的动作"))
                .child(caption("框选完成后按 Enter 或双击选区，直接执行这一步".into(), theme))
                .child(setting_row_dropdown(state, Select::FinishAction, "执行", theme, cx)),
        )
        .child(
            card(theme)
                .child(div().child("选区外的遮罩浓度"))
                .child(caption("截图窗口一打开就整屏变暗，方便看清将要框选的区域".into(), theme))
                .child(setting_row_dropdown(state, Select::Dim, "浓度", theme, cx)),
        )
        .into_any_element()
}

fn shortcuts_page(
    state: &Settings,
    config: &crate::config::Config,
    theme: Theme,
    cx: &mut Context<Settings>,
) -> AnyElement {
    let capture_valid = hotkey::parse(&config.hotkey_capture).is_some();
    let restore_valid = hotkey::parse(&config.hotkey_restore_pin).is_some();
    let status = app::state(cx).hotkey_status;
    // Only complain about a conflict once the tray thread has actually tried.
    let capture_taken = capture_valid && matches!(status, Some((false, _)));
    let restore_taken = restore_valid && matches!(status, Some((_, false)));
    let capture_bare = hotkey::parse(&config.hotkey_capture)
        .map(|key| !key.has_modifiers())
        .unwrap_or(false);

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("全局快捷键"))
                .child(caption(
                    "点击右侧按钮后按下新的按键。功能键（F1…）最方便，但会被系统独占，\
                     其他程序就收不到了；加 Ctrl / Alt / Shift 可以避免这一点。"
                        .into(),
                    theme,
                ))
                .child(hotkey_row(
                    "截图",
                    &config.hotkey_capture,
                    capture_valid,
                    capture_taken,
                    capture_bare,
                    state.recording == Recording::Capture,
                    Recording::Capture,
                    theme,
                    cx,
                ))
                .child(hotkey_row(
                    "恢复上次贴图",
                    &config.hotkey_restore_pin,
                    restore_valid,
                    restore_taken,
                    false,
                    state.recording == Recording::Restore,
                    Recording::Restore,
                    theme,
                    cx,
                )),
        )
        .child(
            card(theme)
                .child(div().child("Ctrl + 鼠标左键拖动"))
                .child(caption(
                    "按下左键截图窗口就出来并冻结屏幕，跟着鼠标框选，松手即钉在桌面上；\
                     按了没移动就松开只关窗。这个手势只观察鼠标，不会拦截点击，其他程序照常收到拖拽。"
                        .into(),
                    theme,
                ))
                .child(toggle(
                    config.ctrl_drag_pin,
                    "ctrl-drag-toggle",
                    theme,
                    cx,
                    |config| config.ctrl_drag_pin = !config.ctrl_drag_pin,
                )),
        )
        .child(
            card(theme)
                .child(div().child("标注工具"))
                .child(caption(
                    "1 矩形 · 2 圆形 · 3 铅笔 · 4 文字 · 5 箭头 · 6 马赛克 · 7 橡皮 · \
                     Ctrl+Z 撤销 · Esc 取消"
                        .into(),
                    theme,
                )),
        )
        .into_any_element()
}

fn hotkey_row(
    title: &'static str,
    value: &str,
    valid: bool,
    taken: bool,
    bare: bool,
    recording: bool,
    which: Recording,
    theme: Theme,
    cx: &mut Context<Settings>,
) -> AnyElement {
    let label = if recording {
        "请按下按键…".to_string()
    } else if !valid {
        format!("{value}（无效）")
    } else if taken {
        format!("{value}（已被占用）")
    } else if bare {
        format!("{value}（独占该键）")
    } else {
        value.to_string()
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .child(div().child(title))
        .child(
            div()
                .id(title)
                .flex()
                .items_center()
                .justify_center()
                .h(px(CONTROL_HEIGHT))
                .px(px(14.0))
                .rounded(px(RADIUS_CONTROL))
                .cursor_pointer()
                .text_size(px(TEXT_BODY))
                .bg(if recording { theme.accent } else { theme.subtle_pressed })
                .text_color(if recording {
                    theme.on_accent
                } else if taken || !valid {
                    theme.danger
                } else if bare {
                    theme.text_secondary
                } else {
                    theme.text
                })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        this.start_recording(which, cx);
                    }),
                )
                .child(label),
        )
        .into_any_element()
}

// ---------------------------------------------------------------------------
// controls

fn button(
    label: &'static str,
    primary: bool,
    theme: Theme,
    cx: &mut Context<Settings>,
    on_press: impl Fn(&mut Window, &mut App) + 'static,
) -> AnyElement {
    let mut element = div()
        .id(label)
        .flex()
        .items_center()
        .justify_center()
        .h(px(CONTROL_HEIGHT))
        .px(px(18.0))
        .rounded(px(RADIUS_CONTROL))
        .cursor_pointer()
        .text_size(px(TEXT_BODY))
        .bg(if primary { theme.accent } else { theme.subtle_pressed })
        .text_color(if primary { theme.on_accent } else { theme.text })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_this, _event, window, cx| {
                on_press(window, cx);
            }),
        );
    element = element.hover(move |style| {
        style.bg(if primary {
            theme.accent_hover
        } else {
            theme.subtle_hover
        })
    });
    element.child(label).into_any_element()
}

/// A Fluent toggle switch; the knob slides to the right when on.
fn toggle(
    value: bool,
    id: &'static str,
    theme: Theme,
    cx: &mut Context<Settings>,
    mutate: fn(&mut crate::config::Config),
) -> AnyElement {    let mut knob = div()
        .w(px(12.0))
        .h(px(12.0))
        .rounded(px(6.0))
        .bg(if value { theme.on_accent } else { theme.text_secondary });
    if value {
        knob = knob.ml(px(16.0));
    }

    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .w(px(40.0))
        .h(px(20.0))
        .px(px(4.0))
        .rounded(px(10.0))
        .cursor_pointer()
        .bg(if value { theme.accent } else { theme.subtle_pressed })
        .border_1()
        .border_color(if value { theme.accent } else { theme.stroke_strong })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |_this, _event, _window, cx| {
                app::update_config(cx, mutate);
                cx.notify();
            }),
        )
        .child(knob)
        .into_any_element()
}

/// The logo, as a GPUI image.
///
/// It comes from `assets/logo.png`, the same one file the tray icon, the
/// executable's icon resource and the installer are all built from, so the mark
/// on this page cannot drift away from the icon in the notification area.
fn logo_image(size: i32) -> std::sync::Arc<gpui_kit::RenderImage> {
    use std::sync::Arc;

    let rgba = crate::win::icon::rasterize(size);
    let size = size.clamp(1, 1024) as u32;
    // `RenderImage` wants BGRA, and premultiplied: the renderer blends with
    // straight multiplication, so a transparent pixel that still carries colour
    // would fringe.
    let mut bgra = rgba;
    for pixel in bgra.chunks_exact_mut(4) {
        let alpha = pixel[3] as u32;
        let scale = |channel: u8| ((channel as u32 * alpha) / 255) as u8;
        let (r, g, b) = (scale(pixel[0]), scale(pixel[1]), scale(pixel[2]));
        pixel[0] = b;
        pixel[1] = g;
        pixel[2] = r;
    }
    let buffer = image::RgbaImage::from_raw(size, size, bgra).expect("logo dimensions match");
    Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(buffer)]))
}

/// The about page: what this is, and which build it is.
///
/// The version comes from `CARGO_PKG_VERSION`, so it is the version in
/// `Cargo.toml` at build time — which is exactly what `package.ps1` rewrites, and
/// what the executable's own version resource and the installer are stamped with.
fn about_page(theme: Theme, _cx: &mut Context<Settings>) -> AnyElement {
    let version = env!("CARGO_PKG_VERSION");
    let name = env!("CARGO_PKG_DESCRIPTION");

    let row = |label: &'static str, value: String, theme: Theme| -> AnyElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .child(
                div()
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text_secondary)
                    .child(label),
            )
            .child(
                div()
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text)
                    .child(value),
            )
            .into_any_element()
    };

    // The image itself, not an entity: `img` takes the render image directly.
    let logo = logo_image(96);

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(16.0))
                        .child(img(logo).w(px(96.0)).h(px(96.0)))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .child(
                                    div()
                                        .text_size(px(TEXT_TITLE))
                                        .text_color(theme.text)
                                        .child("xshot"),
                                )
                                .child(
                                    div()
                                        .text_size(px(TEXT_BODY))
                                        .text_color(theme.text_secondary)
                                        .child(format!("版本 {version}")),
                                )
                                .child(
                                    div()
                                        .text_size(px(TEXT_CAPTION))
                                        .text_color(theme.text_secondary)
                                        .child(name),
                                ),
                        ),
                ),
        )
        .child(
            card(theme)
                .child(row("程序名", "xshot".into(), theme))
                .child(row("版本", version.into(), theme))
                .child(row("目标平台", format!("Windows {}", std::env::consts::ARCH), theme))
                .child(caption(
                    "版本号来自 Cargo.toml，打包时由 package.ps1 改写；exe 的属性页显示的也是它。"
                        .into(),
                    theme,
                )),
        )
        .into_any_element()
}
