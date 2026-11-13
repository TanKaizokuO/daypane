//! `config.toml`. Every field has a default; a missing file means all defaults.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub popup: PopupConfig,
    pub hover: HoverConfig,
    pub reminders: ReminderConfig,
    pub google: GoogleConfig,
    pub waybar: WaybarConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PopupConfig {
    pub position: Position,
    pub margin_top: i32,
    pub margin_right: i32,
    pub width: i32,
    pub height: i32,
    pub theme: Theme,
}

/// Horizontal popup placement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    #[default]
    Center,
    Right,
}

impl Position {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Center => "center",
            Self::Right => "right",
        }
    }
}

/// Popup colour scheme. `Dark`/`Light` select the GTK theme's dark/light variant;
/// `System` leaves GTK's own setting untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    System,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::System => "system",
        }
    }
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            position: Position::Center,
            margin_top: 8,
            margin_right: 8,
            width: 760,
            height: 480,
            theme: Theme::Dark,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HoverConfig {
    pub enabled: bool,
    pub width: i32,
    pub height: i32,
    pub delay_ms: u64,
}

impl Default for HoverConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            width: 190,
            height: 40,
            delay_ms: 150,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReminderConfig {
    /// Minutes before a task's `due_time` to notify.
    pub task_offset_minutes: i64,
    /// Snooze duration for the notification "Snooze" action.
    pub snooze_minutes: i64,
}

impl Default for ReminderConfig {
    fn default() -> Self {
        Self {
            task_offset_minutes: 10,
            snooze_minutes: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoogleConfig {
    /// OAuth "Desktop app" client ID from your own Google Cloud project.
    pub client_id: Option<String>,
    /// Desktop-app client secret (Google requires it for installed apps; not confidential).
    pub client_secret: Option<String>,
    /// Seconds between incremental syncs.
    pub sync_interval_seconds: u64,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            client_id: None,
            client_secret: None,
            sync_interval_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WaybarConfig {
    /// Waybar `signal` number; the daemon sends SIGRTMIN+signal to waybar after each write.
    pub signal: u8,
}

impl Default for WaybarConfig {
    fn default() -> Self {
        Self { signal: 8 }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&crate::paths::config_file()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_defaults() {
        let c: Config = toml::from_str("[reminders]\ntask_offset_minutes = 30\n").unwrap();
        assert_eq!(c.reminders.task_offset_minutes, 30);
        assert_eq!(c.reminders.snooze_minutes, 10);
        assert_eq!(c.popup.margin_top, 8);
    }

    #[test]
    fn popup_and_hover_defaults_and_parsing() {
        let defaults: Config = toml::from_str("").unwrap();
        assert_eq!(defaults.popup.position, Position::Center);
        assert!(defaults.hover.enabled);
        assert_eq!(
            (
                defaults.hover.width,
                defaults.hover.height,
                defaults.hover.delay_ms
            ),
            (190, 40, 150)
        );

        let config: Config = toml::from_str(
            "[popup]\nposition = \"right\"\n[hover]\nenabled = false\nwidth = 220\nheight = 30\ndelay_ms = 75\n",
        )
        .unwrap();
        assert_eq!(config.popup.position, Position::Right);
        assert!(!config.hover.enabled);
        assert_eq!(
            (
                config.hover.width,
                config.hover.height,
                config.hover.delay_ms
            ),
            (220, 30, 75)
        );
        assert!(toml::from_str::<Config>("[popup]\nposition = \"left\"").is_err());
        assert!(toml::from_str::<Config>("[hover]\nunknown = true").is_err());
    }

    #[test]
    fn unknown_key_is_an_error() {
        assert!(toml::from_str::<Config>("[popup]\nmargn_top = 3\n").is_err());
    }
}
