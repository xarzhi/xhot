//! Fluent Design tokens.
//!
//! Everything the settings surface draws takes its colour, radius and metric
//! from here, so the app reads as one Fluent surface and a future setting can
//! never invent its own palette. The values follow the Fluent 2 / Windows 11
//! token set: layered fills rather than borders, 4 px control radii, 8 px
//! cards, and the Segoe UI text ramp.

use gpui_kit::*;

/// Control corner radius (buttons, inputs, list rows).
pub const RADIUS_CONTROL: f32 = 4.0;
/// Card / dialog corner radius.
pub const RADIUS_CARD: f32 = 8.0;
/// Standard control height.
pub const CONTROL_HEIGHT: f32 = 32.0;
/// Height of a navigation row.
pub const NAV_ROW_HEIGHT: f32 = 36.0;

/// Body text size, in logical pixels.
pub const TEXT_BODY: f32 = 14.0;
/// Secondary/caption text size.
pub const TEXT_CAPTION: f32 = 12.0;
/// Page title size.
pub const TEXT_TITLE: f32 = 20.0;

/// The family Fluent uses on Windows 11.
pub const FONT_FAMILY: &str = "Segoe UI Variable Text";

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub is_dark: bool,

    /// Backdrop behind everything (drawn over the Mica material).
    pub window: Rgba,
    /// Navigation pane background.
    pub sidebar: Rgba,
    /// Card and control surface.
    pub card: Rgba,
    pub card_hover: Rgba,
    pub card_pressed: Rgba,
    /// Barely-there fill used for list row hover.
    pub subtle_hover: Rgba,
    pub subtle_pressed: Rgba,
    /// Divider and control outline.
    pub stroke: Rgba,
    pub stroke_strong: Rgba,

    pub text: Rgba,
    pub text_secondary: Rgba,
    pub text_disabled: Rgba,

    pub accent: Rgba,
    pub accent_hover: Rgba,
    pub accent_pressed: Rgba,
    /// Text drawn on top of the accent fill.
    pub on_accent: Rgba,
    /// Translucent accent used behind selected text or an active row.
    pub selection: Rgba,

    pub danger: Rgba,
    pub danger_hover: Rgba,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            is_dark: true,
            window: rgba(0x2020_20F2),
            sidebar: rgba(0x2727_27B8),
            card: rgba(0xFFFF_FF0D),
            card_hover: rgba(0xFFFF_FF14),
            card_pressed: rgba(0xFFFF_FF0A),
            subtle_hover: rgba(0xFFFF_FF0F),
            subtle_pressed: rgba(0xFFFF_FF08),
            stroke: rgba(0xFFFF_FF12),
            stroke_strong: rgba(0xFFFF_FF1F),
            text: rgba(0xFFFF_FFFF),
            text_secondary: rgba(0xFFFF_FFC9),
            text_disabled: rgba(0xFFFF_FF5C),
            accent: rgba(0x60CD_FFFF),
            accent_hover: rgba(0x7AD8_FFFF),
            accent_pressed: rgba(0x4FB8_E8FF),
            on_accent: rgba(0x0000_00FF),
            selection: rgba(0x60CD_FF40),
            danger: rgba(0xFF99_A4FF),
            danger_hover: rgba(0xFFB3_BCFF),
        }
    }

    pub fn light() -> Self {
        Self {
            is_dark: false,
            window: rgba(0xF3F3_F3F2),
            sidebar: rgba(0xEBEB_EBCC),
            card: rgba(0xFFFF_FFB3),
            card_hover: rgba(0xFFFF_FFD9),
            card_pressed: rgba(0xF9F9_F9FF),
            subtle_hover: rgba(0x0000_000A),
            subtle_pressed: rgba(0x0000_0005),
            stroke: rgba(0x0000_000F),
            stroke_strong: rgba(0x0000_001F),
            text: rgba(0x1B1B_1BFF),
            text_secondary: rgba(0x0000_009C),
            text_disabled: rgba(0x0000_005C),
            accent: rgba(0x005F_B8FF),
            accent_hover: rgba(0x1975_C2FF),
            accent_pressed: rgba(0x004C_94FF),
            on_accent: rgba(0xFFFF_FFFF),
            selection: rgba(0x005F_B833),
            danger: rgba(0xC42B_1CFF),
            danger_hover: rgba(0xD13A_2BFF),
        }
    }

    /// Backdrop appearance handed to GPUI when creating a window.
    pub fn backdrop(&self) -> WindowBackgroundAppearance {
        // Mica is the Fluent 2 window material on Windows 11; on older builds
        // GPUI falls back to an opaque window, which the layer fills cover.
        WindowBackgroundAppearance::MicaBackdrop
    }
}