# $APP — v1 spec

> **Naming.** `$APP` = the app name, defined **once** in `Cargo.toml` `[package] name` (currently `daypane`).
> Code derives every identifier from it in `src/app.rs` (`APP_NAME`, `APP_ID`, `DAEMON_UNIT`, …) via `env!("CARGO_PKG_NAME")`.
> Non-Rust assets (systemd unit, `.desktop`, Waybar snippet) are rendered from templates by `$APP install`, never committed with the name baked in.
> `scripts/check-name.sh` fails CI if the name appears literally outside `Cargo.toml` (prose `*.md` exempt).

## Positioning
Public, open-source Waybar popup: **Google Calendar (read-only) on the left, a local task list on the right**, with the task list drivable by AI agents over MCP. Differentiator vs Waycal/mycal: real Google integration done right + agent-native tasks.

## Stack
- Rust, single binary, subcommands: `popup | daemon | mcp | install | login | calendars | add | today | next | done | reopen | rm | restore | ls`.
- gtk4-rs + gtk4-layer-shell; rusqlite (WAL); `rmcp` (official Rust MCP SDK).
- Paths via XDG: `$XDG_CONFIG_HOME/$APP/config.toml`, `$XDG_DATA_HOME/$APP/$APP.db`, `$XDG_CONFIG_HOME/$APP/style.css`.

## Google Calendar
- Calendar REST API v3, **read-only** (`calendar.readonly` scope), incremental sync via `syncToken`; full resync on `410 Gone`.
- OAuth: **bring-your-own client ID**, always (no bundled client, no verification). Loopback redirect + PKCE. README carries a step-by-step Google Cloud setup guide.
- Refresh token stored in Secret Service (`org.freedesktop.secrets`), keyed by `APP_ID`.
- Single account. User selects which calendars are shown.
- Backend behind a `CalendarBackend` trait so CalDAV can be added later.

## Tasks
- Local SQLite only; the app owns it.
- Fields: `title`, `due_date`, optional `due_time`, `notes` (Markdown), `priority` (none/low/med/high), `tags[]`, `links[]`, `completed_at`, `deleted_at`.
- Delete = soft delete to trash; undo/restore from GUI, CLI, MCP; purge after 30 days.
- No local events; everything you create is a task.

## MCP server
- `$APP mcp`, stdio transport, spawned by the agent; opens the shared DB directly.
- Scope: **tasks only** (no calendar access).
- Tools: `list_tasks` (filters: date, tag, status, overdue, query), `get_task`, `create_task`, `update_task`, `complete_task`, `reopen_task`, `delete_task` (soft), `restore_task`.

## Daemon (`$APP daemon`)
- systemd user unit (`DAEMON_UNIT`) rendered by `$APP install`; documented `exec` fallback for non-systemd setups.
- Responsibilities: Google incremental sync, reminder scheduling, Waybar JSON output.
- Reminders: Google events use each event's own Google reminder overrides/defaults; tasks with a `due_time` fire at a configurable default offset. Tasks with date only: no reminder.
- Missed reminders (suspend/off) are **dropped**. The Today view's Overdue section is the safety net.
- Notifications via `org.freedesktop.Notifications` with actions (Open, Snooze) where the server supports them.

## Popup (`$APP popup`)
- Single instance via GApplication (`APP_ID`); second invocation toggles closed.
- Layer-shell overlay, anchored top-right (configurable margins), keyboard mode `OnDemand`; closes on Esc or focus loss.
- Reloads on DB file change (inotify), so MCP/CLI/daemon writes appear live.
- **Left pane:** month grid; below it the selected day's Google events (read-only, Meet link if present).
- **Right pane:** with no date selected, Today = Overdue → Due today → collapsible Inbox (undated). Clicking a date shows that date's tasks (Inbox hidden).
- Theming: dark by default (`[popup] theme = dark|light|system`); built-in dark palette over the GTK theme's dark variant; user `style.css` override.

## Input
- Vim-style: `h/j/k/l` + arrows move days; `Tab` switches panes; `j/k` tasks; `x` complete; `e` edit; `d` trash; `u` undo; `t` today; `N` capture; `/` search; `Esc` close.
- Quick capture: English natural-language dates ("mon 5pm", "tomorrow", "in 3 days", "nov 20"), parsed locally, plus `#tag`, `!high`, bare URLs → links. Live parse preview chip; Enter saves.
- `/` searches tasks and cached events.

## Waybar
- Daemon writes Waybar JSON (`return-type: json`): text = next-event countdown, else open-task count; tooltip = today's agenda; class `overdue` when overdue tasks exist. Left-click toggles popup.

## CLI
- `$APP add "<capture syntax>"`, `$APP today`, `$APP next`, `$APP done <id>`, `$APP reopen <id>`, `$APP rm <id>` (trash), `$APP restore <id>`, `$APP ls [filters]`.
- `$APP login` (Google OAuth), `$APP calendars [--select|--deselect <id|name>]` (calendar selection).

## Deferred (not v1)
Google writes, CalDAV, week view, multiple accounts, pywal/Matugen adapters, .ics import/export, recurrence, subtasks, projects/lists.
