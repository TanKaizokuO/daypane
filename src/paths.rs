//! XDG locations. Every path is derived from `APP_NAME`.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::app::APP_NAME;

fn base(dir: Option<PathBuf>, what: &str) -> Result<PathBuf> {
    dir.map(|d| d.join(APP_NAME))
        .with_context(|| format!("cannot determine XDG {what} directory"))
}

/// `$XDG_CONFIG_HOME/$APP`
pub fn config_dir() -> Result<PathBuf> {
    base(dirs::config_dir(), "config")
}

/// `$XDG_DATA_HOME/$APP`
pub fn data_dir() -> Result<PathBuf> {
    base(dirs::data_dir(), "data")
}

/// `$XDG_RUNTIME_DIR/$APP` (falls back to the cache dir when no runtime dir exists).
pub fn runtime_dir() -> Result<PathBuf> {
    base(dirs::runtime_dir().or_else(dirs::cache_dir), "runtime")
}

/// `$XDG_CONFIG_HOME/$APP/config.toml`
pub fn config_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// `$XDG_CONFIG_HOME/$APP/style.css`
#[cfg_attr(not(feature = "gui"), allow(dead_code))] // used by the popup
pub fn style_file() -> Result<PathBuf> {
    Ok(config_dir()?.join("style.css"))
}

/// `$XDG_DATA_HOME/$APP/$APP.db`
pub fn db_file() -> Result<PathBuf> {
    Ok(data_dir()?.join(format!("{APP_NAME}.db")))
}

/// `$XDG_RUNTIME_DIR/$APP/waybar.json`, written by the daemon, read by Waybar.
pub fn waybar_file() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("waybar.json"))
}
