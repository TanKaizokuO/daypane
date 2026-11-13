//! Pure text formatting for rows, titles, and the status line.

use std::fmt::Display;

use chrono::{DateTime, Datelike, NaiveDate, TimeZone};

use crate::db::{Event, Priority, Task};

pub const HINT: &str = "Tab pane · h/j/k/l move · x done · e edit · d trash · u undo · t today · N new · / search · Esc close";

/// "09:00–10:00", "All day"; with `with_date`, prefixed by the start day ("Fri 14 Nov 09:00–10:00").
pub fn event_when<Tz: TimeZone>(e: &Event, tz: &Tz, with_date: bool) -> String
where
    Tz::Offset: Display,
{
    let local = |ts: i64| DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(tz));
    let Some(start) = local(e.start_ts) else {
        return String::new();
    };
    let span = if e.all_day {
        "All day".to_string()
    } else {
        let end = local(e.end_ts).unwrap_or_else(|| start.clone());
        format!("{}–{}", start.format("%H:%M"), end.format("%H:%M"))
    };
    if with_date {
        format!("{} {span}", start.format("%a %-d %b"))
    } else {
        span
    }
}

/// Date and/or time label for a task row; `None` when there is nothing to show.
pub fn task_when(t: &Task, show_date: bool) -> Option<String> {
    let date = t.due_date.filter(|_| show_date).map(|d| d.format("%a %-d %b").to_string());
    let time = t.due_time.map(|t| t.format("%H:%M").to_string());
    match (date, time) {
        (Some(d), Some(t)) => Some(format!("{d} {t}")),
        (Some(d), None) => Some(d),
        (None, t) => t,
    }
}

pub fn priority_label(p: Priority) -> Option<&'static str> {
    match p {
        Priority::None => None,
        Priority::Low => Some("low"),
        Priority::Med => Some("med"),
        Priority::High => Some("high"),
    }
}

pub fn priority_class(p: Priority) -> Option<&'static str> {
    match p {
        Priority::None => None,
        Priority::Low => Some("priority-low"),
        Priority::Med => Some("priority-med"),
        Priority::High => Some("priority-high"),
    }
}

/// First non-empty line of the notes, for the one-line preview under a task title.
pub fn notes_preview(notes: &str) -> Option<String> {
    notes.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

/// Heading for the events list: "Today · Fri 13 Nov", "Tomorrow · …", otherwise just the date.
pub fn day_title(day: NaiveDate, today: NaiveDate) -> String {
    let date = if day.year() == today.year() {
        day.format("%a %-d %b").to_string()
    } else {
        day.format("%a %-d %b %Y").to_string()
    };
    match (day - today).num_days() {
        0 => format!("Today · {date}"),
        1 => format!("Tomorrow · {date}"),
        -1 => format!("Yesterday · {date}"),
        _ => date,
    }
}

/// Status-line subject: a task title in quotes, shortened.
pub fn quoted(title: &str) -> String {
    const MAX: usize = 40;
    if title.chars().count() > MAX {
        let cut: String = title.chars().take(MAX).collect();
        format!("“{}…”", cut.trim_end())
    } else {
        format!("“{title}”")
    }
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveTime, Utc};

    use super::*;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn ev(start: i64, end: i64, all_day: bool) -> Event {
        Event {
            calendar_id: "c".into(),
            id: "e".into(),
            summary: "s".into(),
            description: String::new(),
            location: None,
            all_day,
            start_ts: start,
            end_ts: end,
            meet_link: None,
            html_link: None,
            use_default_reminders: true,
            reminders: vec![],
        }
    }

    #[test]
    fn event_times_render_in_the_given_zone() {
        let tz = FixedOffset::east_opt(2 * 3600).unwrap();
        let start = d("2026-11-13").and_hms_opt(9, 0, 0).unwrap().and_local_timezone(tz).unwrap().timestamp();
        let e = ev(start, start + 3600, false);
        assert_eq!(event_when(&e, &tz, false), "09:00–10:00");
        assert_eq!(event_when(&e, &tz, true), "Fri 13 Nov 09:00–10:00");
        assert_eq!(event_when(&ev(start, start + 86400, true), &tz, false), "All day");
    }

    fn task(date: Option<&str>, time: Option<(u32, u32)>) -> Task {
        Task {
            id: 1,
            title: "t".into(),
            due_date: date.map(d),
            due_time: time.and_then(|(h, m)| NaiveTime::from_hms_opt(h, m, 0)),
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

    #[test]
    fn task_when_combinations() {
        assert_eq!(task_when(&task(Some("2026-11-10"), Some((9, 5))), true).as_deref(), Some("Tue 10 Nov 09:05"));
        assert_eq!(task_when(&task(Some("2026-11-10"), Some((9, 5))), false).as_deref(), Some("09:05"));
        assert_eq!(task_when(&task(Some("2026-11-10"), None), true).as_deref(), Some("Tue 10 Nov"));
        assert_eq!(task_when(&task(Some("2026-11-10"), None), false), None);
        assert_eq!(task_when(&task(None, None), true), None);
    }

    #[test]
    fn notes_preview_skips_blank_lines() {
        assert_eq!(notes_preview("\n  \n  first line \nsecond").as_deref(), Some("first line"));
        assert_eq!(notes_preview("  \n"), None);
    }

    #[test]
    fn day_titles() {
        let today = d("2026-11-13");
        assert_eq!(day_title(today, today), "Today · Fri 13 Nov");
        assert_eq!(day_title(d("2026-11-14"), today), "Tomorrow · Sat 14 Nov");
        assert_eq!(day_title(d("2026-11-12"), today), "Yesterday · Thu 12 Nov");
        assert_eq!(day_title(d("2026-11-20"), today), "Fri 20 Nov");
        assert_eq!(day_title(d("2027-01-04"), today), "Mon 4 Jan 2027");
    }

    #[test]
    fn quoted_truncates_long_titles() {
        assert_eq!(quoted("short"), "“short”");
        let long = "x".repeat(60);
        assert_eq!(quoted(&long), format!("“{}…”", "x".repeat(40)));
    }

    #[test]
    fn priority_helpers() {
        assert_eq!(priority_label(Priority::None), None);
        assert_eq!(priority_class(Priority::High), Some("priority-high"));
    }
}
