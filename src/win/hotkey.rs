//! Parsing and registering global hotkeys.
//!
//! The default bindings are modifier combinations rather than bare function
//! keys. `RegisterHotKey` claims a chord system-wide, so binding a naked `F1`
//! would break F1 in every other program — which is exactly what a screenshot
//! tool must not do. A chord such as `Ctrl+Alt+A` is effectively never used by
//! anything else, and the user can change it.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};

/// Do not repeat while the chord is held down.
const MOD_NOREPEAT: u32 = 0x4000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub modifiers: u32,
    pub virtual_key: u32,
}

impl Hotkey {
    /// The `HOT_KEY_MODIFIERS` value to hand to `RegisterHotKey`.
    pub fn modifiers_for_registration(self) -> HOT_KEY_MODIFIERS {
        HOT_KEY_MODIFIERS(self.modifiers | MOD_NOREPEAT)
    }

    /// Renders the chord back in the form the settings page edits.
    pub fn display(self) -> String {
        let mut parts = Vec::new();
        if self.modifiers & MOD_CONTROL.0 != 0 {
            parts.push("Ctrl".to_string());
        }
        if self.modifiers & MOD_ALT.0 != 0 {
            parts.push("Alt".to_string());
        }
        if self.modifiers & MOD_SHIFT.0 != 0 {
            parts.push("Shift".to_string());
        }
        if self.modifiers & MOD_WIN.0 != 0 {
            parts.push("Win".to_string());
        }
        parts.push(key_name(self.virtual_key));
        parts.join("+")
    }

    /// Whether a key was actually named.
    ///
    /// A binding without modifiers is allowed — the user may well prefer a
    /// plain `F1` — but it is claimed system-wide, so the settings page warns
    /// about it rather than refusing it.
    pub fn is_bound(self) -> bool {
        self.virtual_key != 0
    }

    /// Whether the binding is a chord, and therefore cannot collide with a key
    /// another program expects to receive on its own.
    pub fn has_modifiers(self) -> bool {
        self.modifiers != 0
    }
}

/// Parses `Ctrl+Alt+A`, `ctrl-shift-f1`, `Win+PrintScreen`, and so on.
///
/// Case and separators are forgiving because this text is typed by hand.
pub fn parse(spec: &str) -> Option<Hotkey> {
    let mut modifiers = 0u32;
    let mut virtual_key = 0u32;

    for raw in spec.split(['+', '-', ' ']).filter(|part| !part.is_empty()) {
        let part = raw.trim();
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
            "alt" => modifiers |= MOD_ALT.0,
            "shift" => modifiers |= MOD_SHIFT.0,
            "win" | "super" | "meta" => modifiers |= MOD_WIN.0,
            other => {
                virtual_key = key_code(other)?;
            }
        }
    }

    let hotkey = Hotkey {
        modifiers,
        virtual_key,
    };
    hotkey.is_bound().then_some(hotkey)
}

fn key_code(name: &str) -> Option<u32> {
    let mut chars = name.chars();
    if let (Some(single), None) = (chars.next(), chars.next()) {
        let upper = single.to_ascii_uppercase();
        if upper.is_ascii_uppercase() {
            return Some(upper as u32);
        }
        if upper.is_ascii_digit() {
            return Some(upper as u32);
        }
    }
    if let Some(number) = name.strip_prefix('f') {
        if let Ok(index) = number.parse::<u32>() {
            if (1..=24).contains(&index) {
                return Some(0x70 + index - 1);
            }
        }
    }
    Some(match name {
        "printscreen" | "prtsc" => 0x2C,
        "insert" | "ins" => 0x2D,
        "delete" | "del" => 0x2E,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "pgup" => 0x21,
        "pagedown" | "pgdn" => 0x22,
        "space" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "backspace" => 0x08,
        "escape" | "esc" => 0x1B,
        "`" | "tilde" => 0xC0,
        "," => 0xBC,
        "." => 0xBE,
        "/" => 0xBF,
        ";" => 0xBA,
        "'" => 0xDE,
        "[" => 0xDB,
        "]" => 0xDD,
        "\\" => 0xDC,
        "-" => 0xBD,
        "=" => 0xBB,
        _ => return None,
    })
}

fn key_name(virtual_key: u32) -> String {
    if (0x41..=0x5A).contains(&virtual_key) {
        return (virtual_key as u8 as char).to_string();
    }
    if (0x30..=0x39).contains(&virtual_key) {
        return (virtual_key as u8 as char).to_string();
    }
    if (0x70..=0x87).contains(&virtual_key) {
        return format!("F{}", virtual_key - 0x70 + 1);
    }
    match virtual_key {
        0x2C => "PrintScreen".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x08 => "Backspace".into(),
        0x1B => "Escape".into(),
        other => format!("0x{other:02X}"),
    }
}
