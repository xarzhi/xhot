//! The settings window, in Fluent Design.
//!
//! A navigation pane on the left and a page of cards on the right, following
//! the Fluent 2 / Windows 11 conventions: Mica behind everything, layered card
//! fills instead of borders, 4 px control radii, 32 px controls, and the Segoe
//! UI Variable text ramp. Every colour comes from [`crate::ui::theme`] so a new
//! setting cannot drift away from the system.

use gpui_kit::*;

use crate::app;
use crate::config::{CloseBehavior, FinishAction, ThemeChoice};
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
}

impl Page {
    const ALL: [Page; 4] = [
        Page::General,
        Page::Appearance,
        Page::Screenshot,
        Page::Shortcuts,
    ];

    fn label(self) -> &'static str {
        match self {
            Page::General => "常规设置",
            Page::Appearance => "外观设置",
            Page::Screenshot => "截图设置",
            Page::Shortcuts => "快捷键设置",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Page::General => "⚙",
            Page::Appearance => "🎨",
            Page::Screenshot => "✂",
            Page::Shortcuts => "⌨",
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

pub struct Settings {
    focus: FocusHandle,
    page: Page,
    recording: Recording,
    /// Keystrokes seen while recording, so a chord can be assembled.
    pending_modifiers: Modifiers,
    /// Title-bar drag in progress: the cursor and window origin at press time.
    dragging: Option<((i32, i32), (i32, i32))>,
}

impl Settings {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Start with keyboard focus so Escape and the hotkey recorder work
        // without the user having to click first.
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        Self {
            focus,
            page: Page::General,
            recording: Recording::None,
            pending_modifiers: Modifiers::default(),
            dragging: None,
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let config = app::state(cx).config.clone();
        let theme = config.resolved_theme();
        let page = self.page;

        let title_bar = self.title_bar(theme, cx);
        let sidebar = self.sidebar(theme, cx);
        let body = match page {
            Page::General => general_page(&config, theme, cx),
            Page::Appearance => appearance_page(&config, theme, cx),
            Page::Screenshot => screenshot_page(&config, theme, cx),
            Page::Shortcuts => shortcuts_page(self, &config, theme, cx),
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
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .gap(px(8.0))
                            .px(px(28.0))
                            .py(px(20.0))
                            .overflow_hidden()
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
    }
}

impl Settings {
    fn title_bar(&self, theme: Theme, cx: &mut Context<Self>) -> AnyElement {
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
                    .id("settings-close")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(40.0))
                    .h(px(30.0))
                    .rounded(px(RADIUS_CONTROL))
                    .cursor_pointer()
                    .text_size(px(TEXT_BODY))
                    .text_color(theme.text_secondary)
                    .hover(move |style| style.bg(theme.danger).text_color(theme.window))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|_this, _event, _window, cx| {
                            cx.stop_propagation();
                            app::close_settings(cx);
                        }),
                    )
                    .child("✕"),
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

fn general_page(config: &crate::config::Config, theme: Theme, cx: &mut Context<Settings>) -> AnyElement {
    let behavior = config.close_behavior;
    let save_dir = config.save_dir.display().to_string();

    let behavior_choices: Vec<AnyElement> = CloseBehavior::ALL
        .into_iter()
        .map(|option| {
            let active = option == behavior;
            div()
                .id(option.label())
                .flex()
                .flex_col()
                .gap(px(2.0))
                .flex_1()
                .p(px(12.0))
                .rounded(px(RADIUS_CONTROL))
                .cursor_pointer()
                .bg(if active { theme.card_hover } else { theme.subtle_pressed })
                .border_1()
                .border_color(if active { theme.accent } else { theme.stroke })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, _event, _window, cx| {
                        app::update_config(cx, |config| config.close_behavior = option);
                        cx.notify();
                    }),
                )
                .child(div().child(option.label()))
                .child(caption(option.hint().into(), theme))
                .into_any_element()
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("关闭设置窗口时"))
                .child(div().flex().flex_row().gap(px(10.0)).children(behavior_choices)),
        )
        .child(
            card(theme)
                .child(div().child("截图保存目录"))
                .child(caption(save_dir, theme))
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
        .into_any_element()
}

fn appearance_page(config: &crate::config::Config, theme: Theme, cx: &mut Context<Settings>) -> AnyElement {
    let choice = config.theme;
    let chooser: Vec<AnyElement> = ThemeChoice::ALL
        .into_iter()
        .map(|option| {
            let active = option == choice;
            div()
                .id(option.label())
                .flex()
                .items_center()
                .justify_center()
                .h(px(CONTROL_HEIGHT))
                .px(px(16.0))
                .rounded(px(RADIUS_CONTROL))
                .cursor_pointer()
                .bg(if active { theme.accent } else { theme.subtle_pressed })
                .text_color(if active { theme.on_accent } else { theme.text })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, _event, _window, cx| {
                        app::update_config(cx, |config| config.theme = option);
                        cx.notify();
                    }),
                )
                .child(option.label())
                .into_any_element()
        })
        .collect();

    // A row of swatches makes the active palette visible without a preview pane.
    let swatches: Vec<AnyElement> = [
        theme.accent,
        theme.card,
        theme.window,
        theme.stroke_strong,
        theme.text,
        theme.danger,
    ]
    .into_iter()
    .map(|color| {
        div()
            .w(px(40.0))
            .h(px(40.0))
            .rounded(px(RADIUS_CONTROL))
            .bg(color)
            .border_1()
            .border_color(theme.stroke)
            .into_any_element()
    })
    .collect();

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("主题"))
                .child(caption("设置窗口与右键菜单使用的配色".into(), theme))
                .child(div().flex().flex_row().gap(px(8.0)).children(chooser)),
        )
        .child(
            card(theme)
                .child(div().child("当前配色"))
                .child(div().flex().flex_row().gap(px(8.0)).children(swatches)),
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

fn screenshot_page(config: &crate::config::Config, theme: Theme, cx: &mut Context<Settings>) -> AnyElement {
    let action = config.finish_action;
    let dim = config.dim_strength;

    let actions: Vec<AnyElement> = FinishAction::ALL
        .into_iter()
        .map(|option| {
            let active = option == action;
            div()
                .id(option.label())
                .flex()
                .items_center()
                .justify_center()
                .h(px(CONTROL_HEIGHT))
                .px(px(14.0))
                .rounded(px(RADIUS_CONTROL))
                .cursor_pointer()
                .bg(if active { theme.accent } else { theme.subtle_pressed })
                .text_color(if active { theme.on_accent } else { theme.text })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_this, _event, _window, cx| {
                        app::update_config(cx, |config| config.finish_action = option);
                        cx.notify();
                    }),
                )
                .child(option.label())
                .into_any_element()
        })
        .collect();

    let dims: Vec<AnyElement> = [0.2f32, 0.42, 0.62]
        .into_iter()
        .map(|value| {
            let active = (value - dim).abs() < 0.01;
            let label = if value < 0.3 {
                "浅"
            } else if value < 0.5 {
                "中"
            } else {
                "深"
            };
            div()
                .id(("dim", (value * 100.0) as u32))
                .flex()
                .items_center()
                .justify_center()
                .h(px(CONTROL_HEIGHT))
                .px(px(16.0))
                .rounded(px(RADIUS_CONTROL))
                .cursor_pointer()
                .bg(if active { theme.accent } else { theme.subtle_pressed })
                .text_color(if active { theme.on_accent } else { theme.text })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |_event_this, _event, _window, cx| {
                        app::update_config(cx, |config| config.dim_strength = value);
                        cx.notify();
                    }),
                )
                .child(label)
                .into_any_element()
        })
        .collect();

    div()
        .flex()
        .flex_col()
        .gap(px(12.0))
        .child(
            card(theme)
                .child(div().child("Enter / 双击选区 的动作"))
                .child(div().flex().flex_row().gap(px(8.0)).children(actions)),
        )
        .child(
            card(theme)
                .child(div().child("选区外的遮罩浓度"))
                .child(caption("截图窗口一打开就整屏变暗，方便看清将要框选的区域".into(), theme))
                .child(div().flex().flex_row().gap(px(8.0)).children(dims)),
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
                    "直接在任意位置拖出一个矩形，松手即截图并钉在桌面上。\
                     这个手势只观察鼠标，不会拦截点击，其他程序照常收到拖拽。"
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
) -> AnyElement {
    let mut knob = div()
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
