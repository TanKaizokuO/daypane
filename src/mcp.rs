//! MCP server over stdio (`APP_NAME mcp`), spawned by an agent. Tasks only; no calendar access.
//!
//! Opens the shared SQLite store directly. stdout carries the protocol and nothing else;
//! diagnostics go to stderr. Tool failures (bad id, validation) are returned as tool results
//! with `is_error = true`, never as protocol errors.

use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use chrono::{Local, NaiveDate, NaiveTime};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ServerCapabilities, ServerConfig},
    schemars::{self, JsonSchema},
    tool, tool_router, Json, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Deserializer, Serialize};

use crate::app::APP_NAME;
use crate::db::{Db, NewTask, Priority, Status, Task, TaskFilter, TaskPatch};

const INSTRUCTIONS: &str = "A local task list (SQLite, shared with the desktop popup and CLI). \
Tasks have a title, optional due_date (YYYY-MM-DD) and due_time (HH:MM, local time, requires a due_date), \
Markdown notes, priority (none|low|med|high), tags and links. Deleting a task moves it to trash \
(restore_task undoes it; trash is purged after 30 days). This server has no calendar access.";

// ─── Parameters ─────────────────────────────────────────────────────────────

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct ListTasksParams {
    /// Only tasks due exactly on this local date (YYYY-MM-DD).
    pub date: Option<String>,
    /// Only tasks carrying this tag (case-insensitive, leading `#` optional).
    pub tag: Option<String>,
    /// open (default) | completed | deleted | all (everything not in trash).
    pub status: Option<Status>,
    /// When true, only open tasks due before today (local date).
    pub overdue: Option<bool>,
    /// Case-insensitive substring match over title, notes and tags.
    pub query: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdParams {
    /// Task id.
    pub id: i64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CreateTaskParams {
    /// Task title (must not be empty).
    pub title: String,
    /// Due date, YYYY-MM-DD (local).
    pub due_date: Option<String>,
    /// Due time, HH:MM (24h, local). Requires `due_date`.
    pub due_time: Option<String>,
    /// Markdown notes.
    pub notes: Option<String>,
    /// none (default) | low | med | high.
    pub priority: Option<Priority>,
    pub tags: Option<Vec<String>>,
    pub links: Option<Vec<String>>,
}

/// Distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UpdateTaskParams {
    /// Task id.
    pub id: i64,
    /// New title.
    pub title: Option<String>,
    /// New due date, YYYY-MM-DD. Explicit `null` clears the date (and the time).
    #[serde(default, deserialize_with = "present")]
    #[schemars(with = "Option<String>")]
    pub due_date: Option<Option<String>>,
    /// New due time, HH:MM. Explicit `null` clears the time.
    #[serde(default, deserialize_with = "present")]
    #[schemars(with = "Option<String>")]
    pub due_time: Option<Option<String>>,
    /// Replaces the notes.
    pub notes: Option<String>,
    /// none | low | med | high.
    pub priority: Option<Priority>,
    /// Replaces all tags.
    pub tags: Option<Vec<String>>,
    /// Replaces all links.
    pub links: Option<Vec<String>>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TaskList {
    pub count: usize,
    pub tasks: Vec<Task>,
}

// ─── Logic (plain functions over `&Db`, unit-testable) ──────────────────────

fn fail(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn parse_date(field: &str, s: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| format!("invalid {field} {s:?}: expected YYYY-MM-DD"))
}

fn parse_time(field: &str, s: &str) -> Result<NaiveTime, String> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M")
        .map_err(|_| format!("invalid {field} {s:?}: expected HH:MM (24h)"))
}

fn list(db: &Db, p: ListTasksParams, today: NaiveDate) -> Result<TaskList, String> {
    let filter = TaskFilter {
        status: p.status.unwrap_or_default(),
        date: p.date.as_deref().map(|s| parse_date("date", s)).transpose()?,
        overdue: p.overdue.unwrap_or(false),
        today: Some(today),
        tag: p.tag,
        query: p.query,
        ..Default::default()
    };
    let tasks = db.list_tasks(&filter).map_err(fail)?;
    Ok(TaskList { count: tasks.len(), tasks })
}

fn create(db: &Db, p: CreateTaskParams) -> Result<Task, String> {
    let new = NewTask {
        title: p.title,
        due_date: p.due_date.as_deref().map(|s| parse_date("due_date", s)).transpose()?,
        due_time: p.due_time.as_deref().map(|s| parse_time("due_time", s)).transpose()?,
        notes: p.notes.unwrap_or_default(),
        priority: p.priority.unwrap_or_default(),
        tags: p.tags.unwrap_or_default(),
        links: p.links.unwrap_or_default(),
    };
    db.create_task(new).map_err(fail)
}

fn update(db: &Db, p: UpdateTaskParams) -> Result<Task, String> {
    let patch = TaskPatch {
        title: p.title,
        due_date: p
            .due_date
            .map(|v| v.as_deref().map(|s| parse_date("due_date", s)).transpose())
            .transpose()?,
        due_time: p
            .due_time
            .map(|v| v.as_deref().map(|s| parse_time("due_time", s)).transpose())
            .transpose()?,
        notes: p.notes,
        priority: p.priority,
        tags: p.tags,
        links: p.links,
    };
    if patch == TaskPatch::default() {
        return Err("no fields to update".into());
    }
    db.update_task(p.id, patch).map_err(fail)
}

// ─── Server ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct TaskServer {
    // `rusqlite::Connection` is `!Sync`; calls are short and serialized by this lock.
    db: Arc<Mutex<Db>>,
    tool_router: ToolRouter<Self>,
}

impl TaskServer {
    fn new(db: Db) -> Self {
        Self { db: Arc::new(Mutex::new(db)), tool_router: Self::tool_router() }
    }

    fn db(&self) -> MutexGuard<'_, Db> {
        // A panic mid-call cannot leave the connection in a bad state; keep serving.
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[tool_router]
impl TaskServer {
    #[tool(
        description = "List tasks, ordered by due date/time then priority. Default: open tasks. \
                       Filters combine with AND."
    )]
    fn list_tasks(&self, Parameters(p): Parameters<ListTasksParams>) -> Result<Json<TaskList>, String> {
        list(&self.db(), p, Local::now().date_naive()).map(Json)
    }

    #[tool(description = "Get one task by id (including completed and trashed tasks).")]
    fn get_task(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Task>, String> {
        let db = self.db();
        match db.get_task(p.id).map_err(fail)? {
            Some(t) => Ok(Json(t)),
            None => Err(format!("no task with id {}", p.id)),
        }
    }

    #[tool(description = "Create a task. Returns the created task.")]
    fn create_task(&self, Parameters(p): Parameters<CreateTaskParams>) -> Result<Json<Task>, String> {
        create(&self.db(), p).map(Json)
    }

    #[tool(
        description = "Update fields of a task; omitted fields are unchanged. \
                       Pass due_date: null to clear the due date (this also clears due_time), \
                       due_time: null to clear only the time. Returns the updated task."
    )]
    fn update_task(&self, Parameters(p): Parameters<UpdateTaskParams>) -> Result<Json<Task>, String> {
        update(&self.db(), p).map(Json)
    }

    #[tool(description = "Mark a task completed. Returns the task.")]
    fn complete_task(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Task>, String> {
        self.db().complete_task(p.id).map(Json).map_err(fail)
    }

    #[tool(description = "Reopen a completed task. Returns the task.")]
    fn reopen_task(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Task>, String> {
        self.db().reopen_task(p.id).map(Json).map_err(fail)
    }

    #[tool(description = "Move a task to trash (soft delete; undo with restore_task). Returns the task.")]
    fn delete_task(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Task>, String> {
        self.db().delete_task(p.id).map(Json).map_err(fail)
    }

    #[tool(description = "Restore a task from trash. Returns the task.")]
    fn restore_task(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Task>, String> {
        self.db().restore_task(p.id).map(Json).map_err(fail)
    }
}

#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for TaskServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(APP_NAME, env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve MCP over stdio until the client closes the connection.
pub fn run() -> Result<()> {
    let db = Db::open_default()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting async runtime")?;
    rt.block_on(async {
        let service = TaskServer::new(db)
            .serve(rmcp::transport::stdio())
            .await
            .context("starting MCP server")?;
        service.waiting().await.context("MCP server")?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn params(v: serde_json::Value) -> UpdateTaskParams {
        serde_json::from_value(v).unwrap()
    }

    fn new_task(db: &Db, title: &str, date: Option<&str>) -> Task {
        create(
            db,
            CreateTaskParams {
                title: title.into(),
                due_date: date.map(Into::into),
                due_time: None,
                notes: None,
                priority: None,
                tags: None,
                links: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn update_params_distinguish_absent_from_null() {
        let p = params(serde_json::json!({"id": 1}));
        assert_eq!((p.due_date, p.due_time), (None, None));
        let p = params(serde_json::json!({"id": 1, "due_date": null}));
        assert_eq!(p.due_date, Some(None));
        assert_eq!(p.due_time, None);
        let p = params(serde_json::json!({"id": 1, "due_date": "2026-11-20", "due_time": "09:30"}));
        assert_eq!(p.due_date, Some(Some("2026-11-20".into())));
        assert_eq!(p.due_time, Some(Some("09:30".into())));
    }

    #[test]
    fn update_clears_and_sets_dates() {
        let db = Db::open_in_memory().unwrap();
        let t = create(
            &db,
            CreateTaskParams {
                title: "x".into(),
                due_date: Some("2026-11-20".into()),
                due_time: Some("09:30".into()),
                notes: None,
                priority: Some(Priority::High),
                tags: Some(vec!["#Home".into()]),
                links: None,
            },
        )
        .unwrap();
        assert_eq!(t.due_date, Some(d("2026-11-20")));
        assert_eq!(t.tags, vec!["home"]);

        // Absent fields are untouched.
        let u = update(&db, params(serde_json::json!({"id": t.id, "title": "y"}))).unwrap();
        assert_eq!((u.title.as_str(), u.due_date, u.priority), ("y", t.due_date, Priority::High));
        assert_eq!(u.due_time, t.due_time);

        // null clears only the time.
        let u = update(&db, params(serde_json::json!({"id": t.id, "due_time": null}))).unwrap();
        assert_eq!((u.due_date, u.due_time), (t.due_date, None));

        // null date clears both.
        update(&db, params(serde_json::json!({"id": t.id, "due_time": "08:00"}))).unwrap();
        let u = update(&db, params(serde_json::json!({"id": t.id, "due_date": null}))).unwrap();
        assert_eq!((u.due_date, u.due_time), (None, None));
    }

    #[test]
    fn errors_are_strings_not_panics() {
        let db = Db::open_in_memory().unwrap();
        assert!(update(&db, params(serde_json::json!({"id": 99, "title": "a"}))).unwrap_err().contains("99"));
        assert!(update(&db, params(serde_json::json!({"id": 99}))).unwrap_err().contains("no fields"));
        let t = new_task(&db, "a", None);
        let e = update(&db, params(serde_json::json!({"id": t.id, "due_date": "20/11/2026"}))).unwrap_err();
        assert!(e.contains("YYYY-MM-DD"), "{e}");
        let e = update(&db, params(serde_json::json!({"id": t.id, "due_time": "9am"}))).unwrap_err();
        assert!(e.contains("HH:MM"), "{e}");
        // due_time on an undated task is rejected by the store.
        let e = update(&db, params(serde_json::json!({"id": t.id, "due_time": "09:00"}))).unwrap_err();
        assert!(e.contains("due_time"), "{e}");
        let e = create(
            &db,
            CreateTaskParams {
                title: "  ".into(),
                due_date: None,
                due_time: None,
                notes: None,
                priority: None,
                tags: None,
                links: None,
            },
        )
        .unwrap_err();
        assert!(e.contains("title"), "{e}");
    }

    #[test]
    fn list_filters_use_supplied_today() {
        let db = Db::open_in_memory().unwrap();
        let past = new_task(&db, "past", Some("2026-11-01"));
        let today = new_task(&db, "today report", Some("2026-11-13"));
        let inbox = new_task(&db, "inbox", None);
        db.complete_task(inbox.id).unwrap();
        let ids = |l: TaskList| l.tasks.iter().map(|t| t.id).collect::<Vec<_>>();
        let now = d("2026-11-13");

        let open = list(&db, ListTasksParams::default(), now).unwrap();
        assert_eq!(open.count, 2);
        assert_eq!(ids(open), vec![past.id, today.id]);

        let overdue = list(&db, ListTasksParams { overdue: Some(true), ..Default::default() }, now).unwrap();
        assert_eq!(ids(overdue), vec![past.id]);

        let by_date = list(&db, ListTasksParams { date: Some("2026-11-13".into()), ..Default::default() }, now).unwrap();
        assert_eq!(ids(by_date), vec![today.id]);

        let done = list(&db, ListTasksParams { status: Some(Status::Completed), ..Default::default() }, now).unwrap();
        assert_eq!(ids(done), vec![inbox.id]);

        let q = list(&db, ListTasksParams { query: Some("REPORT".into()), ..Default::default() }, now).unwrap();
        assert_eq!(ids(q), vec![today.id]);

        db.delete_task(past.id).unwrap();
        let trash = list(&db, ListTasksParams { status: Some(Status::Deleted), ..Default::default() }, now).unwrap();
        assert_eq!(ids(trash), vec![past.id]);

        let e = list(&db, ListTasksParams { date: Some("tomorrow".into()), ..Default::default() }, now).unwrap_err();
        assert!(e.contains("YYYY-MM-DD"));
    }

    #[test]
    fn exposes_exactly_the_spec_tools() {
        let server = TaskServer::new(Db::open_in_memory().unwrap());
        let mut names: Vec<String> = server.tool_router.list_all().into_iter().map(|t| t.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "complete_task",
                "create_task",
                "delete_task",
                "get_task",
                "list_tasks",
                "reopen_task",
                "restore_task",
                "update_task"
            ]
        );
    }
}
