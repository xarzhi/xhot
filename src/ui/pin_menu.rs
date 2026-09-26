//! The pin's right-click menu, as a window of its own.
//!
//! A pin is exactly as large as its image, so a menu drawn inside it would be
//! clipped the moment it ran past the edge — which it always does near the
//! bottom or right of the screen. Giving the menu its own borderless topmost
//! window lets it extend anywhere, and it closes as soon as it loses
//! activation, the way a native menu does.

use gpui_kit::*;

use crate::ui::theme::{RADIUS_CARD, RADIUS_CONTROL, TEXT_BODY, Theme};

/// What the user picked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    CopyImage,
    CopyText,
    Edit,
    ToggleShadow,
    Save,
    Close,
}

struct Item {
    label: String,
    choice: MenuChoice,
    enabled: bool,
    separator_after: bool,
}

pub struct PinMenu {
    focus: FocusHandle,
    theme: Theme,
    items: Vec<Item>,
    chosen: Option<MenuChoice>,
    dismissed: bool,
}

impl PinMenu {
    pub fn new(shadowed: bool, has_text: bool, theme: Theme, cx: &mut Context<Self>) -> Self {
        let items = vec![
            Item {
                label: "复制图像".into(),
                choice: MenuChoice::CopyImage,
                enabled: true,
                separator_after: false,
            },
            Item {
                label: if has_text {
                    "复制全部文字".into()
                } else {
                    "复制全部文字（未识别到）".into()
                },
                choice: MenuChoice::CopyText,
                enabled: has_text,
                separator_after: true,
            },
            Item {
                label: "编辑".into(),
                choice: MenuChoice::Edit,
                enabled: true,
                separator_after: true,
            },
            Item {
                label: if shadowed {
                    "取消窗口阴影".into()
                } else {
                    "打开窗口阴影".into()
                },
                choice: MenuChoice::ToggleShadow,
                enabled: true,
                separator_after: false,
            },
            Item {
                label: "保存图像至路径".into(),
                choice: MenuChoice::Save,
                enabled: true,
                separator_after: true,
            },
            Item {
                label: "关闭".into(),
                choice: MenuChoice::Close,
                enabled: true,
                separator_after: false,
            },
        ];
        Self {
            focus: cx.focus_handle(),
            theme,
            items,
            chosen: None,
            dismissed: false,
        }
    }

    pub fn take_choice(&self) -> Option<MenuChoice> {
        self.chosen
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    pub fn was_dismissed(&self) -> bool {
        self.dismissed
    }

    /// Logical size the menu window should have.
    pub fn size(&self) -> (f32, f32) {
        let rows: f32 = self
            .items
            .iter()
            .map(|item| if item.separator_after { 30.0 + 9.0 } else { 30.0 })
            .sum();
        (200.0, 8.0 + rows)
    }
}

impl Render for PinMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme;

        let mut rows: Vec<AnyElement> = Vec::new();
        for (index, item) in self.items.iter().enumerate() {
            let enabled = item.enabled;
            let choice = item.choice;
            let mut row = div()
                .id(("pin-menu-row", index))
                .flex()
                .items_center()
                .h(px(30.0))
                .px(px(12.0))
                .rounded(px(RADIUS_CONTROL))
                .text_size(px(TEXT_BODY))
                .text_color(if enabled { theme.text } else { theme.text_disabled })
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, _window, cx| {
                        if enabled {
                            this.chosen = Some(choice);
                            cx.notify();
                        }
                    }),
                );
            if enabled {
                row = row
                    .cursor_pointer()
                    .hover(move |style| style.bg(theme.subtle_hover));
            }
            rows.push(row.child(item.label.clone()).into_any_element());

            if item.separator_after {
                rows.push(
                    div()
                        .h(px(1.0))
                        .my(px(4.0))
                        .mx(px(8.0))
                        .bg(theme.stroke)
                        .into_any_element(),
                );
            }
        }

        div()
            .id("pin-menu-root")
            .flex()
            .flex_col()
            .size_full()
            .p(px(4.0))
            .rounded(px(RADIUS_CARD))
            .bg(theme.window)
            .border_1()
            .border_color(theme.stroke_strong)
            .shadow_xl()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                if event.keystroke.key == "escape" {
                    this.dismissed = true;
                    cx.notify();
                }
            }))
            .children(rows)
    }
}
