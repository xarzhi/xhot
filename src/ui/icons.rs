//! The icon font and the glyphs the UI draws with.
//!
//! `assets/iconfont` is a standard iconfont.cn export: one `iconfont.ttf` plus
//! the sprite JSON that names each glyph. Only the `.ttf` is used here — the
//! web formats are left in place because they are what the site regenerates.
//!
//! Icons are drawn as **text in that font**, which matters for the thing people
//! notice: DirectWrite antialiases a glyph with the same rasteriser it uses for
//! every other character, so an icon stays sharp at any size and picks up the
//! colour it is given, with none of the bitmap scaling that made the old emoji
//! stand-ins look ragged.

use gpui_kit::*;

/// The family name declared inside `iconfont.ttf`.
const FAMILY: &str = "iconfont";

/// One code point per glyph, straight from `assets/iconfont/iconfont.json`.
///
/// The font also carries `up-arrow`, `down-arrow`, `right-arrow` and
/// `left-arrow` (`e642`, `e643`, `eca7`, `eca8`) and `录屏` (`e60a`) which
/// nothing draws yet.
pub mod glyph {
    pub const ARROW: &str = "\u{e630}";
    pub const SAVE: &str = "\u{ec09}";
    pub const REVOKE: &str = "\u{e6e2}";
    pub const COPY: &str = "\u{e62f}";
    /// The icon font's second "two overlapping squares" glyph, used as the
    /// window's restore button.
    pub const RESTORE: &str = "\u{e67e}";
    pub const PIN: &str = "\u{e61a}";
    pub const RECT: &str = "\u{e620}";
    pub const RUBBER: &str = "\u{ea15}";
    pub const MOSAIC: &str = "\u{e749}";
    pub const CIRCLE: &str = "\u{e6c7}";
    pub const MINIMIZE: &str = "\u{e711}";
    pub const MAXIMIZE: &str = "\u{e655}";
    pub const CLOSE: &str = "\u{e609}";
    /// The pencil, which the font files under `编辑`.
    ///
    /// Until this glyph existed the pencil was stroked by hand in a 16-unit box;
    /// the font's own drawing replaced it, so the whole toolbar is now one set.
    pub const PEN: &str = "\u{e600}";
    /// The letter T, which the font files under `文字`.
    pub const TEXT: &str = "\u{e732}";
    /// The scrolling shot, which the font files under `长截图`.
    pub const LONG_SHOT: &str = "\u{e601}";
}

/// Registers the font. Call once, before the first window exists.
pub fn load(cx: &App) {
    const FONT: &[u8] = include_bytes!("../assets/iconfont/iconfont.ttf");
    if let Err(error) = cx
        .text_system()
        .add_fonts(vec![std::borrow::Cow::Borrowed(FONT)])
    {
        eprintln!("xshot: could not load assets/iconfont/iconfont.ttf: {error:#}");
    }
}

/// One font glyph, centred in a `size` × `size` box.
pub fn icon(glyph: &'static str, size: f32, color: Rgba) -> AnyElement {
    labelled(glyph, size, color, size * 0.86)
}

/// A font glyph with an explicit font size, for buttons whose box and glyph
/// want different numbers.
pub fn labelled(glyph: &'static str, box_size: f32, color: Rgba, font_size: f32) -> AnyElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w(px(box_size))
        .h(px(box_size))
        .font_family(FAMILY)
        .text_size(px(font_size))
        .text_color(color)
        .child(glyph)
        .into_any_element()
}
