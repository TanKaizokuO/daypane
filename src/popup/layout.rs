//! Pure calendar math: month-grid layout, keyboard day navigation, day markers,
//! and task-list selection movement. No GTK types, so everything here is unit-tested.

use std::collections::HashSet;

use chrono::{Datelike, Days, Months, NaiveDate, TimeZone};

use crate::db::{Event, Task};

/// Column headers; weeks start on Monday.
pub const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
/// The grid always shows six weeks so the popup does not change height between months.
pub const GRID_DAYS: usize = 42;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Left,
    Right,
    Up,
    Down,
}

pub fn first_of_month(d: NaiveDate) -> NaiveDate {
    d.with_day(1).expect("day 1 exists in every month")
}

/// The 42 days shown for the month containing `month`, starting on the Monday on or before the 1st.
pub fn grid_days(month: NaiveDate) -> Vec<NaiveDate> {
    let first = first_of_month(month);
    let start = first - Days::new(u64::from(first.weekday().num_days_from_monday()));
    (0..GRID_DAYS as u64).map(|i| start + Days::new(i)).collect()
}

/// Keyboard day movement: left/right = ±1 day, up/down = ±1 week. Saturates at the calendar's bounds.
pub fn move_day(d: NaiveDate, step: Step) -> NaiveDate {
    let moved = match step {
        Step::Left => d.checked_sub_days(Days::new(1)),
        Step::Right => d.checked_add_days(Days::new(1)),
        Step::Up => d.checked_sub_days(Days::new(7)),
        Step::Down => d.checked_add_days(Days::new(7)),
    };
    moved.unwrap_or(d)
}

/// Shift by whole months, clamping the day (Jan 31 + 1 month = Feb 28/29).
pub fn shift_months(d: NaiveDate, n: i32) -> NaiveDate {
    let m = Months::new(n.unsigned_abs());
    let shifted = if n >= 0 { d.checked_add_months(m) } else { d.checked_sub_months(m) };
    shifted.unwrap_or(d)
}

pub fn month_label(d: NaiveDate) -> String {
    d.format("%B %Y").to_string()
}

/// Local first and last calendar day an event touches (`end_ts` is exclusive).
pub fn event_span_days<Tz: TimeZone>(e: &Event, tz: &Tz) -> Option<(NaiveDate, NaiveDate)> {
    let local = |ts: i64| chrono::DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(tz).date_naive());
    let first = local(e.start_ts)?;
    let last = local((e.end_ts - 1).max(e.start_ts))?;
    Some((first, last))
}

/// Which days of the grid get an event dot and/or a task dot.
#[derive(Debug, Default, PartialEq)]
pub struct DayMarks {
    pub events: HashSet<NaiveDate>,
    pub tasks: HashSet<NaiveDate>,
}

pub fn day_marks<Tz: TimeZone>(events: &[Event], open_tasks: &[Task], tz: &Tz) -> DayMarks {
    let mut marks = DayMarks::default();
    for e in events {
        if let Some((first, last)) = event_span_days(e, tz) {
            let mut d = first;
            while d <= last {
                marks.events.insert(d);
                match d.succ_opt() {
                    Some(next) => d = next,
                    None => break,
                }
            }
        }
    }
    marks.tasks.extend(open_tasks.iter().filter_map(|t| t.due_date));
    marks
}

/// Move the task selection by `delta` rows within `ids`, clamping at both ends.
/// No current selection: forward starts at the first row, backward at the last.
pub fn step_selection(ids: &[i64], current: Option<i64>, delta: i32) -> Option<i64> {
    if ids.is_empty() {
        return None;
    }
    let last = ids.len() as i64 - 1;
    let next = match current.and_then(|c| ids.iter().position(|&i| i == c)) {
        Some(pos) => (pos as i64 + i64::from(delta)).clamp(0, last),
        None if delta >= 0 => 0,
        None => last,
    };
    Some(ids[next as usize])
}

/// Pick the selection after the list changed: keep `prev` if still present, otherwise the row now
/// sitting at (or just before) the old index.
pub fn reselect(ids: &[i64], prev: Option<i64>, prev_index: Option<usize>) -> Option<i64> {
    if ids.is_empty() {
        return None;
    }
    if let Some(p) = prev.filter(|p| ids.contains(p)) {
        return Some(p);
    }
    Some(ids[prev_index.unwrap_or(0).min(ids.len() - 1)])
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveTime, Utc};

    use super::*;
    use crate::db::Priority;

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn grid_starts_on_monday_and_covers_six_weeks() {
        // 1 Nov 2026 is a Sunday: the grid must begin Monday 26 Oct.
        let days = grid_days(d("2026-11-13"));
        assert_eq!(days.len(), GRID_DAYS);
        assert_eq!(days[0], d("2026-10-26"));
        assert_eq!(days[6], d("2026-11-01"));
        assert_eq!(days[41], d("2026-12-06"));
        assert!(days.iter().step_by(7).all(|x| x.weekday().num_days_from_monday() == 0));
    }

    #[test]
    fn grid_when_first_is_monday_has_no_leading_days() {
        // 1 Jun 2026 is a Monday.
        assert_eq!(grid_days(d("2026-06-17"))[0], d("2026-06-01"));
    }

    #[test]
    fn day_steps_cross_month_and_year_boundaries() {
        assert_eq!(move_day(d("2026-12-31"), Step::Right), d("2027-01-01"));
        assert_eq!(move_day(d("2026-03-01"), Step::Left), d("2026-02-28"));
        assert_eq!(move_day(d("2026-11-13"), Step::Down), d("2026-11-20"));
        assert_eq!(move_day(d("2026-01-03"), Step::Up), d("2025-12-27"));
        assert_eq!(move_day(NaiveDate::MAX, Step::Right), NaiveDate::MAX);
    }

    #[test]
    fn month_shift_clamps_day() {
        assert_eq!(shift_months(d("2026-01-31"), 1), d("2026-02-28"));
        assert_eq!(shift_months(d("2024-03-31"), -1), d("2024-02-29"));
        assert_eq!(shift_months(d("2026-11-13"), 14), d("2028-01-13"));
        assert_eq!(first_of_month(d("2026-11-13")), d("2026-11-01"));
        assert_eq!(month_label(d("2026-11-13")), "November 2026");
    }

    fn event(start_ts: i64, end_ts: i64) -> Event {
        Event {
            calendar_id: "c".into(),
            id: "e".into(),
            summary: "s".into(),
            description: String::new(),
            location: None,
            all_day: false,
            start_ts,
            end_ts,
            meet_link: None,
            html_link: None,
            use_default_reminders: true,
            reminders: vec![],
        }
    }

    fn at(tz: &FixedOffset, day: &str, h: u32) -> i64 {
        d(day).and_time(NaiveTime::from_hms_opt(h, 0, 0).unwrap()).and_local_timezone(*tz).unwrap().timestamp()
    }

    #[test]
    fn event_spans_use_local_days_and_exclusive_end() {
        let tz = FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        // Ends exactly at local midnight: must not leak into the next day.
        let e = event(at(&tz, "2026-11-13", 22), at(&tz, "2026-11-14", 0));
        assert_eq!(event_span_days(&e, &tz), Some((d("2026-11-13"), d("2026-11-13"))));
        let multi = event(at(&tz, "2026-11-13", 22), at(&tz, "2026-11-15", 1));
        assert_eq!(event_span_days(&multi, &tz), Some((d("2026-11-13"), d("2026-11-15"))));
        // Zero-length events still land on their day.
        let instant = event(at(&tz, "2026-11-13", 9), at(&tz, "2026-11-13", 9));
        assert_eq!(event_span_days(&instant, &tz), Some((d("2026-11-13"), d("2026-11-13"))));
    }

    #[test]
    fn marks_cover_every_spanned_day_and_task_dates() {
        let tz = Utc;
        let multi = event(at(&FixedOffset::east_opt(0).unwrap(), "2026-11-13", 10), at(&FixedOffset::east_opt(0).unwrap(), "2026-11-15", 10));
        let task = Task {
            id: 1,
            title: "t".into(),
            due_date: Some(d("2026-11-20")),
            due_time: None,
            notes: String::new(),
            priority: Priority::None,
            tags: vec![],
            links: vec![],
            created_at: Utc::now(),
            updated_at: Utc::now(),
            completed_at: None,
            deleted_at: None,
        };
        let undated = Task { id: 2, due_date: None, ..task.clone() };
        let marks = day_marks(&[multi], &[task, undated], &tz);
        assert_eq!(marks.events, HashSet::from([d("2026-11-13"), d("2026-11-14"), d("2026-11-15")]));
        assert_eq!(marks.tasks, HashSet::from([d("2026-11-20")]));
    }

    #[test]
    fn selection_stepping_clamps() {
        let ids = [10, 20, 30];
        assert_eq!(step_selection(&ids, Some(10), 1), Some(20));
        assert_eq!(step_selection(&ids, Some(30), 1), Some(30));
        assert_eq!(step_selection(&ids, Some(10), -1), Some(10));
        assert_eq!(step_selection(&ids, None, 1), Some(10));
        assert_eq!(step_selection(&ids, None, -1), Some(30));
        assert_eq!(step_selection(&ids, Some(99), 1), Some(10));
        assert_eq!(step_selection(&[], Some(1), 1), None);
    }

    #[test]
    fn reselect_prefers_same_task_then_same_slot() {
        assert_eq!(reselect(&[1, 2, 3], Some(2), Some(0)), Some(2));
        assert_eq!(reselect(&[1, 3], Some(2), Some(1)), Some(3));
        assert_eq!(reselect(&[1, 3], Some(2), Some(5)), Some(3));
        assert_eq!(reselect(&[1, 3], None, None), Some(1));
        assert_eq!(reselect(&[], Some(2), Some(0)), None);
    }
}
