//! Waybar `return-type: json` output: `{text, tooltip, class, alt}`.
//!
//! `render` is pure (tested); `collect`, `write_atomic` and `signal` do the I/O.
//! Text and tooltip are Pango-escaped, so the Waybar module must NOT set `"escape": true`.

use std::fmt::Display;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};

use crate::db::{Db, Event, Task, TaskFilter};

const NEXT_EVENT_HORIZON_SECS: i64 = 24 * 3600;
const MAX_TEXT_TITLE_CHARS: usize = 28;
const MAX_TOOLTIP_TASKS: usize = 8;

/// `25` → `25m`, `125` → `2h 5m`, `120` → `2h`, `1500` → `1d 1h`.
pub fn format_minutes(minutes: i64) -> String {
    let m = minutes.max(0);
    let (d, h, mm) = (m / 1440, (m % 1440) / 60, m % 60);
    let parts: Vec<String> = [(d, "d"), (h, "h"), (mm, "m")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .take(2)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    if parts.is_empty() {
        "0m".to_owned()
    } else {
        parts.join(" ")
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

fn day_bounds<Tz: TimeZone>(tz: &Tz, day: NaiveDate) -> (i64, i64) {
    let at = |d: NaiveDate| {
        let local = |h| d.and_hms_opt(h, 0, 0).expect("valid hour");
        tz.from_local_datetime(&local(0))
            .earliest()
            // Midnight skipped by DST.
            .or_else(|| tz.from_local_datetime(&local(1)).earliest())
            .map_or(0, |t| t.timestamp())
    };
    (at(day), at(day.succ_opt().expect("date in range")))
}

fn clock<Tz: TimeZone>(tz: &Tz, ts: i64) -> String
where
    Tz::Offset: Display,
{
    tz.timestamp_opt(ts, 0)
        .single()
        .map_or_else(|| "??:??".to_owned(), |t| t.format("%H:%M").to_string())
}

fn event_title(e: &Event) -> &str {
    if e.summary.trim().is_empty() {
        "(no title)"
    } else {
        &e.summary
    }
}

/// Build the Waybar payload.
///
/// * `events`: any events that may matter; filtered here to today (tooltip) and the next 24 h (text).
/// * `overdue` / `due_today`: open tasks, already sorted.
/// * `open_tasks`: count of all open tasks (including undated).
pub fn render<Tz: TimeZone>(
    now: &DateTime<Tz>,
    events: &[Event],
    overdue: &[Task],
    due_today: &[Task],
    open_tasks: usize,
) -> Value
where
    Tz::Offset: Display,
{
    let tz = now.timezone();
    let now_ts = now.timestamp();
    let today = now.date_naive();

    // ── text ──
    let timed = || events.iter().filter(|e| !e.all_day);
    let in_progress = timed()
        .filter(|e| e.start_ts <= now_ts && now_ts < e.end_ts)
        .max_by_key(|e| e.start_ts);
    let upcoming = timed()
        .filter(|e| e.start_ts > now_ts && e.start_ts <= now_ts + NEXT_EVENT_HORIZON_SECS)
        .min_by_key(|e| e.start_ts);

    let (text, state) = if let Some(e) = in_progress {
        (
            format!("{} now", truncate(event_title(e), MAX_TEXT_TITLE_CHARS)),
            "now",
        )
    } else if let Some(e) = upcoming {
        let minutes = (e.start_ts - now_ts + 59) / 60;
        (
            format!(
                "{} in {}",
                truncate(event_title(e), MAX_TEXT_TITLE_CHARS),
                format_minutes(minutes)
            ),
            "upcoming",
        )
    } else if open_tasks == 0 {
        ("no tasks".to_owned(), "idle")
    } else if open_tasks == 1 {
        ("1 task".to_owned(), "tasks")
    } else {
        (format!("{open_tasks} tasks"), "tasks")
    };

    // ── tooltip ──
    let (day_start, day_end) = day_bounds(&tz, today);
    let mut todays: Vec<&Event> = events
        .iter()
        .filter(|e| e.start_ts < day_end && e.end_ts > day_start)
        .collect();
    todays.sort_by_key(|e| (!e.all_day, e.start_ts));

    let mut sections: Vec<String> = vec![format!("<b>{}</b>", now.format("%A, %b %-d"))];

    if !todays.is_empty() {
        let lines: Vec<String> = todays
            .iter()
            .map(|e| {
                let when = if e.all_day {
                    "All day".to_owned()
                } else {
                    format!("{}–{}", clock(&tz, e.start_ts), clock(&tz, e.end_ts))
                };
                format!("{}  {}", when, escape(event_title(e)))
            })
            .collect();
        sections.push(lines.join("\n"));
    }

    let task_lines = |tasks: &[Task], show_date: bool| -> Vec<String> {
        let mut lines: Vec<String> = tasks
            .iter()
            .take(MAX_TOOLTIP_TASKS)
            .map(|t| {
                let mut line = format!("• {}", escape(&t.title));
                if show_date {
                    if let Some(d) = t.due_date {
                        line.push_str(&format!("  ({})", d.format("%b %-d")));
                    }
                } else if let Some(time) = t.due_time {
                    line = format!("• {}  {}", time.format("%H:%M"), escape(&t.title));
                }
                line
            })
            .collect();
        if tasks.len() > MAX_TOOLTIP_TASKS {
            lines.push(format!("…and {} more", tasks.len() - MAX_TOOLTIP_TASKS));
        }
        lines
    };
    if !overdue.is_empty() {
        sections.push(format!(
            "<b>Overdue</b>\n{}",
            task_lines(overdue, true).join("\n")
        ));
    }
    if !due_today.is_empty() {
        sections.push(format!(
            "<b>Due today</b>\n{}",
            task_lines(due_today, false).join("\n")
        ));
    }
    if todays.is_empty() && overdue.is_empty() && due_today.is_empty() {
        sections.push("Nothing scheduled today".to_owned());
    }

    let mut class = vec![state];
    if !overdue.is_empty() {
        class.push("overdue");
    }

    json!({
        "text": escape(&text),
        "tooltip": sections.join("\n\n"),
        "class": class,
        "alt": state,
    })
}

/// Gather what `render` needs from the DB at `now`.
pub fn collect(db: &Db, now: &DateTime<Local>) -> Result<Value> {
    let today = now.date_naive();
    let (day_start, _) = crate::db::local_day_bounds(today);
    let events = db.events_between(day_start, now.timestamp() + NEXT_EVENT_HORIZON_SECS + 1)?;
    let view = db.today_view(today)?;
    let open = db.list_tasks(&TaskFilter::default())?.len();
    Ok(render(now, &events, &view.overdue, &view.due_today, open))
}

/// Write `contents` to `path` via a temp file in the same directory + rename.
pub fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .context("waybar file has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

/// Ask Waybar to re-run the module: `SIGRTMIN+n` to processes named `waybar`. Failure is ignored.
pub fn signal(n: u8) {
    let _ = std::process::Command::new("pkill")
        .arg(format!("-RTMIN+{n}"))
        .args(["-x", "waybar"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveTime, Utc};

    use super::*;
    use crate::db::Priority;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    fn at(h: u32, m: u32) -> DateTime<FixedOffset> {
        tz().with_ymd_and_hms(2026, 11, 13, h, m, 0).unwrap()
    }

    fn ev(summary: &str, start: DateTime<FixedOffset>, mins: i64) -> Event {
        Event {
            calendar_id: "c".into(),
            id: summary.into(),
            summary: summary.into(),
            description: String::new(),
            location: None,
            all_day: false,
            start_ts: start.timestamp(),
            end_ts: start.timestamp() + mins * 60,
            meet_link: None,
            html_link: None,
            use_default_reminders: true,
            reminders: vec![],
        }
    }

    fn all_day(summary: &str) -> Event {
        let mut e = ev(summary, at(0, 0), 24 * 60);
        e.all_day = true;
        e
    }

    fn task(title: &str, due: (u32, u32), time: Option<(u32, u32)>) -> Task {
        Task {
            id: 1,
            title: title.into(),
            due_date: NaiveDate::from_ymd_opt(2026, due.0, due.1),
            due_time: time.map(|(h, m)| NaiveTime::from_hms_opt(h, m, 0).unwrap()),
            notes: String::new(),
            priority: Priority::None,
            tags: vec![],
            links: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            deleted_at: None,
        }
    }

    fn text(v: &Value) -> &str {
        v["text"].as_str().unwrap()
    }

    fn classes(v: &Value) -> Vec<&str> {
        v["class"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect()
    }

    #[test]
    fn minutes_formatting() {
        assert_eq!(format_minutes(0), "0m");
        assert_eq!(format_minutes(1), "1m");
        assert_eq!(format_minutes(25), "25m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(125), "2h 5m");
        assert_eq!(format_minutes(1500), "1d 1h");
    }

    #[test]
    fn countdown_to_next_event() {
        let v = render(&at(9, 35), &[ev("Standup", at(10, 0), 30)], &[], &[], 3);
        assert_eq!(text(&v), "Standup in 25m");
        assert_eq!(v["alt"], "upcoming");
        assert_eq!(classes(&v), ["upcoming"]);
    }

    #[test]
    fn countdown_rounds_up_and_uses_hours() {
        let now = tz().with_ymd_and_hms(2026, 11, 13, 7, 54, 30).unwrap();
        let v = render(&now, &[ev("Review", at(10, 0), 30)], &[], &[], 0);
        assert_eq!(text(&v), "Review in 2h 6m");
        let now = tz().with_ymd_and_hms(2026, 11, 13, 9, 59, 20).unwrap();
        let v = render(&now, &[ev("X", at(10, 0), 5)], &[], &[], 0);
        assert_eq!(text(&v), "X in 1m");
    }

    #[test]
    fn in_progress_event_says_now() {
        let v = render(
            &at(10, 10),
            &[ev("Standup", at(10, 0), 30), ev("Later", at(11, 0), 30)],
            &[],
            &[],
            3,
        );
        assert_eq!(text(&v), "Standup now");
        assert_eq!(classes(&v), ["now"]);
        // Boundaries: starts exactly now = now; ends exactly now = no longer in progress.
        assert_eq!(
            text(&render(
                &at(10, 0),
                &[ev("Standup", at(10, 0), 30)],
                &[],
                &[],
                0
            )),
            "Standup now"
        );
        assert_eq!(
            text(&render(
                &at(10, 30),
                &[ev("Standup", at(10, 0), 30)],
                &[],
                &[],
                2
            )),
            "2 tasks"
        );
    }

    #[test]
    fn events_beyond_24h_and_all_day_do_not_drive_the_text() {
        let far = tz().with_ymd_and_hms(2026, 11, 14, 9, 31, 0).unwrap();
        let v = render(
            &at(9, 30),
            &[ev("Far", far, 30), all_day("Holiday")],
            &[],
            &[],
            1,
        );
        assert_eq!(text(&v), "1 task");
    }

    #[test]
    fn falls_back_to_task_count() {
        assert_eq!(text(&render(&at(9, 0), &[], &[], &[], 0)), "no tasks");
        assert_eq!(render(&at(9, 0), &[], &[], &[], 0)["alt"], "idle");
        assert_eq!(text(&render(&at(9, 0), &[], &[], &[], 1)), "1 task");
        assert_eq!(text(&render(&at(9, 0), &[], &[], &[], 7)), "7 tasks");
    }

    #[test]
    fn overdue_class_is_added() {
        let od = [task("Pay rent", (11, 10), None)];
        let v = render(&at(9, 0), &[], &od, &[], 1);
        assert_eq!(classes(&v), ["tasks", "overdue"]);
        let v = render(&at(9, 0), &[ev("S", at(9, 30), 30)], &od, &[], 1);
        assert_eq!(classes(&v), ["upcoming", "overdue"]);
        assert_eq!(classes(&render(&at(9, 0), &[], &[], &[], 1)), ["tasks"]);
    }

    #[test]
    fn tooltip_lists_events_then_tasks_and_escapes_pango() {
        let od = [task("Fix <b>bug</b> & ship", (11, 10), None)];
        let today = [
            task("Call Tom & Jerry", (11, 13), Some((14, 30))),
            task("Plain", (11, 13), None),
        ];
        let events = [
            ev("R&D <sync>", at(10, 0), 30),
            all_day("Holiday"),
            ev(
                "Tomorrow",
                tz().with_ymd_and_hms(2026, 11, 14, 10, 0, 0).unwrap(),
                30,
            ),
        ];
        let v = render(&at(9, 0), &events, &od, &today, 4);
        let tip = v["tooltip"].as_str().unwrap();
        assert_eq!(
            tip,
            "<b>Friday, Nov 13</b>\n\n\
             All day  Holiday\n\
             10:00–10:30  R&amp;D &lt;sync&gt;\n\n\
             <b>Overdue</b>\n\
             • Fix &lt;b&gt;bug&lt;/b&gt; &amp; ship  (Nov 10)\n\n\
             <b>Due today</b>\n\
             • 14:30  Call Tom &amp; Jerry\n\
             • Plain"
        );
        assert_eq!(
            text(&render(
                &at(9, 0),
                &[ev("R&D <x>", at(10, 0), 30)],
                &[],
                &[],
                0
            )),
            "R&amp;D &lt;x&gt; in 1h"
        );
    }

    #[test]
    fn empty_day_tooltip_and_task_cap() {
        let v = render(&at(9, 0), &[], &[], &[], 0);
        assert_eq!(
            v["tooltip"],
            "<b>Friday, Nov 13</b>\n\nNothing scheduled today"
        );
        let many: Vec<Task> = (0..10)
            .map(|i| task(&format!("t{i}"), (11, 1), None))
            .collect();
        let tip = render(&at(9, 0), &[], &many, &[], 10)["tooltip"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(tip.contains("• t7"));
        assert!(!tip.contains("• t8"));
        assert!(tip.ends_with("…and 2 more"));
    }

    #[test]
    fn long_titles_are_truncated_in_text_only() {
        let long = "A".repeat(60);
        let v = render(&at(9, 0), &[ev(&long, at(9, 30), 30)], &[], &[], 0);
        assert_eq!(
            text(&v).chars().count(),
            MAX_TEXT_TITLE_CHARS + " in 30m".len()
        );
        assert!(text(&v).starts_with(&format!("{}…", "A".repeat(27))));
        assert!(v["tooltip"].as_str().unwrap().contains(&long));
    }

    #[test]
    fn write_atomic_replaces_file_without_leaving_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sub/waybar.json");
        write_atomic(&p, "one").unwrap();
        write_atomic(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(std::fs::read_dir(p.parent().unwrap()).unwrap().count(), 1);
    }
}
