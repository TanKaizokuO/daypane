mod app;
mod capture;
mod cli;
mod config;
mod daemon;
mod db;
mod google;
mod install;
mod mcp;
mod paths;
#[cfg(feature = "gui")]
mod popup;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = app::APP_NAME, version, about = env!("CARGO_PKG_DESCRIPTION"))]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Toggle the popup window (single instance).
    Popup,
    /// Background service: Google sync, reminders, Waybar output.
    Daemon,
    /// MCP server over stdio (tasks only).
    Mcp,
    /// Render and install the systemd user unit, .desktop file, and print the Waybar snippet.
    Install(install::InstallArgs),
    /// Authorize Google Calendar (read-only) in the browser.
    Login,
    /// List Google calendars, or choose which are shown.
    Calendars(cli::CalendarsArgs),
    /// Add a task using capture syntax: "pay rent fri 5pm #home !high https://…".
    Add {
        /// Capture text (joined with spaces).
        #[arg(required = true, num_args = 1..)]
        text: Vec<String>,
    },
    /// Today's agenda: events, overdue, due today, inbox.
    Today(cli::JsonFlag),
    /// The next upcoming event or timed task.
    Next(cli::JsonFlag),
    /// Mark a task complete.
    Done { id: i64 },
    /// Reopen a completed task.
    Reopen { id: i64 },
    /// Move a task to trash.
    Rm { id: i64 },
    /// Restore a task from trash.
    Restore { id: i64 },
    /// List tasks.
    Ls(cli::LsArgs),
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        #[cfg(feature = "gui")]
        Cmd::Popup => popup::run(),
        #[cfg(not(feature = "gui"))]
        Cmd::Popup => {
            anyhow::bail!("built without the `gui` feature; rebuild with default features")
        }
        Cmd::Daemon => daemon::run(),
        Cmd::Mcp => mcp::run(),
        Cmd::Install(a) => install::run(a),
        Cmd::Login => google::login(),
        Cmd::Calendars(a) => cli::calendars(a),
        Cmd::Add { text } => cli::add(&text.join(" ")),
        Cmd::Today(f) => cli::today(f),
        Cmd::Next(f) => cli::next(f),
        Cmd::Done { id } => cli::done(id),
        Cmd::Reopen { id } => cli::reopen(id),
        Cmd::Rm { id } => cli::rm(id),
        Cmd::Restore { id } => cli::restore(id),
        Cmd::Ls(a) => cli::ls(a),
    }
}
