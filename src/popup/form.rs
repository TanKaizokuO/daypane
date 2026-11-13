//! Edit-form parsing: turns the raw text of the form fields into a minimal `TaskPatch`.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

use crate::capture;
use crate::db::{Priority, Task, TaskPatch};

/// Order of the priority drop-down rows.
pub const PRIORITIES: [Priority; 4] =
    [Priority::None, Priority::Low, Priority::Med, Priority::High];
pub const PRIORITY_NAMES: [&str; 4] = ["none", "low", "med", "high"];

pub fn priority_index(p: Priority) -> u32 {
    PRIORITIES.iter().position(|&x| x == p).unwrap_or(0) as u32
}

pub fn priority_at(index: u32) -> Priority {
    PRIORITIES.get(index as usize).copied().unwrap_or_default()
}

/// Raw field contents as typed by the user.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FormInput {
    pub title: String,
    pub date: String,
    pub time: String,
    pub priority: Priority,
    pub tags: String,
    pub notes: String,
}

impl FormInput {
    pub fn from_task(t: &Task) -> Self {
        Self {
            title: t.title.clone(),
            date: t
                .due_date
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            time: t
                .due_time
                .map(|t| t.format("%H:%M").to_string())
                .unwrap_or_default(),
            priority: t.priority,
            tags: t.tags.join(" "),
            notes: t.notes.clone(),
        }
    }
}

/// Tags separated by whitespace and/or commas; `#` prefix optional; lower-cased, de-duplicated.
pub fn parse_tags(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in s.split(|c: char| c.is_whitespace() || c == ',') {
        let t = t.trim_start_matches('#').to_lowercase();
        if !t.is_empty() && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// `YYYY-MM-DD`, otherwise whatever the capture parser understands ("tomorrow", "nov 20", …).
pub fn parse_date(s: &str, now: NaiveDateTime) -> Result<Option<NaiveDate>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Some(d));
    }
    capture::parse(s, now)
        .due_date
        .map(Some)
        .ok_or_else(|| format!("Unrecognized date “{s}” (use YYYY-MM-DD)"))
}

/// `HH:MM` (24 h), otherwise whatever the capture parser understands ("5pm").
pub fn parse_time(s: &str, now: NaiveDateTime) -> Result<Option<NaiveTime>, String> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M") {
        return Ok(Some(t));
    }
    capture::parse(s, now)
        .due_time
        .map(Some)
        .ok_or_else(|| format!("Unrecognized time “{s}” (use HH:MM)"))
}

/// Validate `input` against `task` and return only the changed fields.
/// `Ok(None)` = nothing changed. `Err` = a message for the form's error line.
pub fn build_patch(
    task: &Task,
    input: &FormInput,
    now: NaiveDateTime,
) -> Result<Option<TaskPatch>, String> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err("Title is required".into());
    }
    let date = parse_date(&input.date, now)?;
    let time = parse_time(&input.time, now)?;
    if time.is_some() && date.is_none() {
        return Err("A time needs a date".into());
    }
    let notes = input.notes.trim_end();
    let tags = parse_tags(&input.tags);

    let mut patch = TaskPatch::default();
    if title != task.title {
        patch.title = Some(title.to_string());
    }
    if date != task.due_date {
        patch.due_date = Some(date);
    }
    if time != task.due_time {
        patch.due_time = Some(time);
    }
    if notes != task.notes.trim_end() {
        patch.notes = Some(notes.to_string());
    }
    if input.priority != task.priority {
        patch.priority = Some(input.priority);
    }
    if tags != task.tags {
        patch.tags = Some(tags);
    }
    Ok((patch != TaskPatch::default()).then_some(patch))
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 13)
            .unwrap()
            .and_hms_opt(9, 0, 0)
            .unwrap()
    }

    fn task() -> Task {
        Task {
            id: 1,
            title: "Pay rent".into(),
            due_date: NaiveDate::from_ymd_opt(2026, 11, 14),
            due_time: NaiveTime::from_hms_opt(17, 0, 0),
            notes: "landlord".into(),
            priority: Priority::Low,
            tags: vec!["home".into()],
            links: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            deleted_at: None,
        }
    }

    #[test]
    fn unchanged_form_yields_no_patch() {
        let t = task();
        assert_eq!(build_patch(&t, &FormInput::from_task(&t), now()), Ok(None));
    }

    #[test]
    fn only_changed_fields_are_patched() {
        let t = task();
        let mut f = FormInput::from_task(&t);
        f.title = "  Pay rent!  ".into();
        f.priority = Priority::High;
        f.tags = "#Home, urgent  home".into();
        f.notes = "landlord\n\n".into();
        let p = build_patch(&t, &f, now()).unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Pay rent!"));
        assert_eq!(p.priority, Some(Priority::High));
        assert_eq!(p.tags, Some(vec!["home".into(), "urgent".into()]));
        assert_eq!(
            (p.notes, p.due_date, p.due_time),
            (None, None, None),
            "trailing whitespace is not a change"
        );
    }

    #[test]
    fn clearing_date_and_time() {
        let t = task();
        let mut f = FormInput::from_task(&t);
        f.date.clear();
        f.time.clear();
        let p = build_patch(&t, &f, now()).unwrap().unwrap();
        assert_eq!((p.due_date, p.due_time), (Some(None), Some(None)));
    }

    #[test]
    fn rejects_bad_input() {
        let t = task();
        let mut f = FormInput::from_task(&t);
        f.title = "   ".into();
        assert!(build_patch(&t, &f, now()).unwrap_err().contains("Title"));

        let mut f = FormInput::from_task(&t);
        f.date.clear();
        assert_eq!(
            build_patch(&t, &f, now()),
            Err("A time needs a date".into())
        );

        let mut f = FormInput::from_task(&t);
        f.time = "zzz".into();
        assert!(build_patch(&t, &f, now()).unwrap_err().contains("time"));
        let mut f = FormInput::from_task(&t);
        f.date = "zzz".into();
        assert!(build_patch(&t, &f, now()).unwrap_err().contains("date"));
    }

    #[test]
    fn strict_formats_parse() {
        assert_eq!(
            parse_date("2026-12-01", now()),
            Ok(NaiveDate::from_ymd_opt(2026, 12, 1))
        );
        assert_eq!(parse_date("  ", now()), Ok(None));
        assert_eq!(
            parse_time("9:05", now()),
            Ok(NaiveTime::from_hms_opt(9, 5, 0))
        );
        assert_eq!(
            parse_time("17:30", now()),
            Ok(NaiveTime::from_hms_opt(17, 30, 0))
        );
    }

    #[test]
    fn priority_rows_round_trip() {
        for (i, p) in PRIORITIES.iter().enumerate() {
            assert_eq!(priority_index(*p), i as u32);
            assert_eq!(priority_at(i as u32), *p);
        }
        assert_eq!(priority_at(99), Priority::None);
    }
}
