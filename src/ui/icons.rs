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
/// `left-arrow` (`e642`, `e643`, `eca7`, `eca8`) which nothing draws yet.
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

/// The two glyphs the icon font does not have.
///
/// `iconfont.json` covers the capture tools and the window buttons but stops
/// short of the pencil and the text tool, so those are stroked by hand in the
/// same 16-unit box and at the same weight, which keeps them from standing out.
/// Adding `pen` / `text` to the iconfont would replace them with one line each.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Drawn {
    Pen,
    Text,
}

/// A hand-stroked icon, centred in a `size` × `size` box.
pub fn drawn(kind: Drawn, size: f32, color: Rgba) -> AnyElement {
    let scale = size / 16.0;
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let origin = bounds.origin;
            let at = |x: f32, y: f32| point(origin.x + px(x * scale), origin.y + px(y * scale));
            // The font's own strokes are about 1.4 units wide in this box.
            let mut builder = PathBuilder::stroke(px(1.4 * scale));
            match kind {
                Drawn::Pen => {
                    // Body, then the sharpened tip, so it reads as a pencil and
                    // not just a diagonal bar.
                    builder.move_to(at(4.4, 11.6));
                    builder.line_to(at(10.4, 5.6));
                    builder.line_to(at(12.0, 7.2));
                    builder.line_to(at(6.0, 13.2));
                    builder.close();
                    builder.move_to(at(3.0, 14.0));
                    builder.line_to(at(3.6, 11.2));
                    builder.line_to(at(6.0, 13.2));
                }
                Drawn::Text => {
                    // A capital T: the bar, then the stem.
                    builder.move_to(at(3.4, 4.0));
                    builder.line_to(at(12.6, 4.0));
                    builder.move_to(at(8.0, 4.0));
                    builder.line_to(at(8.0, 13.0));
                }
            }
            if let Ok(path) = builder.build() {
                window.paint_path(path, color);
            }
        },
    )
    .w(px(size))
    .h(px(size))
    .into_any_element()
}
