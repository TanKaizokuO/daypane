//! Shared SQLite store (WAL). Opened concurrently by popup, daemon, MCP server, and CLI.
//!
//! Tasks are owned by the app. Calendar tables are a read-only cache of Google data,
//! written only by the daemon's sync.
//!
//! Conventions: `due_date` = local `YYYY-MM-DD`; `due_time` = local `HH:MM`;
//! audit timestamps (`created_at`, `completed_at`, …) = UTC RFC 3339;
//! event `start_ts`/`end_ts` = unix seconds (all-day events: local midnight).

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, NaiveTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

/// Trash retention; older soft-deleted tasks are purged.
pub const TRASH_RETENTION_DAYS: i64 = 30;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id           INTEGER PRIMARY KEY,
    title        TEXT NOT NULL,
    due_date     TEXT,
    due_time     TEXT,
    notes        TEXT NOT NULL DEFAULT '',
    priority     INTEGER NOT NULL DEFAULT 0,
    tags         TEXT NOT NULL DEFAULT '[]',
    links        TEXT NOT NULL DEFAULT '[]',
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    completed_at TEXT,
    deleted_at   TEXT,
    CHECK (due_time IS NULL OR due_date IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS tasks_due ON tasks(due_date);

CREATE TABLE IF NOT EXISTS calendars (
    id                TEXT PRIMARY KEY,
    summary           TEXT NOT NULL,
    color             TEXT,
    selected          INTEGER NOT NULL DEFAULT 1,
    sync_token        TEXT,
    default_reminders TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS events (
    calendar_id   TEXT NOT NULL REFERENCES calendars(id) ON DELETE CASCADE,
    id            TEXT NOT NULL,
    summary       TEXT NOT NULL,
    description   TEXT NOT NULL DEFAULT '',
    location      TEXT,
    all_day       INTEGER NOT NULL,
    start_ts      INTEGER NOT NULL,
    end_ts        INTEGER NOT NULL,
    meet_link     TEXT,
    html_link     TEXT,
    use_default_reminders INTEGER NOT NULL DEFAULT 1,
    reminders     TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (calendar_id, id)
);
CREATE INDEX IF NOT EXISTS events_span ON events(start_ts, end_ts);

CREATE TABLE IF NOT EXISTS fired_reminders (
    key      TEXT PRIMARY KEY,
    fired_at INTEGER NOT NULL
);
"#;

// ─── Tasks ──────────────────────────────────────────────────────────────────

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    #[default]
    None,
    Low,
    Med,
    High,
}

impl Priority {
    fn to_i64(self) -> i64 {
        self as i64
    }
    fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::Low,
            2 => Self::Med,
            3 => Self::High,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Task {
    pub id: i64,
    pub title: String,
    /// Local date, `YYYY-MM-DD`.
    pub due_date: Option<NaiveDate>,
    /// Local time, `HH:MM`. Only set when `due_date` is.
    #[serde(with = "hhmm_opt")]
    #[schemars(with = "Option<String>")]
    pub due_time: Option<NaiveTime>,
    /// Markdown.
    pub notes: String,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub deleted_at: Option<DateTime<Utc>>,
}

impl Task {
    pub fn is_open(&self) -> bool {
        self.completed_at.is_none() && self.deleted_at.is_none()
    }
    #[cfg_attr(not(feature = "gui"), allow(dead_code))] // used by the popup
    pub fn is_overdue(&self, today: NaiveDate) -> bool {
        self.is_open() && self.due_date.is_some_and(|d| d < today)
    }
}

/// Fields for a new task. `due_time` without `due_date` is rejected.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NewTask {
    pub title: String,
    pub due_date: Option<NaiveDate>,
    #[serde(with = "hhmm_opt", default)]
    pub due_time: Option<NaiveTime>,
    pub notes: String,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub links: Vec<String>,
}

/// Partial update. Outer `None` = leave unchanged; `Some(None)` = clear.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskPatch {
    pub title: Option<String>,
    pub due_date: Option<Option<NaiveDate>>,
    pub due_time: Option<Option<NaiveTime>>,
    pub notes: Option<String>,
    pub priority: Option<Priority>,
    pub tags: Option<Vec<String>>,
    pub links: Option<Vec<String>>,
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not completed, not deleted.
    #[default]
    Open,
    /// Completed, not deleted.
    Completed,
    /// In trash.
    Deleted,
    /// Everything not in trash.
    All,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskFilter {
    pub status: Status,
    /// Exactly this due date.
    pub date: Option<NaiveDate>,
    /// Only undated tasks (Inbox).
    pub undated: bool,
    /// Open tasks with `due_date < today`. Requires `today`.
    pub overdue: bool,
    pub today: Option<NaiveDate>,
    pub tag: Option<String>,
    /// Case-insensitive substring over title, notes, tags.
    pub query: Option<String>,
}

/// The popup's default right pane / `today` CLI.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TodayView {
    pub overdue: Vec<Task>,
    pub due_today: Vec<Task>,
    pub inbox: Vec<Task>,
}

// ─── Calendar cache ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reminder {
    /// Minutes before event start.
    pub minutes: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    pub id: String,
    pub summary: String,
    pub color: Option<String>,
    pub selected: bool,
    pub sync_token: Option<String>,
    /// Calendar-level default popup reminders.
    pub default_reminders: Vec<Reminder>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub calendar_id: String,
    pub id: String,
    pub summary: String,
    pub description: String,
    pub location: Option<String>,
    pub all_day: bool,
    pub start_ts: i64,
    /// Exclusive.
    pub end_ts: i64,
    pub meet_link: Option<String>,
    pub html_link: Option<String>,
    /// When true, the calendar's `default_reminders` apply instead of `reminders`.
    pub use_default_reminders: bool,
    pub reminders: Vec<Reminder>,
}

// ─── Store ──────────────────────────────────────────────────────────────────

pub struct Db {
    conn: Connection,
}

fn now() -> DateTime<Utc> {
    // Second precision keeps RFC 3339 round-trips exact.
    DateTime::from_timestamp(Utc::now().timestamp(), 0).expect("valid timestamp")
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).expect("serializable")
}

fn validate(title: &str, due_date: Option<NaiveDate>, due_time: Option<NaiveTime>) -> Result<()> {
    if title.trim().is_empty() {
        bail!("task title must not be empty");
    }
    if due_time.is_some() && due_date.is_none() {
        bail!("due_time requires due_date");
    }
    Ok(())
}

fn normalize_tags(tags: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(tags.len());
    for t in tags {
        let t = t.trim().trim_start_matches('#').to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

const TASK_COLS: &str = "id, title, due_date, due_time, notes, priority, tags, links, \
                         created_at, updated_at, completed_at, deleted_at";

fn task_from_row(r: &Row) -> rusqlite::Result<Task> {
    fn parse<T, E: std::error::Error + Send + Sync + 'static>(
        i: usize,
        v: std::result::Result<T, E>,
    ) -> rusqlite::Result<T> {
        v.map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(i, rusqlite::types::Type::Text, Box::new(e))
        })
    }
    let date = |i: usize, s: Option<String>| -> rusqlite::Result<Option<NaiveDate>> {
        s.map(|s| parse(i, NaiveDate::parse_from_str(&s, "%Y-%m-%d")))
            .transpose()
    };
    let time = |i: usize, s: Option<String>| -> rusqlite::Result<Option<NaiveTime>> {
        s.map(|s| parse(i, NaiveTime::parse_from_str(&s, "%H:%M")))
            .transpose()
    };
    let stamp = |i: usize, s: Option<String>| -> rusqlite::Result<Option<DateTime<Utc>>> {
        s.map(|s| parse(i, DateTime::parse_from_rfc3339(&s)).map(|d| d.with_timezone(&Utc)))
            .transpose()
    };
    let list = |i: usize, s: String| -> rusqlite::Result<Vec<String>> {
        parse(i, serde_json::from_str(&s))
    };
    Ok(Task {
        id: r.get(0)?,
        title: r.get(1)?,
        due_date: date(2, r.get(2)?)?,
        due_time: time(3, r.get(3)?)?,
        notes: r.get(4)?,
        priority: Priority::from_i64(r.get(5)?),
        tags: list(6, r.get(6)?)?,
        links: list(7, r.get(7)?)?,
        created_at: stamp(8, Some(r.get(8)?))?.expect("some"),
        updated_at: stamp(9, Some(r.get(9)?))?.expect("some"),
        completed_at: stamp(10, r.get(10)?)?,
        deleted_at: stamp(11, r.get(11)?)?,
    })
}

fn event_from_row(r: &Row) -> rusqlite::Result<Event> {
    let reminders: String = r.get(11)?;
    Ok(Event {
        calendar_id: r.get(0)?,
        id: r.get(1)?,
        summary: r.get(2)?,
        description: r.get(3)?,
        location: r.get(4)?,
        all_day: r.get(5)?,
        start_ts: r.get(6)?,
        end_ts: r.get(7)?,
        meet_link: r.get(8)?,
        html_link: r.get(9)?,
        use_default_reminders: r.get(10)?,
        reminders: serde_json::from_str(&reminders).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(e))
        })?,
    })
}

const EVENT_COLS: &str = "e.calendar_id, e.id, e.summary, e.description, e.location, e.all_day, \
                          e.start_ts, e.end_ts, e.meet_link, e.html_link, e.use_default_reminders, e.reminders";

/// Sort: dated before undated; by date, time (untimed last within a day), priority desc, id.
const TASK_ORDER: &str =
    "ORDER BY due_date IS NULL, due_date, due_time IS NULL, due_time, priority DESC, id";

impl Db {
    /// Open the default XDG database, creating directories and schema as needed.
    pub fn open_default() -> Result<Self> {
        let path = crate::paths::db_file()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Self::open(&path)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<()> {
        let v: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if v > SCHEMA_VERSION {
            bail!("database schema v{v} is newer than this binary (v{SCHEMA_VERSION})");
        }
        self.conn.execute_batch(SCHEMA)?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    // ── tasks ──

    pub fn create_task(&self, t: NewTask) -> Result<Task> {
        validate(&t.title, t.due_date, t.due_time)?;
        let now = ts(now());
        self.conn.execute(
            "INSERT INTO tasks (title, due_date, due_time, notes, priority, tags, links, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)",
            params![
                t.title.trim(),
                t.due_date.map(|d| d.format("%Y-%m-%d").to_string()),
                t.due_time.map(|t| t.format("%H:%M").to_string()),
                t.notes,
                t.priority.to_i64(),
                json(&normalize_tags(t.tags)),
                json(&t.links),
                now,
            ],
        )?;
        self.task_or_err(self.conn.last_insert_rowid())
    }

    /// Any task, including completed and trashed.
    pub fn get_task(&self, id: i64) -> Result<Option<Task>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?1"),
                [id],
                task_from_row,
            )
            .optional()?)
    }

    fn task_or_err(&self, id: i64) -> Result<Task> {
        self.get_task(id)?
            .with_context(|| format!("no task with id {id}"))
    }

    pub fn update_task(&self, id: i64, p: TaskPatch) -> Result<Task> {
        let cur = self.task_or_err(id)?;
        let title = p.title.unwrap_or(cur.title);
        let due_date = p.due_date.unwrap_or(cur.due_date);
        // Clearing the date clears the time unless a time is explicitly set (which then fails validation).
        let due_time = match p.due_time {
            Some(t) => t,
            None if due_date.is_none() => None,
            None => cur.due_time,
        };
        validate(&title, due_date, due_time)?;
        self.conn.execute(
            "UPDATE tasks SET title = ?2, due_date = ?3, due_time = ?4, notes = ?5, priority = ?6,
                              tags = ?7, links = ?8, updated_at = ?9 WHERE id = ?1",
            params![
                id,
                title.trim(),
                due_date.map(|d| d.format("%Y-%m-%d").to_string()),
                due_time.map(|t| t.format("%H:%M").to_string()),
                p.notes.unwrap_or(cur.notes),
                p.priority.unwrap_or(cur.priority).to_i64(),
                json(&normalize_tags(p.tags.unwrap_or(cur.tags))),
                json(&p.links.unwrap_or(cur.links)),
                ts(now()),
            ],
        )?;
        self.task_or_err(id)
    }

    fn set_stamp(&self, id: i64, col: &str, value: Option<String>) -> Result<Task> {
        self.task_or_err(id)?;
        self.conn.execute(
            &format!("UPDATE tasks SET {col} = ?2, updated_at = ?3 WHERE id = ?1"),
            params![id, value, ts(now())],
        )?;
        self.task_or_err(id)
    }

    pub fn complete_task(&self, id: i64) -> Result<Task> {
        let t = self.task_or_err(id)?;
        if t.completed_at.is_some() {
            return Ok(t);
        }
        self.set_stamp(id, "completed_at", Some(ts(now())))
    }

    pub fn reopen_task(&self, id: i64) -> Result<Task> {
        self.set_stamp(id, "completed_at", None)
    }

    /// Soft delete (move to trash).
    pub fn delete_task(&self, id: i64) -> Result<Task> {
        let t = self.task_or_err(id)?;
        if t.deleted_at.is_some() {
            return Ok(t);
        }
        self.set_stamp(id, "deleted_at", Some(ts(now())))
    }

    /// Restore from trash.
    pub fn restore_task(&self, id: i64) -> Result<Task> {
        self.set_stamp(id, "deleted_at", None)
    }

    /// Permanently remove tasks trashed more than [`TRASH_RETENTION_DAYS`] ago. Returns count.
    pub fn purge_trash(&self) -> Result<usize> {
        let cutoff = ts(now() - chrono::Duration::days(TRASH_RETENTION_DAYS));
        Ok(self.conn.execute(
            "DELETE FROM tasks WHERE deleted_at IS NOT NULL AND deleted_at < ?1",
            [cutoff],
        )?)
    }

    pub fn list_tasks(&self, f: &TaskFilter) -> Result<Vec<Task>> {
        let mut sql = format!("SELECT {TASK_COLS} FROM tasks WHERE 1");
        let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        sql.push_str(match f.status {
            Status::Open => " AND completed_at IS NULL AND deleted_at IS NULL",
            Status::Completed => " AND completed_at IS NOT NULL AND deleted_at IS NULL",
            Status::Deleted => " AND deleted_at IS NOT NULL",
            Status::All => " AND deleted_at IS NULL",
        });
        if let Some(d) = f.date {
            args.push(Box::new(d.format("%Y-%m-%d").to_string()));
            sql.push_str(&format!(" AND due_date = ?{}", args.len()));
        }
        if f.undated {
            sql.push_str(" AND due_date IS NULL");
        }
        if f.overdue {
            let today = f.today.context("overdue filter requires `today`")?;
            args.push(Box::new(today.format("%Y-%m-%d").to_string()));
            sql.push_str(&format!(
                " AND completed_at IS NULL AND deleted_at IS NULL AND due_date < ?{}",
                args.len()
            ));
        }
        if let Some(tag) = &f.tag {
            args.push(Box::new(tag.trim().trim_start_matches('#').to_lowercase()));
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_each(tasks.tags) WHERE value = ?{})",
                args.len()
            ));
        }
        if let Some(q) = f.query.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            let escaped = q
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            args.push(Box::new(format!("%{escaped}%")));
            let n = args.len();
            sql.push_str(&format!(
                " AND (lower(title) LIKE ?{n} ESCAPE '\\' OR lower(notes) LIKE ?{n} ESCAPE '\\' \
                   OR lower(tags) LIKE ?{n} ESCAPE '\\')"
            ));
        }
        sql.push(' ');
        sql.push_str(TASK_ORDER);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn today_view(&self, today: NaiveDate) -> Result<TodayView> {
        Ok(TodayView {
            overdue: self.list_tasks(&TaskFilter {
                overdue: true,
                today: Some(today),
                ..Default::default()
            })?,
            due_today: self.list_tasks(&TaskFilter {
                date: Some(today),
                ..Default::default()
            })?,
            inbox: self.list_tasks(&TaskFilter {
                undated: true,
                ..Default::default()
            })?,
        })
    }

    /// Open tasks with a `due_time` (reminder candidates) on or after `from`.
    pub fn timed_tasks_from(&self, from: NaiveDate) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TASK_COLS} FROM tasks WHERE completed_at IS NULL AND deleted_at IS NULL
             AND due_time IS NOT NULL AND due_date >= ?1 {TASK_ORDER}"
        ))?;
        let rows = stmt.query_map([from.format("%Y-%m-%d").to_string()], task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ── calendars ──

    /// Insert or update metadata; preserves `selected` and `sync_token` for existing rows.
    pub fn upsert_calendar(
        &self,
        id: &str,
        summary: &str,
        color: Option<&str>,
        default_reminders: &[Reminder],
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO calendars (id, summary, color, default_reminders) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET summary = excluded.summary, color = excluded.color,
                                           default_reminders = excluded.default_reminders",
            params![id, summary, color, json(&default_reminders)],
        )?;
        Ok(())
    }

    pub fn calendars(&self) -> Result<Vec<Calendar>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, summary, color, selected, sync_token, default_reminders FROM calendars ORDER BY summary")?;
        let rows = stmt.query_map([], |r| {
            let rem: String = r.get(5)?;
            Ok(Calendar {
                id: r.get(0)?,
                summary: r.get(1)?,
                color: r.get(2)?,
                selected: r.get(3)?,
                sync_token: r.get(4)?,
                default_reminders: serde_json::from_str(&rem).unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_calendar_selected(&self, id: &str, selected: bool) -> Result<()> {
        if self.conn.execute(
            "UPDATE calendars SET selected = ?2 WHERE id = ?1",
            params![id, selected],
        )? == 0
        {
            bail!("no calendar with id {id}");
        }
        Ok(())
    }

    /// Remove calendars no longer present upstream (cascades to events).
    pub fn retain_calendars(&self, ids: &[String]) -> Result<()> {
        let keep = json(&ids);
        self.conn.execute(
            "DELETE FROM calendars WHERE id NOT IN (SELECT value FROM json_each(?1))",
            [keep],
        )?;
        Ok(())
    }

    pub fn set_sync_token(&self, calendar_id: &str, token: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE calendars SET sync_token = ?2 WHERE id = ?1",
            params![calendar_id, token],
        )?;
        Ok(())
    }

    /// Drop all cached events of a calendar (before a full resync after `410 Gone`).
    pub fn clear_events(&self, calendar_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM events WHERE calendar_id = ?1", [calendar_id])?;
        Ok(())
    }

    // ── events ──

    pub fn upsert_event(&self, e: &Event) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO events (calendar_id, id, summary, description, location, all_day, start_ts,
                 end_ts, meet_link, html_link, use_default_reminders, reminders)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                e.calendar_id,
                e.id,
                e.summary,
                e.description,
                e.location,
                e.all_day,
                e.start_ts,
                e.end_ts,
                e.meet_link,
                e.html_link,
                e.use_default_reminders,
                json(&e.reminders),
            ],
        )?;
        Ok(())
    }

    pub fn delete_event(&self, calendar_id: &str, id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM events WHERE calendar_id = ?1 AND id = ?2",
            params![calendar_id, id],
        )?;
        Ok(())
    }

    /// Events of selected calendars overlapping `[from_ts, to_ts)`, by start time.
    pub fn events_between(&self, from_ts: i64, to_ts: i64) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events e JOIN calendars c ON c.id = e.calendar_id
             WHERE c.selected AND e.start_ts < ?2 AND e.end_ts > ?1
             ORDER BY e.all_day DESC, e.start_ts, e.summary"
        ))?;
        let rows = stmt.query_map([from_ts, to_ts], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Events of selected calendars overlapping a local calendar day.
    pub fn events_on(&self, day: NaiveDate) -> Result<Vec<Event>> {
        let (a, b) = local_day_bounds(day);
        self.events_between(a, b)
    }

    /// Case-insensitive substring search over cached events of selected calendars.
    #[cfg_attr(not(feature = "gui"), allow(dead_code))] // used by the popup
    pub fn search_events(&self, query: &str) -> Result<Vec<Event>> {
        let q = format!(
            "%{}%",
            query
                .trim()
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EVENT_COLS} FROM events e JOIN calendars c ON c.id = e.calendar_id
             WHERE c.selected AND (lower(e.summary) LIKE ?1 ESCAPE '\\' OR lower(e.description) LIKE ?1 ESCAPE '\\'
                                   OR lower(coalesce(e.location, '')) LIKE ?1 ESCAPE '\\')
             ORDER BY e.start_ts"
        ))?;
        let rows = stmt.query_map([q], event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ── reminder bookkeeping ──

    /// Records `key` as fired. Returns false if it already was.
    pub fn mark_fired(&self, key: &str, at: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "INSERT OR IGNORE INTO fired_reminders (key, fired_at) VALUES (?1, ?2)",
            params![key, at],
        )? == 1)
    }

    pub fn prune_fired(&self, older_than_ts: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM fired_reminders WHERE fired_at < ?1",
            [older_than_ts],
        )?;
        Ok(())
    }
}

/// Unix-second bounds `[start, end)` of a local calendar day (DST-safe).
pub fn local_day_bounds(day: NaiveDate) -> (i64, i64) {
    let at = |d: NaiveDate| {
        d.and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_local_timezone(chrono::Local)
            .earliest()
            .map(|t| t.timestamp())
            // Midnight skipped by DST: fall back to 01:00.
            .unwrap_or_else(|| {
                d.and_hms_opt(1, 0, 0)
                    .expect("1am")
                    .and_local_timezone(chrono::Local)
                    .unwrap()
                    .timestamp()
            })
    };
    (at(day), at(day.succ_opt().expect("date in range")))
}

/// Serde helper: `Option<NaiveTime>` as `"HH:MM"`.
pub mod hhmm_opt {
    use chrono::NaiveTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<NaiveTime>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&t.format("%H:%M").to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<NaiveTime>, D::Error> {
        Option::<String>::deserialize(d)?
            .map(|s| {
                NaiveTime::parse_from_str(&s, "%H:%M")
                    .or_else(|_| NaiveTime::parse_from_str(&s, "%H:%M:%S"))
                    .map_err(serde::de::Error::custom)
            })
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn new(title: &str, date: Option<&str>) -> NewTask {
        NewTask {
            title: title.into(),
            due_date: date.map(d),
            ..Default::default()
        }
    }

    #[test]
    fn today_view_partitions_and_orders() {
        let db = Db::open_in_memory().unwrap();
        let today = d("2026-11-13");
        let late = db.create_task(new("late", Some("2026-11-10"))).unwrap();
        let mut now_hi = new("hi", Some("2026-11-13"));
        now_hi.priority = Priority::High;
        let mut now_timed = new("timed", Some("2026-11-13"));
        now_timed.due_time = NaiveTime::from_hms_opt(9, 0, 0);
        let hi = db.create_task(now_hi).unwrap();
        let timed = db.create_task(now_timed).unwrap();
        let inbox = db.create_task(new("inbox", None)).unwrap();
        let done = db
            .create_task(new("done late", Some("2026-11-01")))
            .unwrap();
        db.complete_task(done.id).unwrap();
        db.create_task(new("future", Some("2026-11-20"))).unwrap();

        let v = db.today_view(today).unwrap();
        let ids = |ts: &[Task]| ts.iter().map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(ids(&v.overdue), [late.id]);
        assert_eq!(
            ids(&v.due_today),
            [timed.id, hi.id],
            "timed first, then by priority"
        );
        assert_eq!(ids(&v.inbox), [inbox.id]);
    }

    #[test]
    fn soft_delete_restore_and_filters() {
        let db = Db::open_in_memory().unwrap();
        let mut t = new("Buy Milk", None);
        t.tags = vec!["#Errands".into(), "errands".into()];
        t.notes = "50% off".into();
        let t = db.create_task(t).unwrap();
        assert_eq!(t.tags, ["errands"], "tags normalized and deduped");

        let by_tag = TaskFilter {
            tag: Some("ERRANDS".into()),
            ..Default::default()
        };
        assert_eq!(db.list_tasks(&by_tag).unwrap().len(), 1);
        let q = |s: &str| TaskFilter {
            query: Some(s.into()),
            ..Default::default()
        };
        assert_eq!(db.list_tasks(&q("milk")).unwrap().len(), 1);
        assert_eq!(
            db.list_tasks(&q("%")).unwrap().len(),
            1,
            "literal percent in notes"
        );
        assert_eq!(
            db.list_tasks(&q("_")).unwrap().len(),
            0,
            "underscore is literal, not wildcard"
        );

        db.delete_task(t.id).unwrap();
        assert!(db.list_tasks(&TaskFilter::default()).unwrap().is_empty());
        assert_eq!(
            db.list_tasks(&TaskFilter {
                status: Status::Deleted,
                ..Default::default()
            })
            .unwrap()
            .len(),
            1
        );
        db.restore_task(t.id).unwrap();
        assert_eq!(db.list_tasks(&TaskFilter::default()).unwrap().len(), 1);
    }

    #[test]
    fn purge_only_old_trash() {
        let db = Db::open_in_memory().unwrap();
        let old = db.create_task(new("old", None)).unwrap();
        let fresh = db.create_task(new("fresh", None)).unwrap();
        db.delete_task(fresh.id).unwrap();
        db.conn
            .execute(
                "UPDATE tasks SET deleted_at = '2000-01-01T00:00:00Z' WHERE id = ?1",
                [old.id],
            )
            .unwrap();
        assert_eq!(db.purge_trash().unwrap(), 1);
        assert!(db.get_task(old.id).unwrap().is_none());
        assert!(db.get_task(fresh.id).unwrap().is_some());
    }

    #[test]
    fn time_requires_date() {
        let db = Db::open_in_memory().unwrap();
        let mut t = new("x", None);
        t.due_time = NaiveTime::from_hms_opt(9, 0, 0);
        assert!(db.create_task(t).is_err());

        let mut t = new("y", Some("2026-11-13"));
        t.due_time = NaiveTime::from_hms_opt(9, 0, 0);
        let t = db.create_task(t).unwrap();
        let cleared = db
            .update_task(
                t.id,
                TaskPatch {
                    due_date: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            (cleared.due_date, cleared.due_time),
            (None, None),
            "clearing date clears time"
        );
    }

    #[test]
    fn events_respect_selection_and_overlap() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_calendar("a", "A", None, &[]).unwrap();
        db.upsert_calendar("b", "B", None, &[]).unwrap();
        let ev = |cal: &str, id: &str, s: i64, e: i64| Event {
            calendar_id: cal.into(),
            id: id.into(),
            summary: id.into(),
            description: String::new(),
            location: None,
            all_day: false,
            start_ts: s,
            end_ts: e,
            meet_link: None,
            html_link: None,
            use_default_reminders: true,
            reminders: vec![],
        };
        db.upsert_event(&ev("a", "in", 100, 200)).unwrap();
        db.upsert_event(&ev("a", "touching", 200, 300)).unwrap();
        db.upsert_event(&ev("b", "hidden", 100, 200)).unwrap();
        db.set_calendar_selected("b", false).unwrap();
        let got: Vec<_> = db
            .events_between(150, 200)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(got, ["in"]);
        db.upsert_calendar("a", "A renamed", None, &[]).unwrap();
        db.set_sync_token("a", Some("tok")).unwrap();
        db.retain_calendars(&["a".into()]).unwrap();
        let cals = db.calendars().unwrap();
        assert_eq!(
            (cals.len(), cals[0].sync_token.as_deref()),
            (1, Some("tok"))
        );
    }

    #[test]
    fn mark_fired_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.mark_fired("k", 1).unwrap());
        assert!(!db.mark_fired("k", 2).unwrap());
    }
}
