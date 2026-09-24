//! Persisted user configuration.
//!
//! The file lives at `%APPDATA%\xshot\config.json`. Loading never fails: a
//! missing or corrupt file simply yields the defaults, because a screenshot
//! tool that refuses to start over its own settings is worse than useless.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What the settings window's close button should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseBehavior {
    /// Terminate the whole program.
    Exit,
    /// Keep running in the notification area so the hotkeys keep working.
    MinimizeToTray,
}

impl Default for CloseBehavior {
    fn default() -> Self {
        Self::MinimizeToTray
    }
}

impl CloseBehavior {
    pub const ALL: [CloseBehavior; 2] = [CloseBehavior::MinimizeToTray, CloseBehavior::Exit];

    pub fn label(self) -> &'static str {
        match self {
            Self::Exit => "退出程序",
            Self::MinimizeToTray => "最小化至托盘",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Exit => "关闭设置窗口时结束 xshot 进程",
            Self::MinimizeToTray => "关闭后继续驻留托盘，快捷键依然可用",
        }
    }
}

/// Light, dark, or whatever Windows is using.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeChoice {
    System,
    Dark,
    Light,
}

impl Default for ThemeChoice {
    fn default() -> Self {
        Self::Dark
    }
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::Dark, ThemeChoice::Light, ThemeChoice::System];

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "跟随系统",
            Self::Dark => "深色",
            Self::Light => "浅色",
        }
    }
}

/// What the finishing gesture (Enter, double-click, 完成) should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishAction {
    Copy,
    Save,
    Pin,
}

impl Default for FinishAction {
    fn default() -> Self {
        Self::Copy
    }
}

impl FinishAction {
    pub const ALL: [FinishAction; 3] = [FinishAction::Copy, FinishAction::Save, FinishAction::Pin];

    pub fn label(self) -> &'static str {
        match self {
            Self::Copy => "复制到剪贴板",
            Self::Save => "保存到文件",
            Self::Pin => "钉在桌面",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // -- 常规 ---------------------------------------------------------------
    /// Behaviour of the settings window's close button.
    pub close_behavior: CloseBehavior,
    /// Directory that `保存` writes PNG files into.
    pub save_dir: PathBuf,
    /// Copy the finished shot to the clipboard as well as doing whatever the
    /// finish action is.
    pub copy_on_finish: bool,
    /// Register the app to start with Windows.
    pub autostart: bool,
    /// Start without opening the settings window — just the tray icon.
    pub silent_start: bool,

    // -- 外观 ---------------------------------------------------------------
    pub theme: ThemeChoice,
    /// Whether pins get a desktop-compositor drop shadow.
    pub pin_shadow: bool,

    // -- 截图 ---------------------------------------------------------------
    /// What Enter, double-click and 完成 produce.
    pub finish_action: FinishAction,
    /// Mosaic block size in logical pixels.
    pub mosaic_block: f32,
    /// How much the desktop is darkened outside the selection (0.0 - 0.8).
    pub dim_strength: f32,

    // -- 快捷键 -------------------------------------------------------------
    /// Global capture hotkey, e.g. `Ctrl+Alt+A`.
    pub hotkey_capture: String,
    /// Global "bring back the last pin" hotkey.
    pub hotkey_restore_pin: String,
    /// Holding Ctrl and dragging the left button captures that rectangle and
    /// pins it on release, without a separate overlay step.
    pub ctrl_drag_pin: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            close_behavior: CloseBehavior::default(),
            save_dir: default_save_dir(),
            copy_on_finish: true,
            autostart: false,
            // A screenshot tool lives in the tray; showing the settings window
            // on every login would be noise.
            silent_start: true,
            theme: ThemeChoice::default(),
            pin_shadow: true,
            finish_action: FinishAction::default(),
            mosaic_block: 10.0,
            dim_strength: 0.42,
            // The user's own bindings. Both are plain function keys by
            // request; the settings page turns them red when another program
            // already owns one, and they can be re-recorded there.
            hotkey_capture: "F1".into(),
            hotkey_restore_pin: "F2".into(),
            ctrl_drag_pin: true,
        }
    }
}

/// The desktop, where a screenshot is easiest to find again.
pub fn default_save_dir() -> PathBuf {
    desktop_dir().unwrap_or_else(std::env::temp_dir)
}

/// `%USERPROFILE%\Desktop`.
pub fn desktop_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|home| PathBuf::from(home).join("Desktop"))
}

/// Where `保存` used to write before the save dialog existed.
fn legacy_save_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .map(|home| PathBuf::from(home).join("Pictures").join("xshot"))
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("xshot").join("config.json")
}

impl Config {
    pub fn load() -> Self {
        let mut config: Config = std::fs::read_to_string(config_path())
            .ok()
            .and_then(|raw| serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default();
        config.migrate();
        config
    }

    /// Corrects bindings that an earlier build wrote with the wrong default.
    ///
    /// Those exact chords were never a user choice — they were a bad default —
    /// so restoring the intended function keys cannot hurt anyone who actually
    /// picked them, and it spares everyone else from having to notice.
    fn migrate(&mut self) {
        if self.hotkey_capture == "Ctrl+Alt+A" {
            self.hotkey_capture = "F1".into();
        }
        if self.hotkey_restore_pin == "Ctrl+Alt+P" {
            self.hotkey_restore_pin = "F2".into();
        }
        // Saving now goes through a dialog that defaults to the desktop, so a
        // config still pointing at the old built-in folder is carrying a value
        // nobody chose.
        if let Some(legacy) = legacy_save_dir() {
            if self.save_dir == legacy {
                self.save_dir = default_save_dir();
            }
        }
    }

    pub fn store(&self) {
        let path = config_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Dark unless the user asked for light and the system agrees.
    pub fn resolved_theme(&self) -> crate::ui::theme::Theme {
        use crate::ui::theme::Theme;
        match self.theme {
            ThemeChoice::Dark => Theme::dark(),
            ThemeChoice::Light => Theme::light(),
            ThemeChoice::System => {
                if crate::win::util::system_uses_light_theme() {
                    Theme::light()
                } else {
                    Theme::dark()
                }
            }
        }
    }
}
