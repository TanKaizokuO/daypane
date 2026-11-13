//! Command-line subcommands: `add`, `today`, `next`, `done`, `reopen`, `rm`, `restore`, `ls`, `calendars`.
//!
//! Human output is one line per task: `#<id> [x] title  due  !prio #tags`. `--json` prints the
//! serde representation of the `db` types. Formatting lives in pure functions (unit-tested below);
//! the `pub fn` command entry points only fetch from the DB and print.

use std::io::Write;

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime};
use clap::ValueEnum;
use serde::Serialize;

use crate::app::APP_NAME;
use crate::db::{Calendar, Db, Event, Priority, Status, Task, TaskFilter, TodayView};

/// How far ahead `next` looks for calendar events.
const NEXT_EVENT_HORIZON_DAYS: i64 = 60;

// ─── Args ───────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct CalendarsArgs {
    /// Show these calendars (id or exact name).
    #[arg(long, value_name = "ID", num_args = 1..)]
    select: Vec<String>,
    /// Hide these calendars (id or exact name).
    #[arg(long, value_name = "ID", num_args = 1..)]
    deselect: Vec<String>,
}

#[derive(clap::Args)]
pub struct JsonFlag {
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum StatusArg {
    Open,
    Completed,
    Deleted,
    All,
}

impl From<StatusArg> for Status {
    fn from(s: StatusArg) -> Self {
        match s {
            StatusArg::Open => Status::Open,
            StatusArg::Completed => Status::Completed,
            StatusArg::Deleted => Status::Deleted,
            StatusArg::All => Status::All,
        }
    }
}

#[derive(clap::Args)]
pub struct LsArgs {
    /// Only tasks due on this date: YYYY-MM-DD or capture words ("tomorrow", "fri", "nov 20").
    #[arg(long, value_name = "DATE", conflicts_with = "inbox")]
    date: Option<String>,
    /// Only tasks with this tag.
    #[arg(long, value_name = "TAG")]
    tag: Option<String>,
    /// Only open tasks due before today.
    #[arg(long, conflicts_with = "status")]
    overdue: bool,
    /// Which tasks to list (default: open).
    #[arg(long, value_enum)]
    status: Option<StatusArg>,
    /// Case-insensitive substring over title, notes and tags.
    #[arg(long, short = 'q', value_name = "TEXT")]
    query: Option<String>,
    /// Only undated tasks.
    #[arg(long)]
    inbox: bool,
    /// Print JSON instead of text.
    #[arg(long)]
    json: bool,
}

// ─── Commands ───────────────────────────────────────────────────────────────

pub fn calendars(args: CalendarsArgs) -> Result<()> {
    let db = Db::open_default()?;
    let mut cals = db.calendars()?;
    for (keys, selected) in [(&args.select, true), (&args.deselect, false)] {
        for key in keys {
            let id = resolve_calendar(&cals, key)?.id.clone();
            db.set_calendar_selected(&id, selected)?;
            cals = db.calendars()?;
        }
    }
    if cals.is_empty() {
        return emit(&format!(
            "No calendars cached yet. Run `{APP_NAME} login`, then let `{APP_NAME} daemon` sync.\n"
        ));
    }
    let mut out = String::new();
    for c in &cals {
        out.push_str(&calendar_line(c));
        out.push('\n');
    }
    emit(&out)
}

pub fn add(text: &str) -> Result<()> {
    let capture = crate::capture::parse(text, Local::now().naive_local());
    if capture.title.trim().is_empty() {
        bail!(
            "nothing to add: the title is empty after parsing ({})",
            capture.preview()
        );
    }
    let db = Db::open_default()?;
    let task = db.create_task(capture.into_new_task())?;
    print_task(&task)
}

pub fn today(flag: JsonFlag) -> Result<()> {
    let now = Local::now().naive_local();
    let day = now.date();
    let db = Db::open_default()?;
    let events = db.events_on(day)?;
    let view = db.today_view(day)?;
    if flag.json {
        return emit_json(&TodayOut {
            date: day,
            events,
            view,
        });
    }
    let lines: Vec<String> = events
        .iter()
        .map(|e| {
            event_line(
                &e.summary,
                e.all_day,
                local_naive(e.start_ts),
                local_naive(e.end_ts),
                day,
            )
        })
        .collect();
    emit(&render_today(day, &lines, &view))
}

pub fn next(flag: JsonFlag) -> Result<()> {
    let now = Local::now().naive_local();
    let now_ts = Local::now().timestamp();
    let db = Db::open_default()?;
    let events = db.events_between(now_ts, now_ts + NEXT_EVENT_HORIZON_DAYS * 86_400)?;
    let tasks = db.timed_tasks_from(now.date())?;
    let next_event = earliest(
        now,
        events
            .iter()
            .filter(|e| !e.all_day)
            .map(|e| (local_naive(e.start_ts), e)),
    );
    let next_task = earliest(
        now,
        tasks
            .iter()
            .filter_map(|t| Some((t.due_date?.and_time(t.due_time?), t))),
    );
    let Some(choice) = pick_next(next_event, next_task) else {
        return if flag.json {
            emit("null\n")
        } else {
            emit("Nothing upcoming.\n")
        };
    };
    let (at, in_seconds) = match &choice {
        Next::Event(at, _) | Next::Task(at, _) => (*at, (*at - now).num_seconds()),
    };
    if flag.json {
        let (kind, value) = match &choice {
            Next::Event(_, e) => ("event", serde_json::to_value(e)?),
            Next::Task(_, t) => ("task", serde_json::to_value(t)?),
        };
        return emit_json(&serde_json::json!({
            "kind": kind,
            "starts_at": at.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "in_seconds": in_seconds,
            kind: value,
        }));
    }
    let line = match &choice {
        Next::Event(_, e) => format!(
            "{}  {}  (event)",
            e.summary,
            fmt_due(at.date(), Some(at.time()), now.date())
        ),
        Next::Task(_, t) => task_line(t, now.date()),
    };
    emit(&format!("{}  {line}\n", countdown(in_seconds)))
}

pub fn done(id: i64) -> Result<()> {
    print_task(&Db::open_default()?.complete_task(id)?)
}

pub fn reopen(id: i64) -> Result<()> {
    print_task(&Db::open_default()?.reopen_task(id)?)
}

pub fn rm(id: i64) -> Result<()> {
    print_task(&Db::open_default()?.delete_task(id)?)
}

pub fn restore(id: i64) -> Result<()> {
    print_task(&Db::open_default()?.restore_task(id)?)
}

pub fn ls(args: LsArgs) -> Result<()> {
    let now = Local::now().naive_local();
    let filter = build_filter(&args, now)?;
    let tasks = Db::open_default()?.list_tasks(&filter)?;
    if args.json {
        return emit_json(&tasks);
    }
    if tasks.is_empty() {
        return emit("No tasks.\n");
    }
    let today = now.date();
    emit(
        &tasks
            .iter()
            .map(|t| task_line(t, today) + "\n")
            .collect::<String>(),
    )
}

// ─── Output plumbing ────────────────────────────────────────────────────────

/// Write to stdout; a closed pipe (`… | head`) is not an error.
fn emit(s: &str) -> Result<()> {
    match std::io::stdout().lock().write_all(s.as_bytes()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        r => Ok(r?),
    }
}

fn emit_json<T: Serialize>(v: &T) -> Result<()> {
    emit(&(serde_json::to_string_pretty(v)? + "\n"))
}

fn print_task(t: &Task) -> Result<()> {
    emit(&(task_line(t, Local::now().date_naive()) + "\n"))
}

#[derive(Serialize)]
struct TodayOut {
    date: NaiveDate,
    events: Vec<Event>,
    #[serde(flatten)]
    view: TodayView,
}

// ─── Pure helpers ───────────────────────────────────────────────────────────

fn local_naive(ts: i64) -> NaiveDateTime {
    DateTime::from_timestamp(ts, 0)
        .unwrap_or_default()
        .with_timezone(&Local)
        .naive_local()
}

/// `today` / `tomorrow` / `yesterday` / `Fri 14 Nov` (+ year when it differs), plus ` HH:MM`.
fn fmt_due(date: NaiveDate, time: Option<NaiveTime>, today: NaiveDate) -> String {
    let day = if date == today {
        "today".to_string()
    } else if today.succ_opt() == Some(date) {
        "tomorrow".to_string()
    } else if today.pred_opt() == Some(date) {
        "yesterday".to_string()
    } else if date.year() == today.year() {
        date.format("%a %-d %b").to_string()
    } else {
        date.format("%a %-d %b %Y").to_string()
    };
    match time {
        Some(t) => format!("{day} {}", t.format("%H:%M")),
        None => day,
    }
}

/// `#<id> [ ] title  due  !prio #tags` (`[x]` completed, `[-]` in trash).
fn task_line(t: &Task, today: NaiveDate) -> String {
    let mark = if t.deleted_at.is_some() {
        "[-]"
    } else if t.completed_at.is_some() {
        "[x]"
    } else {
        "[ ]"
    };
    let mut parts = vec![format!("#{} {mark} {}", t.id, t.title)];
    if let Some(d) = t.due_date {
        parts.push(fmt_due(d, t.due_time, today));
    }
    match t.priority {
        Priority::None => {}
        Priority::Low => parts.push("!low".into()),
        Priority::Med => parts.push("!med".into()),
        Priority::High => parts.push("!high".into()),
    }
    if !t.tags.is_empty() {
        parts.push(
            t.tags
                .iter()
                .map(|g| format!("#{g}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    parts.join("  ")
}

/// `all day  Summary` or `09:00–10:00  Summary`; `…` marks a span continuing beyond `day`.
fn event_line(
    summary: &str,
    all_day: bool,
    start: NaiveDateTime,
    end: NaiveDateTime,
    day: NaiveDate,
) -> String {
    if all_day {
        return format!("all day  {summary}");
    }
    let from = if start.date() < day {
        "…".to_string()
    } else {
        start.format("%H:%M").to_string()
    };
    let to = if end.date() > day && end.time() != NaiveTime::MIN {
        "…".to_string()
    } else {
        end.format("%H:%M").to_string()
    };
    format!("{from}–{to}  {summary}")
}

fn render_today(day: NaiveDate, event_lines: &[String], view: &TodayView) -> String {
    let mut out = format!("{}\n", day.format("%A %-d %B"));
    let mut empty = true;
    if !event_lines.is_empty() {
        empty = false;
        out.push_str("Events\n");
        for l in event_lines {
            out.push_str(&format!("  {l}\n"));
        }
    }
    for (heading, tasks) in [
        ("Overdue", &view.overdue),
        ("Due today", &view.due_today),
        ("Inbox", &view.inbox),
    ] {
        if tasks.is_empty() {
            continue;
        }
        empty = false;
        out.push_str(heading);
        out.push('\n');
        for t in tasks {
            out.push_str(&format!("  {}\n", task_line(t, day)));
        }
    }
    if empty {
        out.push_str("Nothing planned.\n");
    }
    out
}

/// `in 45m`, `in 2h 15m`, `in 3d 4h`; zero units are dropped, seconds are truncated.
fn countdown(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let (m, h, d) = (secs / 60, secs / 3600, secs / 86_400);
    match (d, h, m) {
        (0, 0, 0) => "in <1m".into(),
        (0, 0, m) => format!("in {m}m"),
        (0, h, _) => match m % 60 {
            0 => format!("in {h}h"),
            r => format!("in {h}h {r}m"),
        },
        (d, _, _) => match h % 24 {
            0 => format!("in {d}d"),
            r => format!("in {d}d {r}h"),
        },
    }
}

/// The soonest item starting at or after `now` (first wins on ties).
fn earliest<T>(
    now: NaiveDateTime,
    items: impl Iterator<Item = (NaiveDateTime, T)>,
) -> Option<(NaiveDateTime, T)> {
    items.filter(|(at, _)| *at >= now).min_by_key(|(at, _)| *at)
}

enum Next<'a> {
    Event(NaiveDateTime, &'a Event),
    Task(NaiveDateTime, &'a Task),
}

/// Whichever starts sooner; the event wins a tie.
fn pick_next<'a>(
    event: Option<(NaiveDateTime, &'a Event)>,
    task: Option<(NaiveDateTime, &'a Task)>,
) -> Option<Next<'a>> {
    match (event, task) {
        (Some((e, ev)), Some((t, _))) if e <= t => Some(Next::Event(e, ev)),
        (_, Some((t, tk))) => Some(Next::Task(t, tk)),
        (Some((e, ev)), None) => Some(Next::Event(e, ev)),
        (None, None) => None,
    }
}

fn parse_date_arg(s: &str, now: NaiveDateTime) -> Result<NaiveDate> {
    let cap = crate::capture::parse(s, now);
    match cap.due_date {
        Some(d) if cap.title.is_empty() => Ok(d),
        _ => Err(anyhow!("cannot understand date {s:?}; use YYYY-MM-DD or words like \"tomorrow\", \"fri\", \"nov 20\"")),
    }
}

fn build_filter(args: &LsArgs, now: NaiveDateTime) -> Result<TaskFilter> {
    Ok(TaskFilter {
        status: args.status.map(Status::from).unwrap_or_default(),
        date: args
            .date
            .as_deref()
            .map(|s| parse_date_arg(s, now))
            .transpose()?,
        undated: args.inbox,
        overdue: args.overdue,
        today: Some(now.date()),
        tag: args.tag.clone(),
        query: args.query.clone(),
    })
}

fn calendar_line(c: &Calendar) -> String {
    format!(
        "[{}] {}  ({})",
        if c.selected { 'x' } else { ' ' },
        c.summary,
        c.id
    )
}

/// Find a calendar by exact id, else by case-insensitive name (must be unambiguous).
fn resolve_calendar<'a>(cals: &'a [Calendar], key: &str) -> Result<&'a Calendar> {
    if let Some(c) = cals.iter().find(|c| c.id == key) {
        return Ok(c);
    }
    let mut named = cals.iter().filter(|c| c.summary.eq_ignore_ascii_case(key));
    match (named.next(), named.next()) {
        (Some(c), None) => Ok(c),
        (Some(_), Some(_)) => {
            bail!("calendar name {key:?} is ambiguous; use its id (see `{APP_NAME} calendars`)")
        }
        _ => bail!("no calendar matches {key:?} (see `{APP_NAME} calendars`)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn at(d: NaiveDate, h: u32, m: u32) -> NaiveDateTime {
        d.and_hms_opt(h, m, 0).unwrap()
    }

    /// Friday 2026-11-13, 10:00.
    fn now() -> NaiveDateTime {
        at(day(2026, 11, 13), 10, 0)
    }

    fn task(id: i64, title: &str) -> Task {
        let stamp = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
        Task {
            id,
            title: title.into(),
            due_date: None,
            due_time: None,
            notes: String::new(),
            priority: Priority::None,
            tags: vec![],
            links: vec![],
            created_at: stamp,
            updated_at: stamp,
            completed_at: None,
            deleted_at: None,
        }
    }

    fn cal(id: &str, summary: &str, selected: bool) -> Calendar {
        Calendar {
            id: id.into(),
            summary: summary.into(),
            color: None,
            selected,
            sync_token: None,
            default_reminders: vec![],
        }
    }

    fn ls_args() -> LsArgs {
        LsArgs {
            date: None,
            tag: None,
            overdue: false,
            status: None,
            query: None,
            inbox: false,
            json: false,
        }
    }

    #[test]
    fn countdown_formatting() {
        assert_eq!(countdown(-5), "now");
        assert_eq!(countdown(0), "now");
        assert_eq!(countdown(30), "in <1m");
        assert_eq!(countdown(60), "in 1m");
        assert_eq!(countdown(59 * 60 + 59), "in 59m");
        assert_eq!(countdown(3600), "in 1h");
        assert_eq!(countdown(2 * 3600 + 15 * 60 + 59), "in 2h 15m");
        assert_eq!(countdown(23 * 3600 + 59 * 60), "in 23h 59m");
        assert_eq!(countdown(86_400), "in 1d");
        assert_eq!(countdown(3 * 86_400 + 4 * 3600 + 30 * 60), "in 3d 4h");
    }

    #[test]
    fn due_formatting() {
        let today = day(2026, 11, 13);
        assert_eq!(fmt_due(today, None, today), "today");
        assert_eq!(fmt_due(day(2026, 11, 14), None, today), "tomorrow");
        assert_eq!(fmt_due(day(2026, 11, 12), None, today), "yesterday");
        assert_eq!(fmt_due(day(2026, 11, 20), None, today), "Fri 20 Nov");
        assert_eq!(fmt_due(day(2027, 1, 5), None, today), "Tue 5 Jan 2027");
        assert_eq!(
            fmt_due(today, NaiveTime::from_hms_opt(17, 5, 0), today),
            "today 17:05"
        );
        // Year boundaries use calendar neighbours.
        assert_eq!(
            fmt_due(day(2027, 1, 1), None, day(2026, 12, 31)),
            "tomorrow"
        );
    }

    #[test]
    fn task_lines() {
        let today = day(2026, 11, 13);
        let mut t = task(7, "pay rent");
        assert_eq!(task_line(&t, today), "#7 [ ] pay rent");
        t.due_date = Some(day(2026, 11, 14));
        t.due_time = NaiveTime::from_hms_opt(17, 0, 0);
        t.priority = Priority::High;
        t.tags = vec!["home".into(), "money".into()];
        assert_eq!(
            task_line(&t, today),
            "#7 [ ] pay rent  tomorrow 17:00  !high  #home #money"
        );
        t.completed_at = Some(Utc::now());
        assert!(task_line(&t, today).starts_with("#7 [x] "));
        t.deleted_at = Some(Utc::now());
        assert!(task_line(&t, today).starts_with("#7 [-] "));
    }

    #[test]
    fn event_lines() {
        let d = day(2026, 11, 13);
        assert_eq!(
            event_line("Holiday", true, at(d, 0, 0), at(day(2026, 11, 14), 0, 0), d),
            "all day  Holiday"
        );
        assert_eq!(
            event_line("Standup", false, at(d, 9, 0), at(d, 9, 15), d),
            "09:00–09:15  Standup"
        );
        // Spans crossing midnight.
        assert_eq!(
            event_line(
                "Flight",
                false,
                at(d, 22, 0),
                at(day(2026, 11, 14), 6, 30),
                d
            ),
            "22:00–…  Flight"
        );
        assert_eq!(
            event_line(
                "Flight",
                false,
                at(day(2026, 11, 12), 22, 0),
                at(d, 6, 30),
                d
            ),
            "…–06:30  Flight"
        );
        // Ending exactly at midnight is shown as an end time.
        assert_eq!(
            event_line("Late", false, at(d, 22, 0), at(day(2026, 11, 14), 0, 0), d),
            "22:00–00:00  Late"
        );
    }

    #[test]
    fn today_rendering() {
        let d = day(2026, 11, 13);
        assert_eq!(
            render_today(d, &[], &TodayView::default()),
            "Friday 13 November\nNothing planned.\n"
        );

        let mut overdue = task(1, "old");
        overdue.due_date = Some(day(2026, 11, 10));
        let view = TodayView {
            overdue: vec![overdue],
            due_today: vec![],
            inbox: vec![task(2, "someday")],
        };
        let out = render_today(d, &["09:00–10:00  Standup".into()], &view);
        assert_eq!(
            out,
            "Friday 13 November\nEvents\n  09:00–10:00  Standup\nOverdue\n  #1 [ ] old  Tue 10 Nov\nInbox\n  #2 [ ] someday\n"
        );
    }

    #[test]
    fn earliest_ignores_the_past_and_prefers_first_on_tie() {
        let n = now();
        let items = vec![
            (at(day(2026, 11, 13), 9, 0), "past"),
            (at(day(2026, 11, 13), 12, 0), "b"),
            (at(day(2026, 11, 13), 12, 0), "c"),
            (at(day(2026, 11, 14), 8, 0), "d"),
        ];
        assert_eq!(earliest(n, items.into_iter()).map(|x| x.1), Some("b"));
        // Starting exactly now counts as upcoming.
        assert_eq!(
            earliest(n, vec![(n, "now")].into_iter()).map(|x| x.1),
            Some("now")
        );
        assert!(earliest(n, vec![(at(day(2026, 11, 13), 9, 59), "x")].into_iter()).is_none());
    }

    #[test]
    fn pick_next_prefers_the_sooner_and_events_on_ties() {
        let ev = Event {
            calendar_id: "c".into(),
            id: "e".into(),
            summary: "Standup".into(),
            description: String::new(),
            location: None,
            all_day: false,
            start_ts: 0,
            end_ts: 1,
            meet_link: None,
            html_link: None,
            use_default_reminders: true,
            reminders: vec![],
        };
        let tk = task(1, "t");
        let (t10, t11) = (at(day(2026, 11, 13), 10, 0), at(day(2026, 11, 13), 11, 0));
        assert!(matches!(
            pick_next(Some((t10, &ev)), Some((t11, &tk))),
            Some(Next::Event(..))
        ));
        assert!(matches!(
            pick_next(Some((t11, &ev)), Some((t10, &tk))),
            Some(Next::Task(..))
        ));
        assert!(matches!(
            pick_next(Some((t10, &ev)), Some((t10, &tk))),
            Some(Next::Event(..))
        ));
        assert!(matches!(
            pick_next(None, Some((t10, &tk))),
            Some(Next::Task(..))
        ));
        assert!(matches!(
            pick_next(Some((t10, &ev)), None),
            Some(Next::Event(..))
        ));
        assert!(pick_next(None, None).is_none());
    }

    #[test]
    fn date_argument() {
        let n = now();
        assert_eq!(parse_date_arg("2026-11-20", n).unwrap(), day(2026, 11, 20));
        assert_eq!(parse_date_arg("tomorrow", n).unwrap(), day(2026, 11, 14));
        assert_eq!(parse_date_arg("Mon", n).unwrap(), day(2026, 11, 16));
        assert_eq!(parse_date_arg("nov 20", n).unwrap(), day(2026, 11, 20));
        assert_eq!(parse_date_arg("in 2 weeks", n).unwrap(), day(2026, 11, 27));
        assert!(parse_date_arg("soon", n).is_err());
        assert!(parse_date_arg("buy milk tomorrow", n).is_err());
        assert!(parse_date_arg("", n).is_err());
    }

    #[test]
    fn filter_from_args() {
        let n = now();
        let f = build_filter(&ls_args(), n).unwrap();
        assert_eq!(
            f,
            TaskFilter {
                today: Some(n.date()),
                ..Default::default()
            }
        );
        assert_eq!(f.status, Status::Open);

        let f = build_filter(
            &LsArgs {
                date: Some("tomorrow".into()),
                tag: Some("home".into()),
                status: Some(StatusArg::Deleted),
                query: Some("rent".into()),
                ..ls_args()
            },
            n,
        )
        .unwrap();
        assert_eq!(f.date, Some(day(2026, 11, 14)));
        assert_eq!(f.tag.as_deref(), Some("home"));
        assert_eq!(f.status, Status::Deleted);
        assert_eq!(f.query.as_deref(), Some("rent"));

        let f = build_filter(
            &LsArgs {
                overdue: true,
                inbox: true,
                ..ls_args()
            },
            n,
        )
        .unwrap();
        assert!(f.overdue && f.undated);
        assert!(build_filter(
            &LsArgs {
                date: Some("nope".into()),
                ..ls_args()
            },
            n
        )
        .is_err());
    }

    #[test]
    fn calendar_resolution_and_lines() {
        let cals = vec![
            cal("a@x.com", "Work", true),
            cal("b@x.com", "Home", false),
            cal("c@x.com", "home", true),
        ];
        assert_eq!(resolve_calendar(&cals, "a@x.com").unwrap().summary, "Work");
        assert_eq!(resolve_calendar(&cals, "WORK").unwrap().id, "a@x.com");
        assert!(resolve_calendar(&cals, "home")
            .unwrap_err()
            .to_string()
            .contains("ambiguous"));
        assert!(resolve_calendar(&cals, "nope").is_err());
        assert_eq!(calendar_line(&cals[0]), "[x] Work  (a@x.com)");
        assert_eq!(calendar_line(&cals[1]), "[ ] Home  (b@x.com)");
    }
}
