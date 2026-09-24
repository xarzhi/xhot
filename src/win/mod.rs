//! Native Windows integration: screen capture, GDI compositing, the clipboard,
//! the notification-area icon, the global hotkeys, and the system OCR engine.

pub mod autostart;
pub mod clip;
pub mod frame;
pub mod hotkey;
pub mod icon;
pub mod ocr;
pub mod paint;
pub mod tray;
pub mod util;
