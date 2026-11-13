# daypane

A Waybar popup for Linux: **Google Calendar (read-only) on the left, a local task list on the right** — and the task list can be driven by AI agents over MCP.

Click a Waybar module and a layer-shell overlay opens with a month grid, the selected day's Google events, and your tasks (Overdue → Due today → Inbox). Everything is keyboard-driven, tasks live in a local SQLite file, and Google access is read-only with *your own* OAuth client — no bundled credentials, nothing hosted.

## Features

- **Google Calendar, read-only** — Calendar API v3 with incremental sync (`syncToken`, full resync on `410 Gone`), pick which calendars are shown, Meet links shown on events.
- **Local tasks** — title, due date, optional due time, Markdown notes, priority, tags, links. Soft delete to trash with undo/restore (purged after 30 days).
- **Quick capture** — type `pay rent fri 5pm #home !high https://example.com`; dates are parsed locally, with a live preview chip.
- **Reminders** — Google events use their own reminder settings; tasks with a due time notify at a configurable offset; notifications offer Open / Snooze where the server supports actions. Missed reminders (suspend, powered off) are dropped; the Overdue section is the safety net.
- **Waybar module** — next-event countdown (else open-task count), tooltip with today's agenda, `overdue` CSS class.
- **Agent-native** — an MCP server (`daypane mcp`) exposes the task list to Claude Code and other MCP clients.
- **Live reload** — the popup refreshes when the database changes, so CLI / MCP / daemon writes appear immediately.
- **Vim-style keys**, GTK-theme aware, user `style.css` override.

## Build requirements

- Rust (stable, edition 2021)
- GTK 4.14+ development files
- `gtk4-layer-shell` (library + development files)
- A Secret Service provider for storing the Google refresh token (GNOME Keyring, KeePassXC, …)
- A Wayland compositor that supports `wlr-layer-shell` (Sway, Hyprland, river, …)

### Ubuntu / Linux Mint 24.04

```sh
sudo apt install build-essential pkg-config libgtk-4-dev meson ninja-build \
    libwayland-dev wayland-protocols gobject-introspection libgirepository1.0-dev \
    valac git

# gtk4-layer-shell is not packaged for 24.04: build it from source
git clone --depth 1 --branch v1.1.1 https://github.com/wmww/gtk4-layer-shell
cd gtk4-layer-shell
meson setup build -Dexamples=false -Ddocs=false -Dtests=false
ninja -C build
sudo ninja -C build install
sudo ldconfig
```

### Arch Linux

```sh
sudo pacman -S gtk4 gtk4-layer-shell
```

### Headless build (no GUI)

The GTK popup is behind the `gui` cargo feature (on by default). Everything else (daemon, CLI, MCP server) builds without GTK:

```sh
cargo build --no-default-features
```

`daypane popup` is unavailable in such a build. CI builds this way (GTK 4.14 + gtk4-layer-shell are not available on its runners).

## Install

```sh
cargo install --path .
daypane install
```

`daypane install`:

1. writes the systemd user unit to `$XDG_CONFIG_HOME/systemd/user/daypane.service` and runs `systemctl --user daemon-reload` and `systemctl --user enable --now daypane.service`;
2. writes a `.desktop` file to `$XDG_DATA_HOME/applications/io.github.TanKaizokuO.daypane.desktop` (needed so notifications can open the popup);
3. creates a commented default `config.toml` — only if you don't have one yet;
4. prints the Waybar module and CSS snippets to add.

Options:

| Flag           | Effect                                                                          |
| -------------- | ------------------------------------------------------------------------------- |
| `--print`      | Print every rendered file and the instructions; write nothing, run nothing.     |
| `--no-systemd` | Skip the systemd unit and `systemctl`; print an `exec` line for your compositor. |

The unit's `ExecStart` is the absolute path of the binary that ran `install`, so re-run `daypane install` after moving the binary.

## Google Calendar setup

Google requires every app that touches Calendar data to use an OAuth client. daypane ships none: you create a free one in your own Google Cloud project (about five minutes), so no third party ever sees your calendar.

1. **Create a project.** Go to <https://console.cloud.google.com/>, open the project picker → **New Project**, name it (e.g. "daypane"), **Create**, and make sure it is selected.
2. **Enable the API.** **APIs & Services → Library**, search for **Google Calendar API**, open it, click **Enable**.
3. **Configure the OAuth consent screen.** **APIs & Services → OAuth consent screen** (in newer consoles: **Google Auth Platform → Branding / Audience**).
   - User type: **External**.
   - Fill in the app name (e.g. "daypane") and your email as support and developer contact. Scopes can be left empty — the app requests `calendar.readonly` itself.
   - Publishing status: **Testing**.
   - Under **Test users**, **Add users** and add your own Google account address.
4. **Mind the 7-day token expiry.** While the consent screen is in **Testing** mode, Google expires refresh tokens after **7 days**, and you would have to run `daypane login` weekly. To avoid that, set the **Publishing status** to **In production** (**Publish app**). For personal use you do **not** need to submit the app for verification: it stays "unverified", which only means:
   - the sign-in page shows a warning, *"Google hasn't verified this app"* — click **Advanced → Go to daypane (unsafe)** to continue (it is your own app, running locally);
   - unverified apps are limited to 100 users, which is irrelevant for a personal client.

   Refresh tokens issued to a published (even unverified) app do not have the 7-day limit.
5. **Create the OAuth client.** **APIs & Services → Credentials → Create credentials → OAuth client ID**, Application type: **Desktop app**, name it, **Create**. Copy the **Client ID** and **Client secret** (for installed apps the secret is not confidential, but Google still requires it).
6. **Put them in the config.** Edit `~/.config/daypane/config.toml`:

   ```toml
   [google]
   client_id = "1234567890-abc.apps.googleusercontent.com"
   client_secret = "GOCSPX-..."
   ```

7. **Log in.**

   ```sh
   daypane login
   ```

   Your browser opens; sign in, accept the "unverified app" warning if shown, and grant read-only calendar access. daypane uses a loopback redirect with PKCE and stores the refresh token in the Secret Service keyring.
8. **Choose calendars.**

   ```sh
   daypane calendars                      # list calendars and which are selected
   daypane calendars --select "Work"      # show a calendar (id or exact name)
   daypane calendars --deselect "Holidays in India"
   ```

   The daemon syncs every `sync_interval_seconds` (default 300).

## Waybar

Add the module printed by `daypane install` to your Waybar config (`~/.config/waybar/config.jsonc`) and list it in a `modules-*` array:

```jsonc
{
    "modules-right": ["custom/daypane", "clock"],

    "custom/daypane": {
        "exec": "cat /run/user/1000/daypane/waybar.json",
        "return-type": "json",
        "interval": 30,
        "signal": 8,
        "on-click": "/home/you/.cargo/bin/daypane popup",
        "tooltip": true
    }
}
```

(Use the exact output of `daypane install --print`; the paths above are examples.) The daemon rewrites the JSON file and sends `SIGRTMIN+<signal>` to Waybar after every change, so the module updates immediately; `interval` is only a safety net. The `signal` must match `[waybar] signal` in `config.toml` (default `8`).

Style it in `~/.config/waybar/style.css`:

```css
#custom-daypane {
    padding: 0 8px;
}

#custom-daypane.overdue {
    color: #f38ba8;
}
```

The module text is the next-event countdown, or the open-task count when nothing is upcoming; the tooltip is today's agenda; the `overdue` class is set while overdue tasks exist. Left-click toggles the popup.

### Without systemd

Any way of running `daypane daemon` in your graphical session works:

```sh
# sway / i3
exec daypane daemon
# Hyprland
exec-once = daypane daemon
```

`daypane install --no-systemd` prints these lines with the absolute binary path filled in.

## Keybindings (popup)

| Key                  | Action                                  |
| -------------------- | --------------------------------------- |
| `h` `j` `k` `l`, arrows | Move between days                    |
| `Tab`                | Switch pane (calendar ⇄ tasks)          |
| `j` / `k` (tasks pane) | Move between tasks                    |
| `x`                  | Complete / reopen task                  |
| `e`                  | Edit task                               |
| `d`                  | Move task to trash                      |
| `u`                  | Undo (restore last trashed task)        |
| `t`                  | Jump to today                           |
| `N`                  | Quick capture                           |
| `/`                  | Search tasks and cached events          |
| `Esc`                | Close                                   |

Running `daypane popup` while the popup is open toggles it closed. It also closes when it loses focus.

## Quick capture syntax

Used by `N` in the popup and by `daypane add`. Everything is parsed locally; English only.

| Input                                          | Result                                           |
| ---------------------------------------------- | ------------------------------------------------ |
| `call mom tomorrow`                            | due tomorrow, no time                            |
| `standup mon 5pm`                              | due next Monday at 17:00                         |
| `renew passport in 3 days`                     | due in three days                                |
| `dentist nov 20 9:30am`                        | due 20 November at 09:30                         |
| `pay rent fri 5pm #home !high`                 | tag `home`, priority high, due Friday 17:00      |
| `read https://example.com/post #later !low`    | URL stored as a link, tag `later`, priority low  |
| `buy milk`                                     | no date → lands in the Inbox                     |

`#tag` adds a tag, `!high` / `!med` / `!low` sets the priority, and bare URLs become links.

## CLI reference

```
daypane <COMMAND>
```

| Command                    | Description                                                              |
| -------------------------- | ------------------------------------------------------------------------ |
| `popup`                    | Toggle the popup window (single instance). Requires the `gui` build.     |
| `daemon`                   | Background service: Google sync, reminders, Waybar output.               |
| `mcp`                      | MCP server over stdio (tasks only).                                      |
| `install [--print] [--no-systemd]` | Install the unit and `.desktop` file, print Waybar snippets.     |
| `login`                    | Authorize Google Calendar (read-only) in the browser.                    |
| `calendars [--select ID…] [--deselect ID…]` | List calendars, or choose which are shown (id or exact name). |
| `add <text…>`              | Add a task using capture syntax.                                         |
| `today [--json]`           | Today's agenda: events, overdue, due today, inbox.                       |
| `next [--json]`            | The next upcoming event or timed task.                                   |
| `done <id>`                | Mark a task complete.                                                    |
| `reopen <id>`              | Reopen a completed task.                                                 |
| `rm <id>`                  | Move a task to trash.                                                    |
| `restore <id>`             | Restore a task from trash.                                               |
| `ls [filters]`             | List tasks.                                                              |

`ls` filters (combine freely):

| Flag                  | Meaning                                                  |
| --------------------- | -------------------------------------------------------- |
| `--date <DATE>`       | Tasks due on that date: `YYYY-MM-DD` or capture words (`tomorrow`, `fri`, `nov 20`) |
| `--tag <TAG>`         | Tasks carrying the tag                                   |
| `--overdue`           | Only open tasks due before today (not combinable with `--status`) |
| `--status <STATUS>`   | `open` (default), `completed`, `deleted` (trash) or `all` |
| `--query`, `-q <TEXT>`| Case-insensitive substring over title, notes and tags    |
| `--inbox`             | Only undated tasks (not combinable with `--date`)        |
| `--json`              | Machine-readable output                                  |

Run `daypane <command> --help` for the exact accepted values.

## MCP server (for AI agents)

`daypane mcp` speaks MCP over stdio and opens the shared SQLite database directly, so changes show up live in the popup and the Waybar module. Scope is **tasks only**; agents get no access to your calendar.

**Claude Code:**

```sh
claude mcp add daypane -- daypane mcp
```

**Any other MCP client** (generic `mcpServers` config):

```json
{
  "mcpServers": {
    "daypane": {
      "command": "daypane",
      "args": ["mcp"]
    }
  }
}
```

Use an absolute path for `command` if the binary isn't on the client's `PATH` (e.g. `~/.cargo/bin/daypane`).

Tools:

| Tool            | Description                                                                |
| --------------- | -------------------------------------------------------------------------- |
| `list_tasks`    | List tasks; filters: `date`, `tag`, `status`, `overdue`, `query`           |
| `get_task`      | Fetch one task by id                                                       |
| `create_task`   | Create a task (title, due date/time, notes, priority, tags, links)         |
| `update_task`   | Change any field of a task                                                 |
| `complete_task` | Mark a task done                                                           |
| `reopen_task`   | Reopen a completed task                                                    |
| `delete_task`   | Soft-delete (move to trash)                                                |
| `restore_task`  | Restore a trashed task                                                     |

## Configuration reference

`~/.config/daypane/config.toml` (`$XDG_CONFIG_HOME/daypane/config.toml`). A missing file means all defaults; every key is optional; unknown keys are an error. `daypane install` writes a commented file with these defaults if none exists.

| Section       | Key                    | Default | Description                                                              |
| ------------- | ---------------------- | ------- | ------------------------------------------------------------------------ |
| `[popup]`     | `margin_top`           | `8`     | Distance from the top screen edge (px)                                   |
|               | `margin_right`         | `8`     | Distance from the right screen edge (px)                                 |
|               | `width`                | `760`   | Popup width (px)                                                         |
|               | `height`               | `480`   | Popup height (px)                                                        |
| `[reminders]` | `task_offset_minutes`  | `10`    | Minutes before a task's `due_time` to notify                             |
|               | `snooze_minutes`       | `10`    | Duration of the notification "Snooze" action                             |
| `[google]`    | `client_id`            | unset   | OAuth "Desktop app" client ID from your own Google Cloud project         |
|               | `client_secret`        | unset   | The matching client secret                                               |
|               | `sync_interval_seconds`| `300`   | Seconds between incremental syncs                                        |
| `[waybar]`    | `signal`               | `8`     | Waybar `signal` number; the daemon sends `SIGRTMIN+signal` after writes  |

## Theming

The popup follows your GTK theme. To override or extend it, create `~/.config/daypane/style.css`; it is loaded on top of the built-in stylesheet. Available classes:

`.overdue` `.today` `.selected-day` `.priority-high` `.priority-med` `.priority-low`

These and the rest of the widget classes are documented in [`src/popup/default.css`](src/popup/default.css), which is also the best starting point for your own overrides.

```css
.overdue { color: #f38ba8; }
.priority-high { border-left: 3px solid #f38ba8; }
```

## Files

| Path                                                        | Purpose                                      |
| ----------------------------------------------------------- | -------------------------------------------- |
| `$XDG_CONFIG_HOME/daypane/config.toml`                      | Configuration                                |
| `$XDG_CONFIG_HOME/daypane/style.css`                        | Optional popup theme override                |
| `$XDG_DATA_HOME/daypane/daypane.db`                         | SQLite database (tasks, cached events)       |
| `$XDG_RUNTIME_DIR/daypane/waybar.json`                      | Waybar module output written by the daemon   |
| `$XDG_CONFIG_HOME/systemd/user/daypane.service`             | systemd user unit                            |
| `$XDG_DATA_HOME/applications/io.github.TanKaizokuO.daypane.desktop` | Desktop entry (notifications)        |

(Defaults: `~/.config`, `~/.local/share`, `/run/user/<uid>`.)

## Privacy

- Google access is **read-only** (`calendar.readonly` scope); daypane never writes to your calendar.
- The OAuth refresh token is stored in the Secret Service keyring (GNOME Keyring, KeePassXC, …), never in a file.
- Nothing leaves your machine except calls to the Google APIs you authorized. No telemetry, no hosted backend, no bundled OAuth client.
- Tasks live only in the local SQLite database. The MCP server exposes tasks, not calendar data, and only to the process that spawns it.

## Development

```sh
scripts/check-name.sh                                       # app name only defined in Cargo.toml
cargo fmt --check
cargo clippy --no-default-features --all-targets -- -D warnings
cargo test --no-default-features
```

The project name is defined **once**, in `Cargo.toml` (`[package] name`). Rust code derives every identifier (binary, config dir, D-Bus/app ID, systemd unit, Waybar module) from it in `src/app.rs`, and `install` renders the unit, `.desktop` file, and Waybar snippets from templates in `assets/`. `scripts/check-name.sh` fails CI if the name is hard-coded anywhere else (Markdown prose excepted), so renaming the project is a one-line change.

## License

[MIT](LICENSE) © 2026 TanKaizokuO
