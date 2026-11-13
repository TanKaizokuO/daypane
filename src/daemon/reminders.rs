//! Pure reminder scheduling: which reminders are due *right now*.
//!
//! Missed reminders are dropped by construction: a reminder is due only when its fire time lies in
//! `[earliest_ts, now_ts]`. Anything older is never queued, so suspend/resume cannot replay it.

use chrono::{DateTime, TimeZone};

use super::waybar::format_minutes;
use crate::db::{Calendar, Event, Reminder, Task};

/// A reminder may fire at most this many seconds after its fire time.
pub const GRACE_SECS: i64 = 60;
/// Seconds between scheduler ticks.
pub const TICK_SECS: u64 = 15;
/// A wall-clock gap between ticks larger than this (or negative) means suspend / clock change.
const JUMP_SECS: i64 = 3 * TICK_SECS as i64;
/// Google allows reminders up to four weeks before an event; look this far ahead.
pub const HORIZON_SECS: i64 = 29 * 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueReminder {
    /// Dedupe key for `Db::mark_fired`.
    pub key: String,
    pub title: String,
    pub body: String,
    pub fire_ts: i64,
}

/// Earliest fire time still worth delivering. After a wall-clock jump (suspend/resume, clock
/// change) everything that fell into the gap is dropped, not just what exceeds the grace window.
pub fn window_start(now_ts: i64, last_tick_ts: i64, grace_secs: i64) -> i64 {
    let gap = now_ts - last_tick_ts;
    if !(0..=JUMP_SECS).contains(&gap) {
        now_ts
    } else {
        now_ts - grace_secs
    }
}

/// Reminders with `earliest_ts <= fire_time <= now_ts`, sorted by fire time.
///
/// * Events use their own reminders, or the calendar's defaults when `use_default_reminders`.
///   All-day events fire relative to their start (local midnight).
/// * Tasks fire `task_offset_minutes` before `due_date + due_time`; date-only tasks never fire.
pub fn due_reminders<Tz: TimeZone>(
    events: &[Event],
    calendars: &[Calendar],
    tasks: &[Task],
    task_offset_minutes: i64,
    earliest_ts: i64,
    now_ts: i64,
    tz: &Tz,
) -> Vec<DueReminder>
where
    Tz::Offset: std::fmt::Display,
{
    let mut out: Vec<DueReminder> = Vec::new();
    let mut push = |r: DueReminder| {
        if (earliest_ts..=now_ts).contains(&r.fire_ts) && !out.iter().any(|o| o.key == r.key) {
            out.push(r);
        }
    };

    for e in events {
        let defaults: &[Reminder] = if e.use_default_reminders {
            calendars
                .iter()
                .find(|c| c.id == e.calendar_id)
                .map_or(&[], |c| c.default_reminders.as_slice())
        } else {
            &[]
        };
        let own: &[Reminder] = if e.use_default_reminders { &[] } else { &e.reminders };
        for r in defaults.iter().chain(own) {
            if r.minutes < 0 {
                continue;
            }
            push(DueReminder {
                key: format!("ev:{}:{}:{}:{}", e.calendar_id, e.id, e.start_ts, r.minutes),
                title: if e.summary.trim().is_empty() { "(no title)".to_owned() } else { e.summary.clone() },
                body: event_body(e, r.minutes, tz),
                fire_ts: e.start_ts - r.minutes * 60,
            });
        }
    }

    for t in tasks {
        let (Some(date), Some(time)) = (t.due_date, t.due_time) else { continue };
        if !t.is_open() {
            continue;
        }
        let Some(due) = tz.from_local_datetime(&date.and_time(time)).earliest() else { continue };
        let due_ts = due.timestamp();
        push(DueReminder {
            key: format!("task:{}:{}", t.id, due_ts),
            title: t.title.clone(),
            body: format!("Due at {}", time.format("%H:%M")),
            fire_ts: due_ts - task_offset_minutes * 60,
        });
    }

    out.sort_by_key(|r| r.fire_ts);
    out
}

fn event_body<Tz: TimeZone>(e: &Event, minutes: i64, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let start: Option<DateTime<Tz>> = tz.timestamp_opt(e.start_ts, 0).single();
    let when = match (&start, e.all_day) {
        (Some(s), true) => format!("All day, {}", s.format("%a %b %-d")),
        (Some(s), false) => s.format("%H:%M").to_string(),
        (None, _) => String::new(),
    };
    let lead = if minutes == 0 { "starting now".to_owned() } else { format!("in {}", format_minutes(minutes)) };
    let mut body = if when.is_empty() { lead } else { format!("{when} · {lead}") };
    if let Some(loc) = e.location.as_deref().filter(|l| !l.trim().is_empty()) {
        body.push_str(&format!("\n{loc}"));
    }
    body
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, NaiveDate, NaiveTime, Utc};

    use super::*;
    use crate::db::Priority;

    fn tz() -> FixedOffset {
        FixedOffset::east_opt(2 * 3600).unwrap()
    }

    fn midnight_ts(tz: &FixedOffset, d: NaiveDate) -> i64 {
        tz.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).earliest().unwrap().timestamp()
    }

    fn date(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    fn ts(d: u32, h: u32, m: u32) -> i64 {
        tz().from_local_datetime(&date(d).and_hms_opt(h, m, 0).unwrap()).unwrap().timestamp()
    }

    fn event(id: &str, start: i64, use_default: bool, reminders: &[i64]) -> Event {
        Event {
            calendar_id: "cal".into(),
            id: id.into(),
            summary: "Standup".into(),
            description: String::new(),
            location: None,
            all_day: false,
            start_ts: start,
            end_ts: start + 1800,
            meet_link: None,
            html_link: None,
            use_default_reminders: use_default,
            reminders: reminders.iter().map(|&minutes| Reminder { minutes }).collect(),
        }
    }

    fn calendar(defaults: &[i64]) -> Calendar {
        Calendar {
            id: "cal".into(),
            summary: "Cal".into(),
            color: None,
            selected: true,
            sync_token: None,
            default_reminders: defaults.iter().map(|&minutes| Reminder { minutes }).collect(),
        }
    }

    fn task(id: i64, due: Option<(u32, Option<(u32, u32)>)>) -> Task {
        Task {
            id,
            title: format!("task {id}"),
            due_date: due.map(|(d, _)| date(d)),
            due_time: due.and_then(|(_, t)| t).map(|(h, m)| NaiveTime::from_hms_opt(h, m, 0).unwrap()),
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

    fn run(events: &[Event], cals: &[Calendar], tasks: &[Task], now: i64) -> Vec<DueReminder> {
        due_reminders(events, cals, tasks, 10, now - GRACE_SECS, now, &tz())
    }

    #[test]
    fn fires_at_start_minus_minutes() {
        let e = event("a", ts(13, 10, 0), false, &[10]);
        let got = run(&[e], &[], &[], ts(13, 9, 50) + 5);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].fire_ts, ts(13, 9, 50));
        assert_eq!(got[0].title, "Standup");
        assert_eq!(got[0].key, format!("ev:cal:a:{}:10", ts(13, 10, 0)));
        assert_eq!(got[0].body, "10:00 · in 10m");
    }

    #[test]
    fn not_yet_due_is_not_fired() {
        let e = event("a", ts(13, 10, 0), false, &[10]);
        assert!(run(&[e], &[], &[], ts(13, 9, 49)).is_empty());
    }

    #[test]
    fn missed_reminders_are_dropped_not_queued() {
        let e = event("a", ts(13, 10, 0), false, &[10]);
        // 61 s late: beyond grace. Hours later (resume from suspend): also dropped.
        assert!(run(std::slice::from_ref(&e), &[], &[], ts(13, 9, 50) + 61).is_empty());
        assert!(run(std::slice::from_ref(&e), &[], &[], ts(13, 12, 0)).is_empty());
        // Exactly at the grace boundary still fires.
        assert_eq!(run(&[e], &[], &[], ts(13, 9, 50) + 60).len(), 1);
    }

    #[test]
    fn default_reminders_apply_only_when_flagged() {
        let cal = calendar(&[30]);
        let defaulted = event("d", ts(13, 10, 0), true, &[10]);
        let overridden = event("o", ts(13, 10, 0), false, &[10]);
        let got = run(std::slice::from_ref(&defaulted), std::slice::from_ref(&cal), &[], ts(13, 9, 30));
        assert_eq!(got.len(), 1);
        assert!(got[0].key.ends_with(":30"));
        // Its own 10-minute override is ignored while defaults are in use.
        assert!(run(&[defaulted], std::slice::from_ref(&cal), &[], ts(13, 9, 50)).is_empty());
        // Override: own 10 min fires, the calendar's 30 does not.
        assert!(run(std::slice::from_ref(&overridden), std::slice::from_ref(&cal), &[], ts(13, 9, 30)).is_empty());
        let got = run(&[overridden], &[cal], &[], ts(13, 9, 50));
        assert_eq!(got.len(), 1);
        assert!(got[0].key.ends_with(":10"));
    }

    #[test]
    fn event_with_unknown_calendar_or_no_reminders_is_silent() {
        let e = event("a", ts(13, 10, 0), true, &[]);
        assert!(run(&[e], &[calendar(&[])], &[], ts(13, 10, 0)).is_empty());
        let e = event("b", ts(13, 10, 0), false, &[]);
        assert!(run(&[e], &[calendar(&[10])], &[], ts(13, 9, 50)).is_empty());
    }

    #[test]
    fn duplicate_minutes_collapse_and_multiple_reminders_sort() {
        let e = event("a", ts(13, 10, 0), false, &[10, 10, 0]);
        let got = run(&[e], &[], &[], ts(13, 10, 0));
        // Only the 0-minute one is in the window; the 10-minute one is 600 s old.
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "10:00 · starting now");

        let e = event("a", ts(13, 10, 0), false, &[10, 10]);
        let got = due_reminders(&[e], &[], &[], 10, ts(13, 9, 0), ts(13, 10, 0), &tz());
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn all_day_event_fires_relative_to_local_midnight() {
        let mut e = event("a", midnight_ts(&tz(), date(14)), false, &[15 * 60]);
        e.all_day = true;
        e.summary = "Holiday".into();
        // 15 h before midnight = 09:00 the day before.
        let got = run(&[e], &[], &[], ts(13, 9, 0));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].body, "All day, Sat Nov 14 · in 15h");
    }

    #[test]
    fn timed_task_fires_at_configured_offset() {
        let t = task(7, Some((13, Some((14, 30)))));
        let got = run(&[], &[], std::slice::from_ref(&t), ts(13, 14, 20));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].title, "task 7");
        assert_eq!(got[0].body, "Due at 14:30");
        assert_eq!(got[0].key, format!("task:7:{}", ts(13, 14, 30)));
        assert!(run(&[], &[], &[t], ts(13, 14, 19)).is_empty());
    }

    #[test]
    fn date_only_and_closed_tasks_never_fire() {
        let date_only = task(1, Some((13, None)));
        let undated = task(2, None);
        let mut done = task(3, Some((13, Some((14, 30)))));
        done.completed_at = Some(Utc::now());
        for now in [ts(13, 0, 0), ts(13, 9, 0), ts(13, 14, 20), ts(13, 23, 59)] {
            assert!(run(&[], &[], &[date_only.clone(), undated.clone(), done.clone()], now).is_empty());
        }
    }

    #[test]
    fn window_start_drops_everything_after_a_clock_jump() {
        let now = 1_000_000;
        assert_eq!(window_start(now, now - 15, GRACE_SECS), now - GRACE_SECS);
        assert_eq!(window_start(now, now - 45, GRACE_SECS), now - GRACE_SECS);
        // Resume from suspend: 10 minutes passed since last tick.
        assert_eq!(window_start(now, now - 600, GRACE_SECS), now);
        // Clock set backwards.
        assert_eq!(window_start(now, now + 30, GRACE_SECS), now);
    }

    #[test]
    fn reminder_inside_suspend_gap_is_dropped_even_within_grace() {
        let e = event("a", ts(13, 10, 0), false, &[10]);
        let now = ts(13, 9, 50) + 30; // woke 30 s after it should have fired
        let earliest = window_start(now, now - 3600, GRACE_SECS);
        assert!(due_reminders(&[e], &[], &[], 10, earliest, now, &tz()).is_empty());
    }
}
