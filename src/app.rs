//! Single source of truth for every name-derived identifier.
//! The name itself lives in Cargo.toml `[package] name`; NEVER write it literally in code.

/// Binary, CLI, config-dir, data-dir, MCP server, and systemd unit name.
pub const APP_NAME: &str = env!("CARGO_PKG_NAME");

/// Reverse-DNS application ID (GApplication single-instance, D-Bus, .desktop, notifications).
pub const APP_ID: &str = concat!("io.github.TanKaizokuO.", env!("CARGO_PKG_NAME"));

/// Secret Service attribute value used to store the Google refresh token.
pub const KEYRING_SERVICE: &str = APP_ID;

/// systemd user unit for `APP_NAME daemon`.
pub const DAEMON_UNIT: &str = concat!(env!("CARGO_PKG_NAME"), ".service");

/// Waybar CSS class / module id prefix.
pub const WAYBAR_MODULE: &str = concat!("custom/", env!("CARGO_PKG_NAME"));
