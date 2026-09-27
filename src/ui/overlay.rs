//! The QQ-style capture overlay.
//!
//! This is one GPUI window stretched across the virtual desktop showing the
//! frozen screen grab, with GPUI elements and paths layered on top: the dimmed
//! mask, the marching selection, the live annotation preview, the floating
//! toolbar and the text tool. Nothing here touches pixels directly — the
//! vector work is GPUI's `canvas`/`Path` API, and the final composite happens
//! once, on commit.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;

use crate::annot::{BRUSHES, ERASERS, MosaicPreview, PALETTE, Pt, Rect, Shape, Tool, WIDTHS};
use crate::app;
use crate::win::frame::Dib;
use crate::win::util as win_util;
use crate::win::{clip, paint, tray};

/// What committing the current selection should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
    Copy,
    Save,
    Pin,
}

/// Which long-running capture the user asked for.
#[derive(Clone, Copy)]
enum Session {
    /// Keep grabbing the framed region while the user scrolls the window
    /// underneath, and stitch the new rows into one tall shot.
    Scrolling,
}

/// A toolbar button press.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Action {
    Arm(Tool),
    Stroke(u32),
    Weight(f32),
    Brush(f32),
    EraserSize(f32),
    Block(f32),
    Undo,
    LongShot,
    /// Keep what a session has stitched (the toolbar's 复制).
    SessionKeep,
    /// Abandon a session (the toolbar's 关闭).
    SessionDiscard,
    Copy,
    Save,
    Pin,
    Cancel,
}

/// Which edge or corner of the selection is being dragged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    N,
    S,
    W,
    E,
    NW,
    NE,
    SW,
    SE,
}

impl Handle {
    /// The pointer that tells the user which way this handle will pull.
    fn cursor(self) -> CursorStyle {
        match self {
            Handle::N | Handle::S => CursorStyle::ResizeUpDown,
            Handle::W | Handle::E => CursorStyle::ResizeLeftRight,
            Handle::NW | Handle::SE => CursorStyle::ResizeUpLeftDownRight,
            Handle::NE | Handle::SW => CursorStyle::ResizeUpRightDownLeft,
        }
    }
}

/// How thick a session's frame is drawn, in logical pixels.
///
/// It sits on the *outside* of the region, so it costs the shot nothing.
const FRAME_THICKNESS: f32 = 4.0;
/// The gap between the framed region and the drawn frame, in logical pixels.
const FRAME_GAP: f32 = 3.0;
/// How far the window's region reaches past the drawn frame, in logical pixels.
const REGION_SLACK: f32 = 2.0;
/// The session toolbar's size, in logical pixels.
///
/// Fixed rather than measured: its rectangle is added to the window's region so
/// the two buttons can still be clicked while every other pixel belongs to the
/// application underneath, and a region is set from numbers, not from a painted
/// layout. Two 28 px buttons, their gap and the padding.
const SESSION_TOOLBAR_SIZE: (f32, f32) = (76.0, 38.0);

/// An interaction currently in flight.
#[derive(Debug, Clone)]
enum Drag {
    /// Rubber-banding a brand new selection.
    Creating { anchor: Pt },
    /// Sliding the whole selection.
    Moving { grab: Pt, original: Rect },
    /// Resizing from one edge or corner.
    Resizing { handle: Handle, original: Rect },
    /// Drawing a shape with one of the tools.
    Drawing,
    /// Wiping annotations with the eraser, remembering where it last was.
    Erasing { last: Pt },
    /// Sliding an already-drawn annotation, by index into `shapes`.
    MovingShape { index: usize, last: Pt },
    /// Resizing a selected annotation from one of its handles.
    ResizingShape { index: usize, handle: Handle, original: Rect },
}

/// In-progress text entry, with offsets in UTF-16 units to match GPUI's IME
/// contract.
struct TextEdit {
    origin: Pt,
    text: String,
    selection: Range<usize>,
    marked: Option<Range<usize>>,
}

/// Rough height of the floating toolbar, used to flip it above the selection
/// when there is not enough room below.
const TOOLBAR_HEIGHT: f32 = 38.0;
/// How close the pointer must be to a handle to grab it.
const HANDLE_GRAB: f32 = 7.0;
/// How close the pointer must be to an annotation to grab and move it.
const GRAB_SLACK: f32 = 4.0;
/// A drag shorter than this is treated as a click and clears the selection.
const MIN_SELECTION: f32 = 4.0;

/// Arrow head geometry, as multiples of the stroke width.
const ARROW_HEAD_LENGTH: f32 = 5.0;
const ARROW_HEAD_HALF_WIDTH: f32 = 2.1;

/// Side of the loupe's source crop, in screen pixels. Odd, so there is a middle
/// pixel to sample.
const MAGNIFIER_CROP: i32 = 21;
/// How much that crop is blown up.
const MAGNIFIER_ZOOM: u32 = 5;
/// Distance from the pointer to the loupe panel.
const MAGNIFIER_OFFSET: f32 = 22.0;
/// How often the physical Shift key is polled while a capture is open.
const SHIFT_POLL: Duration = Duration::from_millis(25);
/// `VK_SHIFT`.
const VK_SHIFT: u8 = 0x10;
/// How long the cached window geometry for the hover highlight is trusted.
///
/// Nothing on screen moves while a capture is open, so this only has to be
/// short enough to notice a window appearing or closing mid-capture.
const HOVER_INDEX_TTL: Duration = Duration::from_millis(400);

/// The loupe shown before a region has been framed: a magnified crop of the
/// frozen screen plus the colour of the pixel at its centre.
struct Magnifier {
    /// Frame pixel the crop is centred on, so the crop is only rebuilt when the
    /// pointer crosses into a different pixel.
    centre: (i32, i32),
    image: Arc<RenderImage>,
}

/// Largest corner radius the slider offers, in logical pixels.
const RADIUS_MAX: f32 = 48.0;

/// The two numbers behind the size badge, while they are being edited.///
/// Numeric only, so the digits are read straight from the key events and no IME
/// or text-input plumbing is involved.
struct SizeEdit {
    width: String,
    height: String,
    /// Which of the two the next digit goes into.
    on_height: bool,
    /// The active field still holds the value it opened with, so the first digit
    /// typed replaces it instead of appending to it — nobody wants to backspace
    /// "500" before typing "300".
    fresh: bool,
}

impl SizeEdit {
    fn open(selection: Rect, map: (f32, f32)) -> Self {
        Self {
            width: format!("{:.0}", selection.w * map.0),
            height: format!("{:.0}", selection.h * map.1),
            on_height: false,
            fresh: true,
        }
    }

    fn field_mut(&mut self) -> &mut String {
        if self.on_height {
            &mut self.height
        } else {
            &mut self.width
        }
    }

    /// Whether this key belongs to the editor, in which case it is consumed.
    fn handle_key(&mut self, key: &str) -> Action2 {
        match key {
            "escape" => return Action2::Close,
            "enter" => return Action2::Apply,
            "tab" => {
                self.on_height = !self.on_height;
                self.fresh = true;
                return Action2::Redraw;
            }
            "backspace" => {
                self.fresh = false;
                self.field_mut().pop();
                return Action2::Redraw;
            }
            _ => {}
        }
        if key.len() == 1 && key.chars().all(|c| c.is_ascii_digit()) {
            if self.fresh {
                self.field_mut().clear();
                self.fresh = false;
            }
            let field = self.field_mut();
            // Four digits is every display there is; more would be a typo.
            if field.len() < 4 {
                field.push_str(key);
            }
            return Action2::Redraw;
        }
        Action2::Redraw
    }
}

/// What the editor wants done with a key press.
enum Action2 {
    Redraw,
    Apply,
    Close,
}

/// Which way an arrow key points.
#[derive(Clone, Copy)]
enum Step {
    Left,
    Right,
    Up,
    Down,
}

impl Step {
    fn from_key(key: &str) -> Option<Self> {
        match key {
            "left" => Some(Step::Left),
            "right" => Some(Step::Right),
            "up" => Some(Step::Up),
            "down" => Some(Step::Down),
            _ => None,
        }
    }
}

/// Moves a rectangle one pixel, kept inside the view.
fn nudge(rect: Rect, step: Step, view_w: f32, view_h: f32) -> Rect {
    let (dx, dy) = match step {
        Step::Left => (-1.0, 0.0),
        Step::Right => (1.0, 0.0),
        Step::Up => (0.0, -1.0),
        Step::Down => (0.0, 1.0),
    };
    Rect::new(
        (rect.x + dx).clamp(0.0, (view_w - rect.w).max(0.0)),
        (rect.y + dy).clamp(0.0, (view_h - rect.h).max(0.0)),
        rect.w,
        rect.h,
    )
}

/// Grows a rectangle one pixel towards `step`, the opposite edge staying put.
fn grow(rect: Rect, step: Step, view_w: f32, view_h: f32) -> Rect {
    match step {
        Step::Left => {
            let x = (rect.x - 1.0).max(0.0);
            Rect::new(x, rect.y, rect.w + (rect.x - x), rect.h)
        }
        Step::Right => Rect::new(rect.x, rect.y, (rect.w + 1.0).min(view_w - rect.x), rect.h),
        Step::Up => {
            let y = (rect.y - 1.0).max(0.0);
            Rect::new(rect.x, y, rect.w, rect.h + (rect.y - y))
        }
        Step::Down => Rect::new(rect.x, rect.y, rect.w, (rect.h + 1.0).min(view_h - rect.y)),
    }
}

pub struct Overlay {
    /// The frozen desktop, kept around for the final GDI composite.
    source: Dib,
    /// The same pixels, ready for GPUI to draw.
    image: Arc<RenderImage>,
    /// Physical size of the captured frame.
    frame_size: (f32, f32),
    /// Physical position of the captured frame's top-left corner.
    screen_origin: (f32, f32),
    /// Device pixels per logical pixel, i.e. the display's DPI scale. Used
    /// whenever logical window coordinates have to be expressed as screen
    /// coordinates (positioning a pin, for instance).
    scale: f32,
    /// Device pixels per logical pixel across the *client area*, per axis.
    ///
    /// A window's client area is not always the size we asked for, so the
    /// frozen frame does not map onto it 1:1. Measuring the real ratio keeps
    /// the crop aligned with what the user sees even when that happens.
    map: (f32, f32),
    /// Logical size of the overlay's client area.
    view_size: (f32, f32),
    /// The focus handle the root element tracks.
    focus: FocusHandle,

    selection: Option<Rect>,
    /// The window under the pointer, highlighted before anything is selected.
    hover_rect: Option<Rect>,
    /// The annotation whose resize handles are showing, if any.
    selected_shape: Option<usize>,
    drag: Option<Drag>,
    tool: Option<Tool>,
    color: u32,
    width: f32,
    /// Mosaic brush width and block size.
    brush: f32,
    block: f32,
    /// Eraser width.
    eraser: f32,
    /// Opacity of the mask drawn outside the selection.
    dim: f32,
    /// The pointer currently shown, so it can follow what is under the mouse.
    cursor: Option<CursorStyle>,
    /// Where the pointer is, in logical overlay coordinates.
    ///
    /// Drives both the round brush cursor the painting tools show and the loupe
    /// that is up before a region has been framed.
    pointer: Option<Pt>,
    /// Cached window geometry for the hover highlight, plus when it was taken.
    hover_index: win_util::HoverIndex,
    hover_index_at: Option<std::time::Instant>,
    /// The loupe image, kept until the pointer moves to another pixel.
    magnifier: Option<Magnifier>,
    /// Whether the sampled colour is shown as `#RRGGBB` or `rgb(r, g, b)`.
    colour_hex: bool,
    /// When the sampled colour was last copied, for the little confirmation.
    colour_copied: Option<std::time::Instant>,
    /// Whether the Shift watcher has been started for this window.
    shift_watcher: bool,
    /// Whether the first frame has been drawn yet, for the capture timing log.
    painted: bool,
    /// Whether the real capture has arrived.
    ///
    /// The window is built while the pixels are still being grabbed, and it is
    /// visible from the moment it exists, so until this flips it draws nothing
    /// at all and the desktop shows through. Otherwise the user would be looking
    /// at a black screen for the duration of the grab.
    ready: bool,
    /// Corner radius applied to the finished shot, in logical pixels.
    radius: f32,
    /// Where the radius slider's track sits, so a drag can be turned back into a
    /// value. `(top, height)` in overlay logical pixels.
    radius_track: (f32, f32),
    /// Whether the radius slider is being dragged.
    radius_drag: bool,
    /// The width/height editor opened from the size badge, if it is up.
    size_edit: Option<SizeEdit>,
    /// Last time a live mosaic preview was rebuilt, to keep dragging cheap.
    last_mosaic_at: Option<std::time::Instant>,
    shapes: Vec<Shape>,
    draft: Option<Shape>,
    text: Option<TextEdit>,
    /// Set while a session owns this window.
    ///
    /// The overlay is then a bare frame over the live desktop: no frozen
    /// screenshot, no dimming, none of the tools. Only the frame itself is still
    /// part of the window — the rest of its region is cut away — so every click
    /// that does not land on those few pixels belongs to the application
    /// underneath, which is the whole point of a scrolling shot.
    frame_only: bool,
    /// Set once frames are actually being captured, which stops the frame from
    /// being dragged: a shot that is being stitched has to keep the same
    /// rectangle from beginning to end.
    frame_locked: bool,
    /// While the frame is being dragged: where the cursor started, and where the
    /// selection was at that moment. Both in physical screen pixels.
    frame_drag: Option<((i32, i32), Rect)>,
    /// Set while a Ctrl+drag is being drawn.
    ///
    /// The window is then *told* what to show — by the mouse hook, through the
    /// app state — instead of being asked by a mouse event of its own: the press
    /// belonged to the window underneath, and that window keeps the mouse for the
    /// whole gesture. The tools are out of reach for the same reason, so they are
    /// not drawn while this is set.
    live_drag: bool,
    /// Whether the frame that answers a press has been drawn yet. Only used to
    /// time it (see `timing`), because that frame *is* the feedback the user
    /// waits for.
    answering: bool,
}

/// Which icon a toolbar button shows.
///
/// Every one of them is a glyph in the icon font — the pencil and the text tool
/// used to be stroked by hand because the set had nothing for them, and the font
/// has since grown `编辑` and `文字`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Art {
    Tool(Tool),
    Undo,
    /// Stitch a scrolling window into one tall shot.
    LongShot,
    /// Keep what a session has stitched, and hand it over.
    SessionKeep,
    Pin,
    Save,
    Copy,
    Cancel,
}

impl Art {
    /// A stable element id — the label used to serve as one.
    fn id(self) -> &'static str {
        match self {
            Art::Tool(tool) => tool.label(),
            Art::Undo => "撤销",
            Art::LongShot => "长截图",
            Art::SessionKeep => "复制",
            Art::Pin => "钉图",
            Art::Save => "保存",
            Art::Copy => "复制",
            Art::Cancel => "取消",
        }
    }

    fn element(self, size: f32, color: Rgba) -> AnyElement {
        use crate::ui::icons::{self, glyph};
        match self {
            Art::Tool(Tool::Rect) => icons::icon(glyph::RECT, size, color),
            Art::Tool(Tool::Ellipse) => icons::icon(glyph::CIRCLE, size, color),
            Art::Tool(Tool::Arrow) => icons::icon(glyph::ARROW, size, color),
            Art::Tool(Tool::Pen) => icons::icon(glyph::PEN, size, color),
            Art::Tool(Tool::Mosaic) => icons::icon(glyph::MOSAIC, size, color),
            Art::Tool(Tool::Eraser) => icons::icon(glyph::RUBBER, size, color),
            Art::Tool(Tool::Text) => icons::icon(glyph::TEXT, size, color),
            Art::Undo => icons::icon(glyph::REVOKE, size, color),
            Art::LongShot => icons::icon(glyph::LONG_SHOT, size, color),
            Art::SessionKeep => icons::icon(glyph::COPY, size, color),
            Art::Pin => icons::icon(glyph::PIN, size, color),
            Art::Save => icons::icon(glyph::SAVE, size, color),
            Art::Copy => icons::icon(glyph::COPY, size, color),
            Art::Cancel => icons::icon(glyph::CLOSE, size, color),
        }
    }
}

impl Overlay {
    pub fn new(
        source: Dib,
        screen_origin: (i32, i32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let scale = window.scale_factor();
        let frame_size = (source.width as f32, source.height as f32);
        let image = source.to_render_image();
        // The overlay covers the whole desktop, so the pointer is already on it.
        // Starting from the real cursor position means the loupe and the colour
        // readout are live from the first frame instead of waiting for the user
        // to jiggle the mouse — which matters if C is the first thing they press.
        let (cursor_x, cursor_y) = win_util::cursor_position();
        let pointer = Pt::new(
            (cursor_x - screen_origin.0) as f32 / scale,
            (cursor_y - screen_origin.1) as f32 / scale,
        );
        Self {
            source,
            image,
            frame_size,
            screen_origin: (screen_origin.0 as f32, screen_origin.1 as f32),
            scale,
            map: (scale, scale),
            view_size: (frame_size.0 / scale, frame_size.1 / scale),
            focus: cx.focus_handle(),
            selection: None,
            hover_rect: None,
            selected_shape: None,
            drag: None,
            tool: None,
            color: PALETTE[0],
            width: WIDTHS[1],
            brush: BRUSHES[1],
            block: app::state(cx).config.mosaic_block.clamp(4.0, 48.0),
            eraser: ERASERS[1],
            dim: app::state(cx).config.dim_strength.clamp(0.0, 0.9),
            cursor: Some(CursorStyle::Crosshair),
            pointer: Some(pointer),
            hover_index: win_util::HoverIndex::default(),
            hover_index_at: None,
            magnifier: None,
            colour_hex: true,
            colour_copied: None,
            shift_watcher: false,
            painted: false,
            ready: false,
            radius: app::state(cx).config.corner_radius.clamp(0.0, 96.0),
            radius_track: (0.0, 0.0),
            radius_drag: false,
            size_edit: None,
            last_mosaic_at: None,
            shapes: Vec::new(),
            draft: None,
            text: None,
            frame_only: false,
            frame_locked: false,
            frame_drag: None,
            live_drag: false,
            answering: false,
        }
    }

    pub fn focus_handle(&self) -> FocusHandle {
        self.focus.clone()
    }

    /// Whether the real capture has landed yet. Until it has, this window holds
    /// a one-pixel placeholder and can neither be drawn nor cropped.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Arms this window for a capture whose pixels are not here yet.
    ///
    /// The window comes up before the grab lands and draws what it can without
    /// pixels — the dimming and the selection — because that is what makes the
    /// press feel answered. The frozen desktop follows a frame or two later, over
    /// dimming that is already in place, so the change is barely visible.
    pub fn begin_capture(&mut self, rect: Option<(i32, i32, i32, i32)>, cx: &mut Context<Self>) {
        self.ready = false;
        self.answering = true;
        self.selection = None;
        self.hover_rect = None;
        self.selected_shape = None;
        self.drag = None;
        self.shapes.clear();
        self.draft = None;
        self.text = None;
        self.tool = None;
        self.magnifier = None;
        self.frame_only = false;
        self.frame_locked = false;
        self.frame_drag = None;
        self.live_drag = rect.is_some();
        self.set_live_rect(rect, cx);
    }

    /// Gives the captured pixels back: nothing stale can be drawn, and the 14 MB
    /// image is not held on to between shots.
    ///
    /// The frame size stays as it is — it is the screen, not the image, and the
    /// window is sized from it.
    pub fn clear_source(&mut self) {
        self.ready = false;
        self.live_drag = false;
        self.selection = None;
        self.magnifier = None;
        self.pointer = None;
        if let Ok(placeholder) = Dib::new(1, 1) {
            self.source = placeholder;
            self.image = self.source.to_render_image();
        }
    }

    /// Draws the rectangle a Ctrl+drag has reached, in physical screen pixels.
    ///
    /// The gesture itself never arrives here: the press went to the window
    /// underneath, which owns the drag and keeps the mouse until the button comes
    /// up. So this window is told what to draw rather than asked.
    pub fn set_live_rect(&mut self, rect: Option<(i32, i32, i32, i32)>, cx: &mut Context<Self>) {
        let selection = rect.map(|(x, y, w, h)| {
            Rect::new(
                (x as f32 - self.screen_origin.0) / self.scale,
                (y as f32 - self.screen_origin.1) / self.scale,
                w as f32 / self.scale,
                h as f32 / self.scale,
            )
        });
        self.selection = selection;
        // The loupe follows the corner the pointer is holding, exactly as it does
        // for a hand-drawn selection — but it needs pixels, so not before the
        // capture has landed.
        if self.ready {
            if let Some(selection) = selection {
                self.pointer = Some(Pt::new(selection.right(), selection.bottom()));
                self.refresh_magnifier();
            }
        }
        cx.notify();
    }

    /// The Ctrl+drag's button came up: pin what it drew, and close.
    pub fn finish_live_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.live_drag = false;
        self.finish(Finish::Pin, window, cx);
    }

    /// Installs a fresh capture into a window that was built ahead of it.
    ///
    /// The window is created while the pixels are still being grabbed, so it
    /// starts out holding a placeholder; everything that belongs to a capture is
    /// set here, and anything the placeholder frame may have accumulated while
    /// it was invisible is dropped.
    pub fn set_source(
        &mut self,
        source: Dib,
        screen_origin: (f32, f32),
        image: Arc<RenderImage>,
    ) {
        self.frame_size = (source.width as f32, source.height as f32);
        self.source = source;
        self.image = image;
        self.screen_origin = screen_origin;
        self.view_size = (self.frame_size.0 / self.scale, self.frame_size.1 / self.scale);
        self.map = (self.scale, self.scale);
        self.selection = None;
        self.hover_rect = None;
        self.selected_shape = None;
        self.drag = None;
        self.shapes.clear();
        self.draft = None;
        self.text = None;
        self.magnifier = None;
        self.hover_index_at = None;
        self.colour_copied = None;
        // A session's frame state belongs to the session that set it; a window
        // handed fresh pixels is framing again, whatever it was doing before.
        self.frame_only = false;
        self.frame_locked = false;
        self.frame_drag = None;
        self.ready = true;
        crate::trace_log!(
            "xshot: overlay source installed frame={}x{}",
            self.frame_size.0,
            self.frame_size.1
        );
    }

    // -- geometry helpers -------------------------------------------------

    fn view_size(&self) -> (f32, f32) {
        self.view_size
    }

    fn local(&self, position: Point<Pixels>) -> Pt {
        // GPUI already reports mouse positions in logical pixels, which is the
        // space annotations live in.
        Pt::new(position.x.as_f32(), position.y.as_f32())
    }

    /// A shape's stored coordinates converted to physical pixels for the
    /// compositor.
    fn composite_offset(&self, origin_device: (i32, i32)) -> (f32, f32) {
        (-(origin_device.0 as f32), -(origin_device.1 as f32))
    }

    /// Everything that should be previewed, including the in-progress draft.
    fn preview_shapes(&self) -> Vec<Shape> {
        let mut shapes = self.shapes.clone();
        if let Some(draft) = &self.draft {
            shapes.push(draft.clone());
        }
        shapes
    }

    // -- sessions ---------------------------------------------------------

    /// Hands the framed region to a long-running session.
    ///
    /// The overlay survives: the session reduces it to a bare frame, so the user
    /// can see exactly what is being captured while driving the window
    /// underneath, and an aborted one can put the tools back.
    fn start_session(&mut self, cx: &mut Context<Self>, kind: Session) {
        let Some(region) = self.selection.map(|_| self.screen_region()) else {
            return;
        };
        match kind {
            Session::Scrolling => {
                // The frame is locked straight away: a scrolling shot has to
                // keep the same rectangle from beginning to end.
                self.set_frame_only(true);
                self.set_frame_locked(true);
                crate::capture::start_scrolling(cx, region);
            }
        }
    }

    /// Switches between normal framing and the stripped-down frame a session
    /// leaves on screen.
    pub fn set_frame_only(&mut self, frame_only: bool) {        if self.frame_only == frame_only {
            return;
        }
        self.frame_only = frame_only;
        if !frame_only {
            // The tools come back with the frozen desktop, and so does the
            // pointer the loupe follows.
            self.magnifier = None;
            self.pointer = None;
            self.frame_locked = false;
        }
        self.frame_drag = None;
        self.apply_frame_region();
    }

    /// Stops the frame being dragged, and thins it down.
    pub fn set_frame_locked(&mut self, locked: bool) {
        if self.frame_locked == locked {
            return;
        }
        self.frame_locked = locked;
        self.frame_drag = None;
        self.apply_frame_region();
    }

    /// How thick the frame is drawn, in logical pixels.
    fn frame_thickness(&self) -> f32 {
        FRAME_THICKNESS
    }

    /// Where the session's own toolbar sits, in logical pixels.
    ///
    /// The same place the framing toolbar uses, so the buttons are where the
    /// pointer already is: under the region's right edge, or above it when there
    /// is no room below.
    fn session_toolbar_rect(&self) -> Option<Rect> {
        let selection = self.selection?;
        let (view_w, view_h) = self.view_size();
        let (width, height) = SESSION_TOOLBAR_SIZE;
        let below = selection.bottom() + 8.0;
        let top = if below + height <= view_h {
            below
        } else {
            (selection.y - height - 8.0).max(0.0)
        };
        let left = (selection.right() - width).clamp(0.0, (view_w - width).max(0.0));
        Some(Rect::new(left, top, width, height))
    }

    /// The four bands the frame occupies, in the window's client coordinates.
    ///
    /// They are drawn in logical pixels but a window region is in physical ones,
    /// hence the map — which is exactly the ratio between the two.
    ///
    /// The outline sits [`FRAME_GAP`] away from the region rather than against
    /// it, which is what keeps it visible: see [`Self::apply_frame_region`].
    fn frame_bands(&self) -> Vec<(i32, i32, i32, i32)> {
        let Some(selection) = self.selection else {
            return Vec::new();
        };
        let (map_x, map_y) = self.map;
        let t = (self.frame_thickness() * map_x).round().max(1.0) as i32;
        // The outline's own rectangle: the region grown by the gap.
        let x = ((selection.x - FRAME_GAP) * map_x).round() as i32;
        let y = ((selection.y - FRAME_GAP) * map_y).round() as i32;
        let w = ((selection.w + FRAME_GAP * 2.0) * map_x).round() as i32;
        let h = ((selection.h + FRAME_GAP * 2.0) * map_y).round() as i32;
        vec![
            (x - t, y - t, w + t * 2, t),
            (x - t, y + h, w + t * 2, t),
            (x - t, y - t, t, h + t * 2),
            (x + w, y - t, t, h + t * 2),
        ]
    }

    /// Hands the window only the frame and the session's toolbar, and gives up
    /// everything else.
    ///
    /// A window's region is what the mouse can see of it, so this is what lets
    /// the desktop — and the taskbar, and any other window — be clicked while the
    /// frame stays on screen. The toolbar has to be in here too, or its own
    /// buttons would be unclickable for the same reason.
    fn apply_frame_region(&self) {
        let mut rects = if self.frame_only {
            self.frame_bands()
        } else {
            Vec::new()
        };
        if self.frame_only {
            // A couple of pixels of slack around each band, so the region cannot
            // clip the frame it is drawn around.
            let (map_x, map_y) = self.map;
            let pad_x = (REGION_SLACK * map_x).round() as i32;
            let pad_y = (REGION_SLACK * map_y).round() as i32;
            for rect in &mut rects {
                let (x, y, w, h) = *rect;
                *rect = (x - pad_x, y - pad_y, w + pad_x * 2, h + pad_y * 2);
            }
            if let Some(toolbar) = self.session_toolbar_rect() {
                rects.push((
                    (toolbar.x * map_x).round() as i32,
                    (toolbar.y * map_y).round() as i32,
                    (toolbar.w * map_x).round().max(1.0) as i32,
                    (toolbar.h * map_y).round().max(1.0) as i32,
                ));
            }
        }
        win_util::set_window_region(app::OVERLAY_TITLE, &rects);
    }

    /// The two buttons a session keeps.
    ///
    /// Everything else — the tools, the colours, the finishing buttons — would
    /// either change a region that is already being captured or end the session
    /// in a way that has no meaning while it is still collecting. What is left is
    /// the pair the user needs: keep what has been stitched, or throw it away.
    fn session_toolbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rect = self.session_toolbar_rect()?;
        let button = |art: Art, action: Action, cx: &mut Context<Self>| -> AnyElement {
            div()
                .id(art.id())
                .flex()
                .items_center()
                .justify_center()
                .w(px(28.0))
                .h(px(24.0))
                .rounded(px(4.0))
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x3C3C3C)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        this.apply(action, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(art.element(16.0, rgb(0xD8D8D8)))
                .into_any_element()
        };
        Some(
            div()
                .id("xshot-session-toolbar")
                .absolute()
                .left(px(rect.x))
                .top(px(rect.y))
                .w(px(rect.w))
                .h(px(rect.h))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(6.0))
                .rounded(px(7.0))
                .bg(rgba(0x1E1E_1EF2))
                .border_1()
                .border_color(rgba(0xFFFF_FF26))
                .shadow_lg()
                .child(button(Art::SessionKeep, Action::SessionKeep, cx))
                .child(button(Art::Cancel, Action::SessionDiscard, cx))
                .into_any_element(),
        )
    }

    /// Starts dragging the frame to somewhere else on screen.
    fn start_frame_drag(&mut self) {
        if self.frame_locked {
            return;
        }
        if let Some(selection) = self.selection {
            self.frame_drag = Some((win_util::cursor_position(), selection));
        }
    }

    /// Moves the frame with the cursor, keeping it on the desktop.
    fn drag_frame(&mut self, cx: &mut Context<Self>) {
        let Some((start_cursor, start_selection)) = self.frame_drag else {
            return;
        };
        let now = win_util::cursor_position();
        let (map_x, map_y) = self.map;
        let dx = (now.0 - start_cursor.0) as f32 / map_x.max(0.0001);
        let dy = (now.1 - start_cursor.1) as f32 / map_y.max(0.0001);
        let (view_w, view_h) = self.view_size();
        let mut moved = start_selection;
        moved.x = (start_selection.x + dx).clamp(0.0, (view_w - moved.w).max(0.0));
        moved.y = (start_selection.y + dy).clamp(0.0, (view_h - moved.h).max(0.0));
        self.selection = Some(moved);
        self.apply_frame_region();
        // The region a session would use if it started now has to follow, but
        // nothing else moves on every pixel: repositioning the toolbar means
        // resizing a window and asking the toolkit for its entity again, and
        // doing that per mouse sample is what makes the frame feel stuck.
        crate::capture::set_region(self.screen_region());
        cx.notify();
    }

    /// Ends a frame drag.
    fn end_frame_drag(&mut self, cx: &mut Context<Self>) {
        if self.frame_drag.take().is_none() {
            return;
        }
        let region = self.screen_region();
        crate::capture::set_region(region);
        // Once, on release: the toolbar travels with the frame.
        crate::ui::record_bar::reposition(cx, region);
        cx.notify();
    }

    /// The framed region in physical screen pixels.
    fn screen_region(&self) -> (i32, i32, i32, i32) {
        let Some(selection) = self.selection else {
            return (0, 0, 1, 1);
        };
        let (map_x, map_y) = self.map;
        let origin = self.screen_origin;
        (
            (origin.0 + selection.x * map_x).round() as i32,
            (origin.1 + selection.y * map_y).round() as i32,
            (selection.w * map_x).round().max(1.0) as i32,
            (selection.h * map_y).round().max(1.0) as i32,
        )
    }

    /// The frame drawn around a session's region.
    ///
    /// It sits entirely *outside* the region: the region is exactly what gets
    /// captured, so a border drawn on its edge — the way the selection outline
    /// is while framing — would appear in every frame of the result. It is also
    /// held [`FRAME_GAP`] away from the region, which is where the window's own
    /// region stops.
    fn session_frame(&self) -> Option<AnyElement> {
        let selection = self.selection?;
        let thickness = self.frame_thickness();
        let accent = rgb(0xFF5252);
        let edge = |x: f32, y: f32, w: f32, h: f32| -> AnyElement {
            div()
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(w))
                .h(px(h))
                .bg(accent)
                .into_any_element()
        };
        let x = selection.x - FRAME_GAP;
        let y = selection.y - FRAME_GAP;
        let w = selection.w + FRAME_GAP * 2.0;
        let h = selection.h + FRAME_GAP * 2.0;
        Some(
            div()
                .absolute()
                .left_0()
                .top_0()
                .w_full()
                .h_full()
                .child(edge(x - thickness, y - thickness, thickness, h + thickness * 2.0))
                .child(edge(x + w, y - thickness, thickness, h + thickness * 2.0))
                .child(edge(x - thickness, y - thickness, w + thickness * 2.0, thickness))
                .child(edge(x - thickness, y + h, w + thickness * 2.0, thickness))
                .into_any_element(),
        )
    }

    // -- text handling ----------------------------------------------------

    fn begin_text(&mut self, origin: Pt) {
        self.text = Some(TextEdit {
            origin,
            text: String::new(),
            selection: 0..0,
            marked: None,
        });
    }

    /// Turns the in-progress text into a committed annotation.
    fn commit_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = self.text.take() else {
            return;
        };
        let _ = window;
        if edit.text.trim().is_empty() {
            return;
        }
        self.shapes.push(Shape::Text {
            pos: edit.origin,
            text: edit.text,
            color: self.color,
            size: 20.0,
        });
        cx.notify();
    }

    fn backspace(&mut self) {
        let Some(edit) = self.text.as_mut() else {
            return;
        };
        let (start, end) = if !edit.selection.is_empty() {
            (
                utf8_index_for_utf16(&edit.text, edit.selection.start),
                utf8_index_for_utf16(&edit.text, edit.selection.end),
            )
        } else if let Some(marked) = edit.marked.clone() {
            (
                utf8_index_for_utf16(&edit.text, marked.start),
                utf8_index_for_utf16(&edit.text, marked.end),
            )
        } else {
            let caret = utf8_index_for_utf16(&edit.text, edit.selection.start);
            let previous = edit.text[..caret]
                .char_indices()
                .next_back()
                .map(|(index, _)| index)
                .unwrap_or(0);
            (previous, caret)
        };
        edit.text.replace_range(start..end, "");
        let caret = utf16_index_for_utf8(&edit.text, start);
        edit.selection = caret..caret;
        edit.marked = None;
    }

    /// Replaces `target` (or the mark, or the selection) with `new_text`.
    fn splice(&mut self, range: Option<Range<usize>>, new_text: &str, mark: bool, new_selection: Option<Range<usize>>) {
        let Some(edit) = self.text.as_mut() else {
            return;
        };
        let target = range
            .or_else(|| edit.marked.clone())
            .unwrap_or_else(|| edit.selection.clone());
        let start = utf8_index_for_utf16(&edit.text, target.start);
        let end = utf8_index_for_utf16(&edit.text, target.end);
        edit.text.replace_range(start..end, new_text);

        let inserted_start = utf16_index_for_utf8(&edit.text, start);
        let inserted_end = inserted_start + utf16_len(new_text);

        if mark {
            edit.marked = Some(inserted_start..inserted_end);
            edit.selection = match new_selection {
                Some(range) => (inserted_start + range.start)..(inserted_start + range.end),
                None => inserted_end..inserted_end,
            };
        } else {
            edit.marked = None;
            edit.selection = inserted_end..inserted_end;
        }
    }

    // -- committing -------------------------------------------------------

    fn finish(&mut self, mode: Finish, window: &mut Window, cx: &mut Context<Self>) {
        crate::trace_log!(
            "xshot: finish {mode:?} selection={:?} shapes={}",
            self.selection.map(|s| (s.x, s.y, s.w, s.h)),
            self.shapes.len()
        );
        self.commit_text(window, cx);

        let Some(selection) = self.selection else {
            // Nothing was selected: treat the gesture as "dismiss".
            self.cancel(window, cx);
            return;
        };

        let map = self.map;
        let origin_x = (selection.x * map.0).round().clamp(0.0, self.frame_size.0 - 1.0) as i32;
        let origin_y = (selection.y * map.1).round().clamp(0.0, self.frame_size.1 - 1.0) as i32;
        let width = ((selection.w * map.0).round() as i32).clamp(1, self.source.width - origin_x);
        let height = ((selection.h * map.1).round() as i32).clamp(1, self.source.height - origin_y);

        let Ok(mut output) = self.source.crop(origin_x, origin_y, width, height) else {
            self.end_session("finish: the crop failed", window, cx);
            return;
        };
        let offset = self.composite_offset((origin_x, origin_y));
        paint::draw_shapes(&mut output, &self.shapes, map, offset);
        // Rounding happens last, on the composited shot: it is the shape of the
        // file, not a decoration on the annotations.
        paint::round_corners(&mut output, (self.radius * map.0).round() as i32);

        match mode {
            Finish::Copy => {
                if let Err(error) = clip::copy_image(&output) {
                    eprintln!("xshot: clipboard write failed: {error:#}");
                }
            }
            Finish::Save => {
                // Ask where it goes. The overlay stays open behind the dialog so
                // that cancelling puts the user back where they were.
                crate::ui::save_dialog::open(cx, output);
                return;
            }
            Finish::Pin => {
                // The pin should land exactly where the selection sat on screen.
                let position = (
                    (self.screen_origin.0 + origin_x as f32) / self.scale,
                    (self.screen_origin.1 + origin_y as f32) / self.scale,
                );
                app::open_pin(cx, output, position, self.scale);
            }
        }

        // Same story as `cancel`: 复制 and 钉图 act on the press, so the release
        // belongs to a window that is about to disappear.
        self.end_session("finish", window, cx);
    }

    /// Ends the capture: the release, the capture, the foreground, the window.
    ///
    /// Every close of this window goes through here, because every one of them
    /// is triggered by a press on the window itself:
    ///
    /// * the press is still held, so its release would land on whatever is
    ///   behind and count as a click there — a right click meant to cancel ends
    ///   up opening the context menu of the window behind;
    /// * GPUI took the mouse capture on that press, and only the matching
    ///   release gives it back;
    /// * the window itself must be off screen the moment this returns. It is kept
    ///   for the next capture, so `remove_window` is not the answer either way:
    ///   [`crate::app::hide_overlay`] hides it through Win32 and waits for the
    ///   compositor to drop the last frame, and empties it so nothing stale can be
    ///   drawn. A full-screen window that is still there while showing nothing is
    ///   what makes clicks do nothing anywhere until another click lands on it.
    fn end_session(&mut self, caller: &str, _window: &mut Window, cx: &mut Context<Self>) {
        tray::swallow_next_button_up(caller);
        // GPUI took the capture on that press, and the release it needs is the
        // one we just arranged to eat. Hand the capture back here, or the next
        // click anywhere lands on a window that no longer exists.
        win_util::release_capture();
        app::restore_foreground(cx);
        app::state_mut(cx).drag = None;
        self.live_drag = false;
        // The window is hidden and emptied rather than dropped: it is reused for
        // every capture, and building it is the most expensive step there is.
        app::hide_overlay(cx);
    }

    pub fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The click that got here is still held down; its release has to be
        // eaten, or it lands on whatever window is behind and counts as a click
        // there — which is how a right click meant to cancel a screenshot ends
        // up opening the context menu of the window behind it.
        self.end_session("cancel", window, cx);
    }

    fn apply(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        match action {
            // Clicking the armed tool again disarms it, returning to selection
            // editing.
            Action::Arm(tool) => self.arm(tool, window, cx),
            Action::Stroke(color) => self.color = color,
            Action::Weight(width) => self.width = width,
            Action::Brush(brush) => self.brush = brush,
            Action::EraserSize(size) => self.eraser = size,
            Action::Block(block) => self.block = block,
            Action::Undo => {
                self.shapes.pop();
            }
            Action::LongShot => self.start_session(cx, Session::Scrolling),
            // Both of these act on the press, and the session's worker picks the
            // answer up on its next tick — it owns the rows, so it is the one
            // that can hand them over or drop them.
            Action::SessionKeep => crate::capture::finish_with(crate::capture::Ending::Copy),
            Action::SessionDiscard => crate::capture::finish_with(crate::capture::Ending::Discard),
            Action::Copy => self.finish(Finish::Copy, window, cx),
            Action::Save => self.finish(Finish::Save, window, cx),
            Action::Pin => self.finish(Finish::Pin, window, cx),
            Action::Cancel => self.cancel(window, cx),
        }
        cx.notify();
    }

    // -- input ------------------------------------------------------------

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A bare frame has exactly one gesture: move the frame. Only the frame's
        // own pixels are part of this window, so this is the only place a press
        // can land.
        if self.frame_only {
            if event.button == MouseButton::Left {
                self.start_frame_drag();
                cx.notify();
            }
            return;
        }

        let position = self.local(event.position);
        let (view_w, view_h) = self.view_size();
        // A press without a preceding move still has to place the loupe and the
        // brush cursor, or the very first click of a session would show neither.
        self.pointer = Some(position);
        self.refresh_magnifier();

        if event.button == MouseButton::Right {
            // Right click backs out one level — and all of it happens on the
            // *release*, in `on_mouse_up`: closing the window here would leave
            // the matching release to the window behind, which is how a right
            // click meant to cancel a screenshot ends up opening the context
            // menu of whatever was under it.
            return;
        }

        if event.button != MouseButton::Left {
            return;
        }

        if self.text.is_some() {
            self.commit_text(window, cx);
        }

        // Handles are checked before the armed tool, so a selection can still
        // be resized after drawing something — the tool stays armed by design,
        // and otherwise every press near an edge would just draw another shape.
        // A selected annotation's own handles come first: they sit on top.
        if let Some((index, handle)) = self.shape_handles_at(position) {
            let original = self.shapes[index].bounds();
            self.drag = Some(Drag::ResizingShape { index, handle, original });
            cx.notify();
            return;
        }
        if let Some(selection) = self.selection {
            if let Some(handle) = hit_handle(position, &selection) {
                self.drag = Some(Drag::Resizing { handle, original: selection });
                cx.notify();
                return;
            }
        }

        // Picking up existing ink also beats drawing, for the same reason.
        if let Some(index) = self.shape_ink_at(position) {
            self.selected_shape = Some(index);
            self.drag = Some(Drag::MovingShape { index, last: position });
            win_util::show_move_cursor();
            cx.notify();
            return;
        }

        if let Some(tool) = self.tool {
            match tool {
                Tool::Text => self.begin_text(position),
                Tool::Eraser => {
                    // The eraser has no shape of its own: it removes whatever
                    // it is dragged across.
                    self.erase_at(position, position);
                    self.drag = Some(Drag::Erasing { last: position });
                }
                Tool::Rect => {
                    self.draft = Some(Shape::Rect {
                        a: position,
                        b: position,
                        color: self.color,
                        width: self.width,
                    });
                    self.drag = Some(Drag::Drawing);
                }
                Tool::Ellipse => {
                    self.draft = Some(Shape::Ellipse {
                        a: position,
                        b: position,
                        color: self.color,
                        width: self.width,
                    });
                    self.drag = Some(Drag::Drawing);
                }
                Tool::Arrow => {
                    self.draft = Some(Shape::Arrow {
                        a: position,
                        b: position,
                        color: self.color,
                        width: self.width,
                    });
                    self.drag = Some(Drag::Drawing);
                }
                Tool::Pen => {
                    self.draft = Some(Shape::Pen {
                        points: vec![position],
                        color: self.color,
                        width: self.width,
                    });
                    self.drag = Some(Drag::Drawing);
                }
                Tool::Mosaic => {
                    self.last_mosaic_at = None;
                    self.draft = Some(Shape::Mosaic {
                        points: vec![position],
                        brush: self.brush,
                        block: self.block,
                        preview: None,
                    });
                    self.drag = Some(Drag::Drawing);
                }
            }
            cx.notify();
            return;
        }

        if let Some(selection) = self.selection {
            // An annotation under the pointer is grabbed before the selection
            // itself: shapes are drawn inside the selection, so this is what
            // makes "pick the rectangle up and move it" work.
            if let Some(index) = self.shape_box_at(position) {
                self.selected_shape = Some(index);
                self.drag = Some(Drag::MovingShape { index, last: position });
                win_util::show_move_cursor();
                cx.notify();
                return;
            }
            if selection.contains(position) {
                self.selected_shape = None;
                self.drag = Some(Drag::Moving { grab: position, original: selection });
                cx.notify();
                return;
            }
        }

        // A press on empty space accepts the window the pointer was lighting
        // up; dragging from there replaces it with a hand-drawn region.
        self.selected_shape = None;
        self.selection = self
            .hover_rect
            .or(Some(Rect::new(position.x, position.y, 0.0, 0.0)));
        self.drag = Some(Drag::Creating { anchor: position });
        let _ = (view_w, view_h);
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.frame_only {
            self.drag_frame(cx);
            return;
        }

        let position = self.local(event.position);
        let (view_w, view_h) = self.view_size();

        // The loupe and the round brush cursor follow every move, whether the
        // user is painting, framing or just looking around.
        self.pointer = Some(position);
        self.refresh_magnifier();

        // The pointer is one thing while the radius slider is being dragged; the
        // selection must not move along with it.
        if self.radius_drag {
            self.drag_radius(event.position.y, cx);
            return;
        }

        let Some(drag) = self.drag.clone() else {
            // Idle: keep the pointer honest about what is under it — a resize
            // arrow over a handle, a move pointer over something draggable —
            // and light up the window the user is pointing at.
            self.detect_hover(cx);
            self.update_cursor(position, cx);
            cx.notify();
            return;
        };

        match drag {
            Drag::Creating { anchor } => {
                self.selection = Some(Rect::from_corners(anchor, position).clamp(view_w, view_h));
            }
            Drag::Moving { grab, original } => {
                let dx = position.x - grab.x;
                let dy = position.y - grab.y;
                let moved = Rect::new(original.x + dx, original.y + dy, original.w, original.h);
                // Clamp by shifting instead of cropping so the size is kept.
                let x = moved.x.clamp(0.0, (view_w - original.w).max(0.0));
                let y = moved.y.clamp(0.0, (view_h - original.h).max(0.0));
                self.selection = Some(Rect::new(x, y, original.w, original.h));
            }
            Drag::Resizing { handle, original } => {
                self.selection = Some(resize(original, handle, position, view_w, view_h));
            }
            Drag::Drawing => match self.draft.as_mut() {
                Some(Shape::Rect { b, .. })
                | Some(Shape::Ellipse { b, .. })
                | Some(Shape::Arrow { b, .. }) => *b = position,
                Some(Shape::Pen { points, .. }) | Some(Shape::Mosaic { points, .. }) => {
                    if points
                        .last()
                        .map(|last| (last.x - position.x).abs() + (last.y - position.y).abs() > 1.5)
                        .unwrap_or(true)
                    {
                        points.push(position);
                    }
                }
                _ => {}
            },
            Drag::Erasing { last } => {
                self.erase_at(last, position);
                self.drag = Some(Drag::Erasing { last: position });
            }
            Drag::MovingShape { index, last } => {
                let (dx, dy) = (position.x - last.x, position.y - last.y);
                if let Some(shape) = self.shapes.get_mut(index) {
                    shape.translate(dx, dy);
                }
                self.drag = Some(Drag::MovingShape { index, last: position });
            }
            Drag::ResizingShape { index, handle, original } => {
                let next = resize(original, handle, position, view_w, view_h);
                if let Some(shape) = self.shapes.get_mut(index) {
                    if let Shape::Rect { a, b, .. } | Shape::Ellipse { a, b, .. } = shape {
                        *a = Pt::new(next.x, next.y);
                        *b = Pt::new(next.right(), next.bottom());
                    }
                }
                self.drag = Some(Drag::ResizingShape { index, handle, original });
            }
        }

        // Keep the pointer consistent with what the drag is doing: a closed
        // hand while carrying an annotation, the resize arrow while pulling a
        // handle.
        // Keep the pointer consistent with what the drag is doing: the move
        // pointer while carrying an annotation, the resize arrow while pulling
        // a handle.
        match &self.drag {
            Some(Drag::MovingShape { .. }) => {
                if self.cursor != Some(CursorStyle::Arrow) {
                    self.cursor = Some(CursorStyle::Arrow);
                    cx.notify();
                }
                win_util::show_move_cursor();
            }
            Some(Drag::ResizingShape { handle, .. }) => {
                let want = Some(handle.cursor());
                if self.cursor != want {
                    self.cursor = want;
                    cx.notify();
                }
            }
            Some(Drag::Resizing { handle, .. }) => {
                let want = Some(handle.cursor());
                if self.cursor != want {
                    self.cursor = want;
                    cx.notify();
                }
            }
            _ => {}
        }

        // Rebuilding the mosaic bitmap is the one expensive part of a freehand
        // mosaic, so it is rate-limited rather than done on every mouse sample.
        if matches!(self.draft, Some(Shape::Mosaic { .. })) {
            let due = self
                .last_mosaic_at
                .map(|at| at.elapsed().as_millis() >= 55)
                .unwrap_or(true);
            if due {
                self.refresh_mosaic_preview();
                self.last_mosaic_at = Some(std::time::Instant::now());
            }
        }
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.frame_only {
            self.end_frame_drag(cx);
            return;
        }
        // Gestures that *end* the session run here rather than on the press: a
        // window that closes on its press leaves the matching release in the
        // air, and Windows hands that release to whatever is behind — which
        // reads it as a click of its own. On the release, the button is already
        // spent on this window and nothing else sees it.
        if event.button == MouseButton::Right {
            // Right click backs out one level: first the tool, then the overlay.
            if self.text.is_some() {
                self.text = None;
            } else if self.tool.is_some() {
                self.tool = None;
            } else {
                self.cancel(window, cx);
                return;
            }
            cx.notify();
            return;
        }
        if event.button != MouseButton::Left {
            return;
        }
        // Double-clicking inside the selection finishes the shot, exactly as it
        // does in QQ's tool. It is also the only finish gesture that is
        // instantaneous: GPUI holds the Enter key back briefly to see whether
        // it starts a multi-key binding, but mouse events are never delayed.
        //
        // The text tool is included because a label is never entered by
        // double-clicking, and leaving it armed after typing is the common
        // case.
        if event.click_count >= 2 && matches!(self.tool, None | Some(Tool::Text)) {
            let position = self.local(event.position);
            if let Some(selection) = self.selection {
                if selection.contains(position) {
                    self.drag = None;
                    let mode = default_finish(cx);
                    self.finish(mode, window, cx);
                    return;
                }
            }
        }
        if self.radius_drag {
            self.radius_drag = false;
            cx.notify();
            return;
        }
        let position = self.local(event.position);
        let Some(drag) = self.drag.take() else {
            return;
        };
        self.update_cursor(position, cx);

        match drag {
            Drag::Creating { .. } => {
                if let Some(selection) = self.selection {
                    if selection.w < MIN_SELECTION || selection.h < MIN_SELECTION {
                        self.selection = None;
                    }
                }
            }
            Drag::Drawing => {
                if let Some(draft) = self.draft.take() {
                    let keep = match &draft {
                        Shape::Rect { a, b, .. } | Shape::Ellipse { a, b, .. } => {
                            (a.x - b.x).abs() >= 2.0 && (a.y - b.y).abs() >= 2.0
                        }
                        Shape::Arrow { a, b, .. } => {
                            (a.x - b.x).abs() >= 2.0 || (a.y - b.y).abs() >= 2.0
                        }
                        Shape::Pen { points, .. } | Shape::Mosaic { points, .. } => {
                            !points.is_empty()
                        }
                        Shape::Text { .. } => true,
                    };
                    if keep {
                        self.shapes.push(draft);
                    }
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Highlights the window under the pointer until a selection is started.
    ///
    /// This is the "point at a window and it lights up" behaviour every
    /// screenshot tool has: the user does not have to trace a window's edges by
    /// hand, and clicking takes the whole window.
    fn detect_hover(&mut self, cx: &mut Context<Self>) {
        if self.selection.is_some() || self.tool.is_some() {
            if self.hover_rect.take().is_some() {
                cx.notify();
            }
            return;
        }
        // The window list is collected at most a couple of times a second and
        // hit-tested in memory. Enumerating every top-level window — and asking
        // DWM for each one's real bounds — on every mouse sample is what made
        // the pointer crawl when it was over the desktop: with no window under
        // it the walk never short-circuits, so each sample paid for the whole
        // window list.
        let due = self
            .hover_index_at
            .map(|at| at.elapsed() >= HOVER_INDEX_TTL)
            .unwrap_or(true);
        if due {
            self.hover_index = win_util::HoverIndex::build();
            self.hover_index_at = Some(std::time::Instant::now());
        }

        let next = self
            .pointer
            .map(|pointer| self.screen_position(pointer))
            .and_then(|(x, y)| self.hover_index.hit(x, y))
            .and_then(|(x, y, w, h)| {
                let (origin_x, origin_y) = self.screen_origin;
                let (map_x, map_y) = self.map;
                let rect = Rect::new(
                (x as f32 - origin_x) / map_x,
                (y as f32 - origin_y) / map_y,
                w as f32 / map_x,
                h as f32 / map_y,
            );
            (rect.w > 8.0 && rect.h > 8.0).then_some(rect)
        });
        if next != self.hover_rect {
            self.hover_rect = next;
            cx.notify();
        }
    }

    /// The eight handles of the currently selected annotation, if it has any.
    fn shape_handles_at(&self, position: Pt) -> Option<(usize, Handle)> {
        let index = self.selected_shape?;
        let shape = self.shapes.get(index)?;
        if !matches!(shape, Shape::Rect { .. } | Shape::Ellipse { .. }) {
            return None;
        }
        hit_handle(position, &shape.bounds()).map(|handle| (index, handle))
    }

    /// The topmost annotation whose *ink* is under `point`.
    ///
    /// Grabbing by the drawn line itself is checked before any armed tool, so a
    /// shape can be picked up straight after drawing it — the tool stays armed
    /// on purpose, and otherwise the press would just start another shape.
    fn shape_ink_at(&self, point: Pt) -> Option<usize> {
        self.shapes
            .iter()
            .rposition(|shape| shape.touched_by(point, point, GRAB_SLACK))
    }

    /// The topmost annotation whose box contains `point`.
    ///
    /// Only consulted when no tool is armed: it lets a hollow rectangle be
    /// picked up from its empty middle as well as from its outline.
    fn shape_box_at(&self, point: Pt) -> Option<usize> {
        self.shapes
            .iter()
            .rposition(|shape| shape.grabbed_by(point, GRAB_SLACK))
    }

    /// What the pointer should look like at `position`.
    ///
    /// Returns `None` when the pointer should be the four-way move cursor,
    /// which the toolkit cannot express and is set through Win32 instead.
    fn cursor_for(&self, position: Pt) -> Option<CursorStyle> {
        if let Some((_, handle)) = self.shape_handles_at(position) {
            return Some(handle.cursor());
        }
        if let Some(selection) = self.selection {
            if let Some(handle) = hit_handle(position, &selection) {
                return Some(handle.cursor());
            }
        }
        if self.shape_ink_at(position).is_some()
            || (self.tool.is_none() && self.shape_box_at(position).is_some())
        {
            return None;
        }
        Some(CursorStyle::Crosshair)
    }

    /// Applies [`Self::cursor_for`], falling back to the Win32 move pointer.
    fn update_cursor(&mut self, position: Pt, cx: &mut Context<Self>) {
        match self.cursor_for(position) {
            Some(style) => {
                if self.cursor != Some(style) {
                    self.cursor = Some(style);
                    cx.notify();
                }
            }
            None => {
                // Declare the arrow so the toolkit stops fighting us, then ask
                // Windows for the move pointer.
                if self.cursor != Some(CursorStyle::Arrow) {
                    self.cursor = Some(CursorStyle::Arrow);
                    cx.notify();
                }
                win_util::show_move_cursor();
            }
        }
    }

    /// Rubs out whatever the eraser stroke was dragged across.
    ///
    /// Nothing is deleted wholesale: a shape that has an outline is reduced to
    /// the parts of that outline the rubber did not touch, so erasing the middle
    /// of a rectangle leaves its other three sides, and erasing across a pencil
    /// squiggle leaves both ends. Only a shape the rubber never reaches is kept
    /// exactly as it was, which also means it keeps its resize handles.
    fn erase_at(&mut self, from: Pt, to: Pt) {
        let radius = self.eraser / 2.0;
        // Indices are about to change, so the selection cannot survive.
        self.selected_shape = None;
        let mut kept: Vec<Shape> = Vec::with_capacity(self.shapes.len());
        for shape in std::mem::take(&mut self.shapes) {
            // Every shape with an outline — including rectangles, ellipses and
            // arrows, which are outlines just as much as a pencil line is — is
            // cut where the rubber crossed it and survives as pencil pieces.
            if let Some(outline) = crate::annot::outline_of(&shape) {
                let (color, width) = shape.ink();
                if !crate::annot::polyline_touched(&outline, from, to, radius) {
                    kept.push(shape);
                    continue;
                }
                for run in crate::annot::split_pen(&outline, from, to, radius) {
                    kept.push(Shape::Pen { points: run, color, width });
                }
                continue;
            }
            match shape {
                Shape::Pen { points, color, width } => {
                    for run in crate::annot::split_pen(&points, from, to, radius) {
                        kept.push(Shape::Pen { points: run, color, width });
                    }
                }
                Shape::Mosaic { points, brush, block, .. } => {
                    // The mosaic is carved out of the captured pixels, so
                    // dropping the covered samples restores the original image
                    // exactly where the rubber passed.
                    let reach = radius + brush / 2.0;
                    for run in crate::annot::split_pen(&points, from, to, reach) {
                        let preview = self.build_mosaic_preview(&run, brush, block);
                        kept.push(Shape::Mosaic { points: run, brush, block, preview });
                    }
                }
                // Text has no outline to cut, so it goes when the rubber
                // reaches it; a letter-by-letter erase is not worth pretending.
                text @ Shape::Text { .. } => {
                    if !text.touched_by(from, to, radius) {
                        kept.push(text);
                    }
                }
                // `outline_of` covered the rest.
                other => kept.push(other),
            }
        }
        self.shapes = kept;
    }

    /// Rebuilds the live mosaic bitmap for the stroke being painted.
    fn refresh_mosaic_preview(&mut self) {
        let Some(Shape::Mosaic { points, brush, block, .. }) = &self.draft else {
            return;
        };
        let (points, brush, block) = (points.clone(), *brush, *block);
        let preview = self.build_mosaic_preview(&points, brush, block);
        if let Some(Shape::Mosaic { preview: slot, .. }) = self.draft.as_mut() {
            *slot = preview;
        }
    }

    /// Renders the blocky bitmap shown while a mosaic stroke is being painted.
    ///
    /// It is produced from the same pixels, brush and block size the final
    /// composite will use, so what the user sees is what gets saved.
    fn build_mosaic_preview(
        &self,
        points: &[Pt],
        brush: f32,
        block: f32,
    ) -> Option<MosaicPreview> {
        if points.is_empty() {
            return None;
        }
        let (map_x, map_y) = self.map;
        let half = brush / 2.0;
        let logical = crate::annot::bounds_of(points).inflate(half);
        let x0 = (logical.x * map_x).floor().max(0.0) as i32;
        let y0 = (logical.y * map_y).floor().max(0.0) as i32;
        let x1 = (logical.right() * map_x).ceil().min(self.frame_size.0) as i32;
        let y1 = (logical.bottom() * map_y).ceil().min(self.frame_size.1) as i32;
        let (width, height) = (x1 - x0, y1 - y0);
        if width < 2 || height < 2 {
            return None;
        }

        let mut tile = self.source.crop(x0, y0, width, height).ok()?;
        // The stroke stays in screen pixels and the tile declares where its own
        // origin sits in that space, so the block grid here is the same grid the
        // exported image uses no matter where the tile happens to be cropped.
        let frame: Vec<(f32, f32)> = points
            .iter()
            .map(|p| (p.x * map_x, p.y * map_y))
            .collect();
        paint::pixelate_stroke(
            &mut tile,
            &frame,
            (brush * map_x).round().max(2.0) as i32,
            (block * map_x).round().max(2.0) as i32,
            (x0, y0),
        );

        Some(MosaicPreview {
            rect: Rect::new(
                x0 as f32 / map_x,
                y0 as f32 / map_y,
                width as f32 / map_x,
                height as f32 / map_y,
            ),
            image: tile.to_render_image(),
        })
    }

    // -- colour sampling ---------------------------------------------------

    /// The frozen frame's pixel under a logical overlay position.
    fn frame_pixel(&self, position: Pt) -> (i32, i32) {
        let x = (position.x * self.map.0).round() as i32;
        let y = (position.y * self.map.1).round() as i32;
        (
            x.clamp(0, (self.source.width - 1).max(0)),
            y.clamp(0, (self.source.height - 1).max(0)),
        )
    }

    /// The screen colour under a logical overlay position, as `RGB`.
    fn pixel_colour(&self, position: Pt) -> [u8; 3] {
        let (x, y) = self.frame_pixel(position);
        let pixels = self.source.pixels();
        // The DIB is BGRA.
        let index = (y as usize * self.source.width as usize + x as usize) * 4;
        [pixels[index + 2], pixels[index + 1], pixels[index]]
    }

    /// The sampled colour written the way the user currently wants to read it.
    fn colour_text(&self, colour: [u8; 3]) -> String {
        if self.colour_hex {
            format!("#{:02X}{:02X}{:02X}", colour[0], colour[1], colour[2])
        } else {
            format!("rgb({}, {}, {})", colour[0], colour[1], colour[2])
        }
    }

    /// The colours as they appear on *screen*, not in the overlay's logical
    /// space: the readout is only useful if it matches what other programs call
    /// the pixel.
    fn screen_position(&self, position: Pt) -> (i32, i32) {
        let (x, y) = self.frame_pixel(position);
        (self.screen_origin.0 as i32 + x, self.screen_origin.1 as i32 + y)
    }

    /// Rebuilds the loupe when the pointer has crossed into another pixel.
    ///
    /// The crop is magnified by hand rather than by scaling the element, so the
    /// pixels stay square and crisp, which is the whole point of a loupe when
    /// you are hunting for an exact edge or colour.
    fn refresh_magnifier(&mut self) {
        if self.selection.is_some() {
            self.magnifier = None;
            return;
        }
        let Some(position) = self.pointer else {
            return;
        };
        let centre = self.frame_pixel(position);
        if self
            .magnifier
            .as_ref()
            .map(|magnifier| magnifier.centre == centre)
            .unwrap_or(false)
        {
            return;
        }
        self.magnifier = self.build_magnifier(centre).map(|image| Magnifier { centre, image });
    }

    fn build_magnifier(&self, centre: (i32, i32)) -> Option<Arc<RenderImage>> {
        let (frame_w, frame_h) = (self.source.width, self.source.height);
        if frame_w < 2 || frame_h < 2 {
            return None;
        }
        let crop_w = MAGNIFIER_CROP.min(frame_w);
        let crop_h = MAGNIFIER_CROP.min(frame_h);
        let half = MAGNIFIER_CROP / 2;
        let x0 = (centre.0 - half).clamp(0, frame_w - crop_w);
        let y0 = (centre.1 - half).clamp(0, frame_h - crop_h);
        // Where the sampled pixel sits inside the crop, so the crosshair stays
        // on it even when the crop had to be pushed in from an edge.
        let focus = ((centre.0 - x0) as u32, (centre.1 - y0) as u32);

        let (zoom, out_w, out_h) = (
            MAGNIFIER_ZOOM,
            crop_w as u32 * MAGNIFIER_ZOOM as u32,
            crop_h as u32 * MAGNIFIER_ZOOM as u32,
        );
        let mut buffer = image::RgbaImage::new(out_w, out_h);
        let source = self.source.pixels();
        let stride = frame_w as usize * 4;
        for row in 0..crop_h as u32 {
            let source_row = (y0 as u32 + row) as usize * stride;
            for column in 0..crop_w as u32 {
                let index = source_row + (x0 as u32 + column) as usize * 4;
                let pixel = image::Rgba([
                    source[index],
                    source[index + 1],
                    source[index + 2],
                    0xFF,
                ]);
                for dy in 0..zoom {
                    for dx in 0..zoom {
                        buffer.put_pixel(
                            column * zoom as u32 + dx,
                            row * zoom as u32 + dy,
                            pixel,
                        );
                    }
                }
            }
        }

        // A crosshair that stops short of the sample, so the pixel being read
        // stays visible under it. Drawn twice — black then white — so it reads
        // on any background.
        let block = zoom as u32;
        let arm = block * 3;
        let (fx, fy) = (focus.0 as i32 * block as i32, focus.1 as i32 * block as i32);
        for (thickness, ink) in [
            (2i32, image::Rgba([0u8, 0, 0, 0xFF])),
            (1i32, image::Rgba([0xFF, 0xFF, 0xFF, 0xFF])),
        ] {
            let mut plot = |x: i32, y: i32| {
                if x >= 0 && y >= 0 && (x as u32) < out_w && (y as u32) < out_h {
                    buffer.put_pixel(x as u32, y as u32, ink);
                }
            };
            for step in 0..arm as i32 {
                for offset in 0..thickness {
                    let gap = block as i32;
                    // Horizontal arms, then vertical ones, rotated around the
                    // sample block.
                    plot(fx + gap + step, fy + offset);
                    plot(fx - gap - 1 - step, fy + offset);
                    plot(fx + offset, fy + gap + step);
                    plot(fx + offset, fy - gap - 1 - step);
                }
            }
        }
        Some(Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])))
    }

    /// Watches the physical Shift key for as long as the overlay lives.
    ///
    /// The loupe's colour notation swaps on Shift, but GPUI reports *nothing*
    /// for a bare modifier press — verified against this exact backend, a lone
    /// Shift produces no key event at all — so the keyboard has to be asked
    /// directly. One `GetAsyncKeyState` call every 45 ms, and the work stops the
    /// moment the window goes away.
    fn watch_shift(cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut held = win_util::key_down(VK_SHIFT);
            loop {
                cx.background_executor().timer(SHIFT_POLL).await;
                let down = win_util::key_down(VK_SHIFT);
                let pressed = down && !held;
                held = down;
                let alive = this.update(cx, |overlay, cx| {
                    // The notation only matters while the loupe is up.
                    if pressed && overlay.selection.is_none() {
                        overlay.colour_hex = !overlay.colour_hex;
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = event.keystroke.key.as_str();
        // A session owns this window while a frame is up over the live desktop:
        // Esc has to end the *session*, so the rows that have been stitched are
        // handed over, rather than closing the frame and throwing them away. The
        // session's stop key may also arrive through the tray's global claim, in
        // which case this never runs — both paths lead to the same place.
        if self.frame_only {
            if key == "escape" {
                crate::capture::request_stop(cx);
            }
            return;
        }

        // While the loupe is up: Shift swaps the colour notation and C puts the
        // sampled colour on the clipboard.
        if self.selection.is_none() && self.text.is_none() {
            if key.eq_ignore_ascii_case("c") && !event.keystroke.modifiers.control {
                if let Some(position) = self.pointer {
                    let text = self.colour_text(self.pixel_colour(position));
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
                    self.colour_copied = Some(std::time::Instant::now());
                }
                // Picking a colour is the whole errand: drop straight out of the
                // capture instead of making the user press Escape as well.
                self.cancel(window, cx);
                return;
            }
        }

        if self.text.is_some() {
            match key {
                "escape" => {
                    self.text = None;
                    cx.notify();
                }
                "enter" => {
                    self.commit_text(window, cx);
                    cx.notify();
                }
                "backspace" => {
                    self.backspace();
                    cx.notify();
                }
                _ => {}
            }
            return;
        }

        // The size editor owns the keyboard while it is up: every key is either
        // a digit or one of its few commands, and nothing must reach the tools.
        if self.size_edit.is_some() {
            let action = self
                .size_edit
                .as_mut()
                .map(|edit| edit.handle_key(key))
                .unwrap_or(Action2::Close);
            match action {
                Action2::Apply => self.apply_size_edit(cx),
                Action2::Close => {
                    self.size_edit = None;
                    cx.notify();
                }
                Action2::Redraw => cx.notify(),
            }
            return;
        }

        // Arrow keys nudge the region a pixel at a time; with Ctrl they grow it
        // one pixel towards that arrow instead.
        if let Some(step) = Step::from_key(key) {
            if let Some(selection) = self.selection {
                let (view_w, view_h) = self.view_size();
                self.selection = Some(if event.keystroke.modifiers.control {
                    grow(selection, step, view_w, view_h)
                } else {
                    nudge(selection, step, view_w, view_h)
                });
                cx.notify();
            }
            return;
        }

        // Modifier combinations first, so that `Ctrl+P` is not also read as the
        // plain `p` pencil shortcut.
        if event.keystroke.modifiers.control {
            match key {
                "z" => {
                    self.shapes.pop();
                    self.selected_shape = None;
                    cx.notify();
                }
                "c" => self.finish(Finish::Copy, window, cx),
                "s" => self.finish(Finish::Save, window, cx),
                "p" => self.finish(Finish::Pin, window, cx),
                _ => {}
            }
            return;
        }

        match key {
            "escape" => self.cancel(window, cx),
            "enter" => {
                let mode = default_finish(cx);
                self.finish(mode, window, cx);
            }
            // Start stitching a scrolling shot of the framed region.
            "l" => self.apply(Action::LongShot, window, cx),
            // Tool shortcuts, mirroring the toolbar buttons. Digits are the
            // primary binding; letters are there for muscle memory.
            "1" | "r" => self.arm(Tool::Rect, window, cx),
            "2" | "o" => self.arm(Tool::Ellipse, window, cx),
            "3" | "p" => self.arm(Tool::Pen, window, cx),
            "4" | "t" => self.arm(Tool::Text, window, cx),
            "5" | "a" => self.arm(Tool::Arrow, window, cx),
            "6" | "m" => self.arm(Tool::Mosaic, window, cx),
            "7" | "e" => self.arm(Tool::Eraser, window, cx),
            _ => {}
        }
    }

    /// Takes the numbers from the size editor and resizes the region to match.
    ///
    /// The size is what the badge showed — device pixels — so it is converted
    /// back through the same mapping the badge used.
    fn apply_size_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.size_edit.take() else {
            return;
        };
        let Some(current) = self.selection else {
            cx.notify();
            return;
        };
        let (view_w, view_h) = self.view_size();
        let width = edit.width.parse::<f32>().unwrap_or(current.w * self.map.0);
        let height = edit.height.parse::<f32>().unwrap_or(current.h * self.map.1);
        let logical_w = (width / self.map.0).clamp(1.0, view_w - current.x);
        let logical_h = (height / self.map.1).clamp(1.0, view_h - current.y);
        self.selection = Some(Rect::new(current.x, current.y, logical_w, logical_h));
        cx.notify();
    }

    /// Sets the corner radius and remembers it for the next shot.
    fn set_radius(&mut self, radius: f32, cx: &mut Context<Self>) {
        self.radius = radius.clamp(0.0, 96.0);
        let value = self.radius;
        app::update_config(cx, |config| config.corner_radius = value);
        cx.notify();
    }

    /// Arms a tool, or disarms it if it was already armed.
    fn arm(&mut self, tool: Tool, window: &mut Window, cx: &mut Context<Self>) {        self.commit_text(window, cx);
        self.tool = if self.tool == Some(tool) { None } else { Some(tool) };
        cx.notify();
    }

    // -- rendering --------------------------------------------------------

    /// The outline around the window the pointer is lighting up.
    fn hover_chrome(&self) -> Vec<AnyElement> {
        if self.selection.is_some() {
            return Vec::new();
        }
        let Some(rect) = self.hover_rect else {
            return Vec::new();
        };
        vec![
            div()
                .absolute()
                .left(px(rect.x))
                .top(px(rect.y))
                .w(px(rect.w))
                .h(px(rect.h))
                .border_2()
                .border_color(rgb(0x1E88E5))
                .into_any_element(),
            div()
                .absolute()
                .left(px(rect.x))
                .top(px((rect.y - 22.0).max(0.0)))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(3.0))
                .bg(rgba(0x0000_00CC))
                .text_size(px(12.0))
                .text_color(rgb(0xFFFFFF))
                .child(format!(
                    "{:.0} × {:.0}  点击选中该窗口",
                    rect.w * self.map.0,
                    rect.h * self.map.1
                ))
                .into_any_element(),
        ]
    }

    /// The round area the painting tools will actually affect, drawn under the
    /// pointer.
    ///
    /// The mosaic brush and the eraser are both round and both sized, so seeing
    /// the circle is the difference between aiming and guessing. Two nested
    /// rings (dark outside, light inside) keep it readable on any background.
    fn brush_circle(&self) -> Option<AnyElement> {
        let diameter = match self.tool? {
            Tool::Mosaic => self.brush,
            Tool::Eraser => self.eraser,
            _ => return None,
        };
        let pointer = self.pointer?;
        let half = diameter / 2.0;
        Some(
            div()
                .absolute()
                .left(px(pointer.x - half))
                .top(px(pointer.y - half))
                .w(px(diameter))
                .h(px(diameter))
                .rounded(px(half))
                .border_1()
                .border_color(rgba(0x0000_00B3))
                .child(
                    div()
                        .absolute()
                        .left(px(1.0))
                        .top(px(1.0))
                        .w(px(diameter - 2.0))
                        .h(px(diameter - 2.0))
                        .rounded(px(half - 1.0))
                        .border_1()
                        .border_color(rgba(0xFFFF_FFD9)),
                )
                .into_any_element(),
        )
    }

    /// The loupe: magnified pixels, the pointer's screen position and the colour
    /// underneath it.
    ///
    /// It is only up while no region has been framed, which is exactly when the
    /// user is deciding where to start.
    fn magnifier_panel(&self) -> Option<AnyElement> {
        if self.selection.is_some() {
            return None;
        }
        let magnifier = self.magnifier.as_ref()?;
        let pointer = self.pointer?;
        let (view_w, view_h) = self.view_size();

        let side = MAGNIFIER_CROP as f32 * MAGNIFIER_ZOOM as f32;
        let (px_w, px_h) = (side + 8.0, side + 62.0);
        // Sit under the pointer's bottom-right, flipping to the other side when
        // it would leave the screen.
        let mut left = pointer.x + MAGNIFIER_OFFSET;
        let mut top = pointer.y + MAGNIFIER_OFFSET;
        if left + px_w > view_w {
            left = (pointer.x - MAGNIFIER_OFFSET - px_w).max(0.0);
        }
        if top + px_h > view_h {
            top = (pointer.y - MAGNIFIER_OFFSET - px_h).max(0.0);
        }
        left = left.clamp(0.0, (view_w - px_w).max(0.0));
        top = top.clamp(0.0, (view_h - px_h).max(0.0));

        let (screen_x, screen_y) = self.screen_position(pointer);
        let colour = self.pixel_colour(pointer);
        let text = self.colour_text(colour);
        let copied = self
            .colour_copied
            .map(|at| at.elapsed().as_millis() < 1200)
            .unwrap_or(false);

        Some(
            div()
                .id("xshot-loupe")
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(px_w))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(4.0))
                .p(px(4.0))
                .rounded(px(6.0))
                .bg(rgba(0x1E1E_1EEB))
                .border_1()
                .border_color(rgba(0xFFFF_FF33))
                .shadow_lg()
                .child(
                    div()
                        .w(px(side))
                        .h(px(side))
                        .rounded(px(3.0))
                        .border_1()
                        .border_color(rgba(0xFFFF_FF40))
                        .child(
                            img(magnifier.image.clone())
                                .w(px(side))
                                .h(px(side))
                                .object_fit(ObjectFit::Fill),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(11.5))
                        .text_color(rgb(0xFFFFFF))
                        .child(format!("{screen_x}, {screen_y}")),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(5.0))
                        .child(
                            div()
                                .w(px(12.0))
                                .h(px(12.0))
                                .rounded(px(2.0))
                                .bg(rgb(
                                    (colour[0] as u32) << 16
                                        | (colour[1] as u32) << 8
                                        | colour[2] as u32,
                                ))
                                .border_1()
                                .border_color(rgba(0xFFFF_FF59)),
                        )
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(rgb(0xFFFFFF))
                                .child(text),
                        ),
                )
                .child(
                    div()
                        .w_full()
                        .text_size(px(10.5))
                        .text_color(if copied {
                            rgb(0x66BB6A)
                        } else {
                            rgba(0xFFFF_FF99)
                        })
                        .child(if copied {
                            "已复制颜色值".to_string()
                        } else {
                            "Shift 切换 HEX/RGB  C 复制".to_string()
                        }),
                )
                .into_any_element(),
        )
    }

    /// The resize frame around a selected annotation.
    ///
    /// Same eight handles as the selection itself, so a rectangle can be
    /// nudged to size after it has been drawn rather than redrawn.
    fn shape_chrome(&self) -> Vec<AnyElement> {
        let Some(index) = self.selected_shape else {
            return Vec::new();
        };
        let Some(shape) = self.shapes.get(index) else {
            return Vec::new();
        };
        if !matches!(shape, Shape::Rect { .. } | Shape::Ellipse { .. }) {
            return Vec::new();
        }
        let bounds = shape.bounds();

        let mut elements = vec![
            div()
                .absolute()
                .left(px(bounds.x))
                .top(px(bounds.y))
                .w(px(bounds.w))
                .h(px(bounds.h))
                .border_1()
                .border_color(rgba(0x60CD_FFAA))
                .into_any_element(),
        ];
        for (_, point) in handle_points(&bounds) {
            elements.push(
                div()
                    .absolute()
                    .left(px(point.x - 3.5))
                    .top(px(point.y - 3.5))
                    .w(px(7.0))
                    .h(px(7.0))
                    .bg(rgb(0x1E88E5))
                    .border_1()
                    .border_color(rgb(0xFFFFFF))
                    .into_any_element(),
            );
        }
        elements
    }

    /// The darkened area outside the selection.
    ///
    /// Before anything is selected the whole desktop is dimmed: the overlay is
    /// a mode, and showing that immediately is what tells the user the screen
    /// has been frozen and they are expected to drag.
    fn dim_mask(&self) -> Vec<AnyElement> {
        let (view_w, view_h) = self.view_size();
        // `rgba` takes 0xRRGGBBAA, so the alpha belongs in the *low* byte.
        let alpha = (self.dim.clamp(0.0, 0.9) * 255.0).round() as u32;
        let shade = rgba(alpha);

        // The hole is whatever the user is currently pointing at: a dragged
        // selection, or the window highlighted under the cursor.
        let Some(selection) = self.selection.or(self.hover_rect) else {
            return vec![
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h_full()
                    .bg(shade)
                    .into_any_element(),
            ];
        };

        vec![
            div()
                .absolute()
                .left_0()
                .top_0()
                .w_full()
                .h(px(selection.y))
                .bg(shade)
                .into_any_element(),
            div()
                .absolute()
                .left_0()
                .top(px(selection.bottom()))
                .w(px(view_w))
                .h(px((view_h - selection.bottom()).max(0.0)))
                .bg(shade)
                .into_any_element(),
            div()
                .absolute()
                .left_0()
                .top(px(selection.y))
                .w(px(selection.x))
                .h(px(selection.h))
                .bg(shade)
                .into_any_element(),
            div()
                .absolute()
                .left(px(selection.right()))
                .top(px(selection.y))
                .w(px((view_w - selection.right()).max(0.0)))
                .h(px(selection.h))
                .bg(shade)
                .into_any_element(),
        ]
    }

    fn selection_chrome(&self) -> Vec<AnyElement> {
        let Some(selection) = self.selection else {
            return Vec::new();
        };
        let mut elements = Vec::new();

        elements.push(
            div()
                .absolute()
                .left(px(selection.x))
                .top(px(selection.y))
                .w(px(selection.w))
                .h(px(selection.h))
                .border_1()
                .border_color(rgb(0x1E88E5))
                // Doubles as the preview of the corner radius: it is exactly the
                // shape the finished shot will be cut to.
                .rounded(px(self.radius))
                .into_any_element(),
        );

        for (_, point) in handle_points(&selection) {
            elements.push(
                div()
                    .absolute()
                    .left(px(point.x - 3.5))
                    .top(px(point.y - 3.5))
                    .w(px(7.0))
                    .h(px(7.0))
                    .bg(rgb(0xFFFFFF))
                    .border_1()
                    .border_color(rgb(0x1E88E5))
                    .into_any_element(),
            );
        }

        elements
    }

    /// The corner pieces a radius cuts away, painted like the outside of the
    /// selection so the shape of the finished shot is visible while framing.
    ///
    /// Without this the region still looks like a rectangle: the mask punches a
    /// square hole, and the four corner triangles keep the frozen desktop at full
    /// brightness, which reads as "this part is included".
    fn radius_corners(&self) -> Option<AnyElement> {
        let selection = self.selection?;
        let radius = self.radius;
        if radius < 0.5 {
            return None;
        }
        let radius = radius.min(selection.w / 2.0).min(selection.h / 2.0);
        let dim = self.dim;
        Some(
            canvas(
                |_, _, _| (),
                move |_bounds, _, window, _cx| {
                    // The same 4-arc Bezier approximation the ellipse annotation
                    // uses, one quarter turn per corner.
                    const KAPPA: f32 = 0.552_284_75;
                    let k = radius * KAPPA;
                    let (x, y) = (selection.x, selection.y);
                    let (right, bottom) = (selection.right(), selection.bottom());
                    let corners: [(Pt, Pt, Pt, Pt, Pt); 4] = [
                        // start, control 1, control 2, end, corner
                        (
                            Pt::new(x + radius, y),
                            Pt::new(x + radius - k, y),
                            Pt::new(x, y + radius - k),
                            Pt::new(x, y + radius),
                            Pt::new(x, y),
                        ),
                        (
                            Pt::new(right - radius, y),
                            Pt::new(right - radius + k, y),
                            Pt::new(right, y + radius - k),
                            Pt::new(right, y + radius),
                            Pt::new(right, y),
                        ),
                        (
                            Pt::new(right, bottom - radius),
                            Pt::new(right, bottom - radius + k),
                            Pt::new(right - radius + k, bottom),
                            Pt::new(right - radius, bottom),
                            Pt::new(right, bottom),
                        ),
                        (
                            Pt::new(x, bottom - radius),
                            Pt::new(x, bottom - radius + k),
                            Pt::new(x + radius - k, bottom),
                            Pt::new(x + radius, bottom),
                            Pt::new(x, bottom),
                        ),
                    ];
                    for (start, c1, c2, end, corner) in corners {
                        let mut builder = PathBuilder::fill();
                        builder.move_to(point(px(corner.x), px(corner.y)));
                        builder.line_to(point(px(start.x), px(start.y)));
                        builder.cubic_bezier_to(
                            point(px(end.x), px(end.y)),
                            point(px(c1.x), px(c1.y)),
                            point(px(c2.x), px(c2.y)),
                        );
                        builder.close();
                        if let Ok(path) = builder.build() {
                            window.paint_path(path, rgba((dim * 255.0) as u32));
                        }
                    }
                },
            )
            .absolute()
            .left_0()
            .top_0()
            .w_full()
            .h_full()
            .into_any_element(),
        )
    }

    /// The size badge and its editor.
    ///
    /// Separate from the rest of the chrome because both are clickable, and
    /// clickable chrome has to be added above the capture surface.
    fn size_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let selection = self.selection?;
        let label_y = if selection.y >= 24.0 { selection.y - 22.0 } else { selection.y + 2.0 };
        let mut column = div()
            .absolute()
            .left(px(selection.x))
            .top(px(label_y))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .child(
                div()
                    .id("xshot-size-badge")
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(3.0))
                    .bg(rgba(0x0000_00CC))
                    .text_size(px(12.0))
                    .text_color(rgb(0xFFFFFF))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgba(0x1E88_5EE6)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _event, _window, cx| {
                            this.size_edit = this
                                .selection
                                .map(|selection| SizeEdit::open(selection, this.map));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(format!(
                        "{:.0} × {:.0}",
                        selection.w * self.map.0,
                        selection.h * self.map.1
                    )),
            );
        if let Some(edit) = &self.size_edit {
            column = column.child(self.size_editor(edit, cx));
        }
        Some(column.into_any_element())
    }

    /// The little width/height form that drops out of the size badge.
    fn size_editor(&self, edit: &SizeEdit, cx: &mut Context<Self>) -> AnyElement {
        let field = |label: &'static str, value: &str, active: bool| -> AnyElement {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .w(px(18.0))
                        .text_size(px(11.5))
                        .text_color(rgba(0xFFFF_FFB3))
                        .child(label),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .w(px(64.0))
                        .h(px(22.0))
                        .px(px(6.0))
                        .rounded(px(3.0))
                        .bg(rgba(0x0000_0080))
                        .border_1()
                        .border_color(if active { rgb(0x1E88E5) } else { rgba(0xFFFF_FF33) })
                        .text_size(px(12.0))
                        .text_color(rgb(0xFFFFFF))
                        .child(value.to_string()),
                )
                .into_any_element()
        };

        div()
            .id("xshot-size-editor")
            .flex()
            .flex_col()
            .gap(px(6.0))
            .p(px(8.0))
            .rounded(px(5.0))
            .bg(rgba(0x1E1E_1EF2))
            .border_1()
            .border_color(rgba(0xFFFF_FF33))
            .shadow_lg()
            .cursor_default()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_this, _event, _window, cx| cx.stop_propagation()),
            )
            .child(field("宽", &edit.width, !edit.on_height))
            .child(field("高", &edit.height, edit.on_height))
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(rgba(0xFFFF_FF99))
                    .child("Tab 切换 · Enter 应用 · Esc 取消"),
            )
            .into_any_element()
    }

    /// The corner-radius bar that sits to the right of the region.
    ///
    /// Rounding only exists in the finished file, so the bar carries a live
    /// preview of it: a square cut with the current radius, and a slider to drag
    /// the value rather than a row of presets to pick from.
    fn radius_bar(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let selection = self.selection?;
        let (view_w, view_h) = self.view_size();
        const WIDTH: f32 = 40.0;
        /// Tallest radius the slider reaches, in logical pixels.
        const MAX_RADIUS: f32 = 48.0;
        const TRACK: f32 = 150.0;
        /// Where the track starts inside the bar.
        const TRACK_TOP: f32 = 62.0;

        // Outside the right edge when there is room, otherwise tucked inside it.
        let left = if selection.right() + WIDTH + 8.0 <= view_w {
            selection.right() + 8.0
        } else {
            (selection.right() - WIDTH - 8.0).max(0.0)
        };
        let bar_height = TRACK_TOP + TRACK + 16.0;
        let top = selection.y.clamp(0.0, (view_h - bar_height).max(0.0));

        // Remembered so the drag handler can turn a pointer position back into a
        // radius without duplicating the layout.
        self.radius_track = (top + TRACK_TOP, TRACK);

        let filled = (self.radius / MAX_RADIUS).clamp(0.0, 1.0);
        let knob_y = (1.0 - filled) * TRACK;

        Some(
            div()
                .id("xshot-radius-bar")
                .absolute()
                .left(px(left))
                .top(px(top))
                .flex()
                .flex_col()
                .items_center()
                .w(px(WIDTH))
                .p(px(4.0))
                .rounded(px(6.0))
                .bg(rgba(0x1E1E_1EF2))
                .border_1()
                .border_color(rgba(0xFFFF_FF26))
                .shadow_lg()
                // Live preview of the shape the shot will be cut to.
                .child(
                    div()
                        .w(px(22.0))
                        .h(px(22.0))
                        .rounded(px(self.radius / 2.0))
                        .bg(rgb(0xFFFFFF)),
                )
                .child(
                    div()
                        .pt(px(2.0))
                        .text_size(px(10.0))
                        .text_color(rgba(0xFFFF_FFB3))
                        .child(format!("{:.0}", self.radius)),
                )
                .child(
                    // The track. Its own height is fixed, and where the pointer
                    // lands on it is what the drag reads.
                    div()
                        .id("xshot-radius-slider")
                        .relative()
                        .w(px(20.0))
                        .h(px(TRACK))
                        .mt(px(TRACK_TOP - 46.0))
                        .flex()
                        .justify_center()
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, event: &MouseDownEvent, _window, cx| {
                                this.drag_radius(event.position.y, cx);
                                this.radius_drag = true;
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(px(8.0))
                                .top_0()
                                .w(px(4.0))
                                .h(px(TRACK))
                                .rounded(px(2.0))
                                .bg(rgba(0xFFFF_FF33)),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(px(8.0))
                                .top(px(knob_y))
                                .w(px(4.0))
                                .h(px(TRACK - knob_y))
                                .rounded(px(2.0))
                                .bg(rgb(0x1E88E5)),
                        )
                        .child(
                            div()
                                .absolute()
                                .left(px(3.0))
                                .top(px(knob_y - 7.0))
                                .w(px(14.0))
                                .h(px(14.0))
                                .rounded(px(7.0))
                                .bg(rgb(0xFFFFFF))
                                .border_1()
                                .border_color(rgb(0x1E88E5)),
                        ),
                )
                .child(
                    div()
                        .pt(px(2.0))
                        .text_size(px(9.0))
                        .text_color(rgba(0xFFFF_FF80))
                        .child("拖动"),
                )
                .into_any_element(),
        )
    }

    /// Sets the radius from a pointer position on the slider.
    fn drag_radius(&mut self, window_y: Pixels, cx: &mut Context<Self>) {
        let (track_top, track) = self.radius_track;
        if track <= 0.0 {
            return;
        }
        // Mouse positions arrive in the same logical pixels the track is
        // measured in.
        let position = window_y.as_f32();
        let fraction = ((position - track_top) / track).clamp(0.0, 1.0);
        let radius = ((1.0 - fraction) * RADIUS_MAX).round();
        self.set_radius(radius, cx);
    }

    /// Mosaic strokes, shown as bitmaps rather than vectors.
    ///
    /// The preview is generated from the very pixels, brush and block size the
    /// final composite will use, so the blurred region on screen is exactly
    /// what gets saved.
    fn mosaic_elements(&self) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        let mut push = |shape: &Shape| {
            if let Shape::Mosaic { preview: Some(preview), .. } = shape {
                elements.push(
                    img(preview.image.clone())
                        .absolute()
                        .left(px(preview.rect.x))
                        .top(px(preview.rect.y))
                        .w(px(preview.rect.w))
                        .h(px(preview.rect.h))
                        .object_fit(ObjectFit::Fill)
                        .into_any_element(),
                );
            }
        };
        for shape in &self.shapes {
            push(shape);
        }
        if let Some(draft) = &self.draft {
            push(draft);
        }
        elements
    }

    /// All vector annotations, painted through one GPUI canvas.
    fn vector_canvas(&self) -> AnyElement {
        let shapes = self.preview_shapes();
        canvas(
            move |_bounds, _window, _cx| shapes,
            move |_bounds, shapes, window, _cx| {
                for shape in &shapes {
                    if let Shape::Arrow { a, b, color, width } = shape {
                        if let Some((shaft, tip)) = arrow_paths(*a, *b, *width) {
                            window.paint_path(shaft, rgb(*color));
                            window.paint_path(tip, rgb(*color));
                        }
                        continue;
                    }
                    let Some((path, color)) = shape_path(shape) else {
                        continue;
                    };
                    window.paint_path(path, rgb(color));
                }
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .w_full()
        .h_full()
        .into_any_element()
    }

    fn text_elements(&self) -> Vec<AnyElement> {
        let mut elements = Vec::new();
        for shape in &self.shapes {
            if let Shape::Text { pos, text, color, size } = shape {
                elements.push(text_element(*pos, text, *color, *size));
            }
        }
        if let Some(edit) = &self.text {
            // The composition string from the IME shows up here as ordinary
            // text while it is still being edited.
            elements.push(text_element(edit.origin, &edit.text, self.color, 20.0));
            let caret_x = edit.origin.x + measure(&edit.text, 20.0);
            elements.push(
                div()
                    .absolute()
                    .left(px(caret_x))
                    .top(px(edit.origin.y))
                    .w(px(1.5))
                    .h(px(20.0 * 1.3))
                    .bg(rgb(self.color))
                    .into_any_element(),
            );
        }
        elements
    }

    /// Registers this view as the platform text input handler, which is what
    /// makes IME composition (e.g. pinyin) work.
    ///
    /// The handler is registered for the window's whole life, and
    /// [`EntityInputHandler::accepts_text_input`] reports whether a box is
    /// actually open. GPUI never clears a platform input handler once it has
    /// been set, so removing it here would leave the window permanently
    /// believing it is a text field — every character-producing key would then
    /// sit in a one-second disambiguation buffer, and the Enter that finishes
    /// a shot would never arrive.
    fn input_layer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let entity = cx.entity();
        let focus = self.focus.clone();
        Some(
            canvas(
                move |bounds, _window, _cx| bounds,
                move |bounds, _bounds, window, cx| {
                    window.handle_input(
                        &focus,
                        ElementInputHandler::new(bounds, entity.clone()),
                        cx,
                    );
                },
            )
            .absolute()
            .left_0()
            .top_0()
            .w_full()
            .h_full()
            .into_any_element(),
        )
    }

    fn toolbar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {        let selection = self.selection?;
        // Held back until the region is settled. During the initial drag the
        // toolbar would be chasing the pointer and sitting on top of exactly the
        // part of the screen the user is still framing.
        if matches!(self.drag, Some(Drag::Creating { .. })) {
            return None;
        }
        let (view_w, view_h) = self.view_size();

        // Sit under the selection's bottom-right corner, flipping above it when
        // there is not enough room below.
        let below = selection.bottom() + 8.0;
        let top = if below + TOOLBAR_HEIGHT <= view_h {
            below
        } else {
            (selection.y - TOOLBAR_HEIGHT - 8.0).max(0.0)
        };
        let right = (view_w - selection.right()).max(0.0);

        let armed = self.tool;
        let color = self.color;
        let width = self.width;

        // Icon buttons. The labels survive as tooltips-free affordances: the
        // glyphs are the standard ones for these tools, and the hint line on the
        // right still spells out what the armed tool does.
        let group = |art: Art, active: bool, action: Action| -> AnyElement {
            div()
                .id(art.id())
                .flex()
                .items_center()
                .justify_center()
                .w(px(28.0))
                .h(px(24.0))
                .rounded(px(4.0))
                .bg(if active { rgb(0x1E88E5) } else { rgba(0x0000_0000) })
                .cursor_pointer()
                .hover(|style| style.bg(rgb(0x3C3C3C)))
                // The press belongs to the button — it must not fall through to
                // the capture surface and start a new selection under it — but
                // the action runs on the *release*: a button that closes the
                // window on its press leaves its own release in the air, and
                // that release lands on whatever is behind. See `end_session`.
                .on_mouse_down(MouseButton::Left, cx.listener(|_, _event, _, cx| {
                    cx.stop_propagation();
                }))
                .on_mouse_up(
                    MouseButton::Left,
                    cx.listener(move |this, _event, window, cx| {
                        this.apply(action, window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(art.element(16.0, if active { rgb(0xFFFFFF) } else { rgb(0xD8D8D8) }))
                .into_any_element()
        };

        let tools: Vec<AnyElement> = [
            Tool::Rect,
            Tool::Ellipse,
            Tool::Arrow,
            Tool::Pen,
            Tool::Mosaic,
            Tool::Eraser,
            Tool::Text,
        ]
        .into_iter()
        .map(|tool| group(Art::Tool(tool), armed == Some(tool), Action::Arm(tool)))
        .collect();

        let swatches: Vec<AnyElement> = PALETTE
            .into_iter()
            .map(|value| {
                let active = value == color;
                div()
                    .id(("swatch", value))
                    .w(px(16.0))
                    .h(px(16.0))
                    .rounded(px(8.0))
                    .bg(rgb(value))
                    .border_1()
                    .border_color(if active { rgb(0xFFFFFF) } else { rgba(0xFFFF_FF33) })
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, window, cx| {
                            this.apply(Action::Stroke(value), window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .into_any_element()
            })
            .collect();

        // The size row is context sensitive: a mosaic picks its brush width and
        // block size, the eraser its reach, everything else its stroke width.
        let brush = self.brush;
        let block = self.block;
        let eraser = self.eraser;
        let size_action: Action;
        let sizes_source: &[f32];
        let sizes_current: f32;
        let sizes_kind: &'static str;
        let sizes_label: &'static str;
        match armed {
            Some(Tool::Mosaic) => {
                size_action = Action::Brush(0.0);
                sizes_source = &BRUSHES;
                sizes_current = brush;
                sizes_kind = "brush";
                sizes_label = "笔刷";
            }
            Some(Tool::Eraser) => {
                size_action = Action::EraserSize(0.0);
                sizes_source = &ERASERS;
                sizes_current = eraser;
                sizes_kind = "eraser";
                sizes_label = "橡皮直径";
            }
            _ => {
                size_action = Action::Weight(0.0);
                sizes_source = &WIDTHS;
                sizes_current = width;
                sizes_kind = "weight";
                sizes_label = "线宽";
            }
        }
        let sizes: Vec<AnyElement> = sizes_source
            .iter()
            .copied()
            .map(|value| {
                let active = (value - sizes_current).abs() < 0.01;
                div()
                    .id((sizes_kind, (value * 10.0) as u32))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(22.0))
                    .h(px(20.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .bg(if active { rgb(0x1E88E5) } else { rgba(0x0000_0000) })
                    .hover(|style| style.bg(rgb(0x3C3C3C)))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _event, window, cx| {
                            let action = match size_action {
                                Action::Brush(_) => Action::Brush(value),
                                Action::EraserSize(_) => Action::EraserSize(value),
                                _ => Action::Weight(value),
                            };
                            this.apply(action, window, cx);
                            cx.stop_propagation();
                        }),
                    )
                    .child({
                        // The eraser and the mosaic brush are round, so their
                        // buttons show a dot of the real diameter; stroke widths
                        // stay a bar.
                        let (w, h) = if sizes_kind == "eraser" {
                            let diameter = (value / 3.0).clamp(5.0, 15.0);
                            (diameter, diameter)
                        } else {
                            ((value / 2.5).clamp(4.0, 16.0), (value / 5.0).clamp(2.0, 6.0))
                        };
                        div()
                            .w(px(w))
                            .h(px(h))
                            .rounded(px(h / 2.0))
                            .bg(rgb(0xFFFFFF))
                    })
                    .into_any_element()
            })
            .collect();

        // The mosaic also exposes its block size, which is a separate knob from
        // how wide the brush paints.
        let mut block_row: Vec<AnyElement> = Vec::new();
        if armed == Some(Tool::Mosaic) {
            for value in [6.0f32, 10.0, 16.0] {
                let active = (value - block).abs() < 0.01;
                block_row.push(
                    div()
                        .id(("block", (value * 10.0) as u32))
                        .flex()
                        .items_center()
                        .justify_center()
                        .w(px(22.0))
                        .h(px(20.0))
                        .rounded(px(4.0))
                        .text_size(px(10.5))
                        .text_color(rgb(0xFFFFFF))
                        .cursor_pointer()
                        .bg(if active { rgb(0x1E88E5) } else { rgba(0x0000_0000) })
                        .hover(|style| style.bg(rgb(0x3C3C3C)))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _event, window, cx| {
                                this.apply(Action::Block(value), window, cx);
                                cx.stop_propagation();
                            }),
                        )
                        .child(format!("{value:.0}"))
                        .into_any_element(),
                );
            }
        }

        let actions = vec![
            group(Art::Undo, false, Action::Undo),
            group(Art::LongShot, false, Action::LongShot),
            group(Art::Pin, false, Action::Pin),
            group(Art::Save, false, Action::Save),
            group(Art::Copy, false, Action::Copy),
            group(Art::Cancel, false, Action::Cancel),
        ];

        // A one-line reminder of what the armed tool does.
        let hint = armed.map(|tool| {
            div()
                .px(px(6.0))
                .text_size(px(11.0))
                .text_color(rgba(0xFFFF_FF8C))
                .child(tool.hint())
                .into_any_element()
        });

        let divider = || {
            div()
                .w(px(1.0))
                .h(px(18.0))
                .bg(rgba(0xFFFF_FF33))
                .into_any_element()
        };

        Some(
            div()
                .id("xshot-toolbar")
                .absolute()
                .right(px(right))
                .top(px(top))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .px(px(6.0))
                .py(px(5.0))
                .rounded(px(7.0))
                .bg(rgba(0x1E1E_1EF2))
                .border_1()
                .border_color(rgba(0xFFFF_FF26))
                .shadow_lg()
                .children(tools)
                .child(divider())
                .children(swatches)
                .child(divider())
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(rgba(0xFFFF_FFB3))
                        .child(sizes_label),
                )
                .children(sizes)
                .children(block_row)                .child(divider())
                .children(actions)
                .children(hint)
                .into_any_element(),
        )
    }
}

impl Render for Overlay {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Measured rather than assumed, so the frame maps onto whatever client
        // area the compositor actually gave us.
        if !self.shift_watcher {
            self.shift_watcher = true;
            Self::watch_shift(cx);
        }
        if !self.painted {
            self.painted = true;
            crate::timing("6 overlay first paint");
        }
        if self.answering {
            // The frame that answers a press: the dimming and the selection, over
            // the live desktop. This is the moment the user stops waiting.
            self.answering = false;
            crate::timing("p overlay answers the press");
        }
        self.scale = window.scale_factor();
        let client = window.bounds().size;
        let (client_w, client_h) = (client.width.as_f32(), client.height.as_f32());
        if client_w > 0.0 && client_h > 0.0 {
            self.view_size = (client_w, client_h);
            self.map = (self.frame_size.0 / client_w, self.frame_size.1 / client_h);
        }

        let mut root = div()
            .id("xshot-overlay-root")
            .relative()
            .size_full()
            .overflow_hidden()
            // Fully transparent until the capture arrives, so the desktop is
            // what the user sees while this window is being filled in — and
            // transparent throughout a session, which is the whole point of
            // leaving the frame up over the live desktop.
            .bg(if self.ready && !self.frame_only {
                rgb(0x000000)
            } else {
                rgba(0x0000_0000)
            })
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down));

        if !self.ready {
            // Nothing but the empty window: no frozen frame to show yet, and a
            // dimmed or black screen would be worse than not being there.
            return root;
        }

        // A session keeps the frame and nothing else: the frozen desktop and the
        // dimming would both hide the application being worked in, and the tools
        // are out of reach anyway — only the frame's own pixels are part of this
        // window while this is set.
        if self.frame_only {
            return root
                .children(self.session_frame())
                .children(self.session_toolbar(cx))
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                .on_mouse_move(cx.listener(Self::on_mouse_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up));
        }

        // The frozen desktop is not here yet on the first frames of a capture: the
        // window is up before the pixels are, drawing the dimming and the
        // selection over the live desktop. That is the difference between "the
        // press opened the window" and "the press did nothing for 40 ms".
        if self.ready {
            root = root.child(
                img(self.image.clone())
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Fill),
            );
        }
        root = root
            .children(self.dim_mask())
            .child(self.vector_canvas())
            .children(self.mosaic_elements())
            .children(self.text_elements())
            .children(self.selection_chrome())
            .children(self.shape_chrome())
            .children(self.radius_corners())
            .children(self.hover_chrome())
            .children(self.brush_circle())
            .children(self.magnifier_panel());

        if let Some(input) = self.input_layer(cx) {
            root = root.child(input);
        }

        // A transparent full-window hit area: clicks anywhere that the toolbar
        // does not cover drive the selection and the drawing tools.
        root = root.child(
            div()
                .id("xshot-capture-surface")
                .absolute()
                .left_0()
                .top_0()
                .w_full()
                .h_full()
                .cursor(self.cursor.unwrap_or(CursorStyle::Crosshair))
                .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                .on_mouse_down(MouseButton::Right, cx.listener(Self::on_mouse_down))
                .on_mouse_move(cx.listener(Self::on_mouse_move))
                .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                // The right button's gesture is finished on its release too: it
                // is the one that cancels the capture, and the window must still
                // be there when the button comes up.
                .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up)),
        );

        // Everything clickable goes *above* the capture surface: it is a
        // full-window hit area, so anything painted before it would never see a
        // click. The size badge and the radius bar are only chrome with
        // handlers, so they are added here rather than with the rest of it.
        root = root.children(self.size_badge(cx)).children(self.radius_bar(cx));

        if !self.live_drag {
            if let Some(toolbar) = self.toolbar(cx) {
                root = root.child(toolbar);
            }
        }

        root
    }
}

impl EntityInputHandler for Overlay {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let edit = self.text.as_ref()?;
        let start = utf8_index_for_utf16(&edit.text, range.start);
        let end = utf8_index_for_utf16(&edit.text, range.end);
        *adjusted_range = Some(range);
        Some(edit.text[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let edit = self.text.as_ref()?;
        Some(UTF16Selection {
            range: edit.selection.clone(),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.text.as_ref()?.marked.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        if let Some(edit) = self.text.as_mut() {
            edit.marked = None;
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // While a text box is open GPUI sends printable keys down its text
        // input path, and keystrokes that produce a character — Enter, above
        // all — stop being delivered to `on_key_down`. Anything that carries
        // meaning for the box is therefore handled here as well; the two paths
        // cannot double-fire because each only commits while a box is open.
        if self.text.is_some() {
            let mut handled = false;
            for character in text.chars() {
                match character {
                    '\r' | '\n' => {
                        self.commit_text(window, cx);
                        handled = true;
                    }
                    '\u{1b}' => {
                        self.text = None;
                        handled = true;
                    }
                    '\u{8}' | '\u{7f}' => {
                        self.backspace();
                        handled = true;
                    }
                    _ => {}
                }
            }
            if handled {
                cx.notify();
                return;
            }
        }

        let cleaned: String = text.chars().filter(|c| !c.is_control()).collect();
        if cleaned.is_empty() {
            return;
        }
        self.splice(range, &cleaned, false, None);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cleaned: String = new_text.chars().filter(|c| !c.is_control()).collect();
        self.splice(range, &cleaned, true, new_selected_range);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let edit = self.text.as_ref()?;
        Some(Bounds {
            origin: point(px(edit.origin.x), px(edit.origin.y)),
            size: size(px(240.0), px(26.0)),
        })
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        Some(utf16_len(&self.text.as_ref()?.text))
    }

    /// Reports a text field only while a text box is open.
    ///
    /// GPUI consults this before holding a keystroke back for multi-key
    /// disambiguation, so returning `false` keeps the toolbar shortcuts and the
    /// Enter that finishes the shot instantaneous.
    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.text.is_some()
    }
}

// ---------------------------------------------------------------------------
// helpers

/// What the finishing gesture should produce, per the user's preference.
fn default_finish(cx: &App) -> Finish {
    match app::state(cx).config.finish_action {
        crate::config::FinishAction::Copy => Finish::Copy,
        crate::config::FinishAction::Save => Finish::Save,
        crate::config::FinishAction::Pin => Finish::Pin,
    }
}

fn text_element(position: Pt, text: &str, color: u32, size: f32) -> AnyElement {
    let mut element = div()
        .absolute()
        .left(px(position.x))
        .top(px(position.y))
        .text_size(px(size))
        .text_color(rgb(color));
    for line in text.split('\n') {
        element = element.child(div().child(line.to_string()));
    }
    element.into_any_element()
}

/// Approximate advance width, only used to place the caret.
fn measure(text: &str, size: f32) -> f32 {
    text.chars()
        .map(|c| if c.is_ascii() { size * 0.55 } else { size })
        .sum()
}

fn handle_points(selection: &Rect) -> [(Handle, Pt); 8] {
    let center = selection.center();
    [
        (Handle::NW, Pt::new(selection.x, selection.y)),
        (Handle::N, Pt::new(center.x, selection.y)),
        (Handle::NE, Pt::new(selection.right(), selection.y)),
        (Handle::E, Pt::new(selection.right(), center.y)),
        (Handle::SE, Pt::new(selection.right(), selection.bottom())),
        (Handle::S, Pt::new(center.x, selection.bottom())),
        (Handle::SW, Pt::new(selection.x, selection.bottom())),
        (Handle::W, Pt::new(selection.x, center.y)),
    ]
}

fn hit_handle(position: Pt, selection: &Rect) -> Option<Handle> {
    handle_points(selection)
        .into_iter()
        .find(|(_, point)| {
            (point.x - position.x).abs() <= HANDLE_GRAB && (point.y - position.y).abs() <= HANDLE_GRAB
        })
        .map(|(handle, _)| handle)
}

fn resize(original: Rect, handle: Handle, position: Pt, view_w: f32, view_h: f32) -> Rect {
    let mut left = original.x;
    let mut top = original.y;
    let mut right = original.right();
    let mut bottom = original.bottom();

    match handle {
        Handle::NW => {
            left = position.x;
            top = position.y;
        }
        Handle::N => top = position.y,
        Handle::NE => {
            right = position.x;
            top = position.y;
        }
        Handle::E => right = position.x,
        Handle::SE => {
            right = position.x;
            bottom = position.y;
        }
        Handle::S => bottom = position.y,
        Handle::SW => {
            left = position.x;
            bottom = position.y;
        }
        Handle::W => left = position.x,
    }

    let left = left.clamp(0.0, view_w);
    let right = right.clamp(0.0, view_w);
    let top = top.clamp(0.0, view_h);
    let bottom = bottom.clamp(0.0, view_h);
    Rect::new(
        left.min(right),
        top.min(bottom),
        (right - left).abs().max(1.0),
        (bottom - top).abs().max(1.0),
    )
}

/// Tessellates one annotation into a GPUI path.
pub(crate) fn shape_path(shape: &Shape) -> Option<(Path<Pixels>, u32)> {
    match shape {
        Shape::Rect { a, b, color, width } => {
            let mut builder = PathBuilder::stroke(px(*width));
            builder.move_to(point(px(a.x), px(a.y)));
            builder.line_to(point(px(b.x), px(a.y)));
            builder.line_to(point(px(b.x), px(b.y)));
            builder.line_to(point(px(a.x), px(b.y)));
            builder.close();
            builder.build().ok().map(|path| (path, *color))
        }
        Shape::Ellipse { a, b, color, width } => {
            let center_x = (a.x + b.x) / 2.0;
            let center_y = (a.y + b.y) / 2.0;
            let radius_x = (b.x - a.x).abs() / 2.0;
            let radius_y = (b.y - a.y).abs() / 2.0;
            if radius_x < 0.5 || radius_y < 0.5 {
                return None;
            }
            // The usual 4-arc Bezier approximation of an ellipse.
            const KAPPA: f32 = 0.552_284_75;
            let kx = radius_x * KAPPA;
            let ky = radius_y * KAPPA;
            let mut builder = PathBuilder::stroke(px(*width));
            builder.move_to(point(px(center_x + radius_x), px(center_y)));
            builder.cubic_bezier_to(
                point(px(center_x), px(center_y + radius_y)),
                point(px(center_x + radius_x), px(center_y + ky)),
                point(px(center_x + kx), px(center_y + radius_y)),
            );
            builder.cubic_bezier_to(
                point(px(center_x - radius_x), px(center_y)),
                point(px(center_x - kx), px(center_y + radius_y)),
                point(px(center_x - radius_x), px(center_y + ky)),
            );
            builder.cubic_bezier_to(
                point(px(center_x), px(center_y - radius_y)),
                point(px(center_x - radius_x), px(center_y - ky)),
                point(px(center_x - kx), px(center_y - radius_y)),
            );
            builder.cubic_bezier_to(
                point(px(center_x + radius_x), px(center_y)),
                point(px(center_x + kx), px(center_y - radius_y)),
                point(px(center_x + radius_x), px(center_y - ky)),
            );
            builder.close();
            builder.build().ok().map(|path| (path, *color))
        }
        Shape::Pen { points, color, width } => {
            if points.is_empty() {
                return None;
            }
            let mut builder = PathBuilder::stroke(px(*width));
            builder.move_to(point(px(points[0].x), px(points[0].y)));
            if points.len() == 1 {
                // A single dot still needs a closed contour to be filled.
                let half = width / 2.0;
                builder.line_to(point(px(points[0].x + half), px(points[0].y)));
                builder.line_to(point(px(points[0].x + half), px(points[0].y + half)));
            } else {
                for next in &points[1..] {
                    builder.line_to(point(px(next.x), px(next.y)));
                }
            }
            builder.build().ok().map(|path| (path, *color))
        }
        Shape::Arrow { .. } | Shape::Text { .. } | Shape::Mosaic { .. } => None,
    }
}

/// The two primitives an arrow is made of: a stroked shaft and a solid head.
pub(crate) fn arrow_paths(a: Pt, b: Pt, width: f32) -> Option<(Path<Pixels>, Path<Pixels>)> {
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    let length = (dx * dx + dy * dy).sqrt();
    if length < 1.0 {
        return None;
    }
    let head = (width * ARROW_HEAD_LENGTH).min(length * 0.6).max(6.0);
    let half = head * ARROW_HEAD_HALF_WIDTH / ARROW_HEAD_LENGTH;
    let (ux, uy) = (dx / length, dy / length);

    // Stop the shaft at the base of the head so it does not show through.
    let mut shaft = PathBuilder::stroke(px(width));
    shaft.move_to(point(px(a.x), px(a.y)));
    shaft.line_to(point(px(b.x - ux * head), px(b.y - uy * head)));

    let mut tip = PathBuilder::fill();
    tip.move_to(point(px(b.x), px(b.y)));
    tip.line_to(point(
        px(b.x - ux * head - uy * half),
        px(b.y - uy * head + ux * half),
    ));
    tip.line_to(point(
        px(b.x - ux * head + uy * half),
        px(b.y - uy * head - ux * half),
    ));
    tip.close();

    Some((shaft.build().ok()?, tip.build().ok()?))
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn utf16_len_upto(text: &str, byte_index: usize) -> usize {
    text[..byte_index].chars().map(char::len_utf16).sum()
}

fn utf8_index_for_utf16(text: &str, target: usize) -> usize {
    let mut units = 0;
    for (index, character) in text.char_indices() {
        if units >= target {
            return index;
        }
        units += character.len_utf16();
    }
    text.len()
}

fn utf16_index_for_utf8(text: &str, target: usize) -> usize {
    utf16_len_upto(text, target.min(text.len()))
}



