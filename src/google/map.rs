//! Pure mapping from Calendar API v3 JSON resources to cache types. No I/O.

use chrono::{DateTime, NaiveDate};
use serde::Deserialize;

use super::{EventChange, RemoteCalendar};
use crate::db::{local_day_bounds, Event, Reminder};

const NO_TITLE: &str = "(no title)";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEvent {
    pub id: String,
    pub status: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub start: Option<RawTime>,
    pub end: Option<RawTime>,
    pub hangout_link: Option<String>,
    pub html_link: Option<String>,
    pub conference_data: Option<RawConference>,
    pub reminders: Option<RawReminders>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawTime {
    /// `YYYY-MM-DD` for all-day events.
    pub date: Option<String>,
    /// RFC 3339 for timed events.
    pub date_time: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawConference {
    #[serde(default)]
    pub entry_points: Vec<RawEntryPoint>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawEntryPoint {
    pub entry_point_type: Option<String>,
    pub uri: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawReminders {
    #[serde(default)]
    pub use_default: bool,
    #[serde(default)]
    pub overrides: Vec<RawReminder>,
}

#[derive(Debug, Deserialize)]
pub struct RawReminder {
    pub method: Option<String>,
    pub minutes: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawCalendarEntry {
    pub id: String,
    pub summary: Option<String>,
    pub summary_override: Option<String>,
    pub background_color: Option<String>,
    #[serde(default)]
    pub default_reminders: Vec<RawReminder>,
    #[serde(default)]
    pub deleted: bool,
}

/// Only `popup` reminders are of interest (we notify; we never send e-mail).
fn popup_reminders(raw: &[RawReminder]) -> Vec<Reminder> {
    raw.iter()
        .filter(|r| r.method.as_deref() == Some("popup"))
        .filter_map(|r| r.minutes)
        .map(|minutes| Reminder { minutes })
        .collect()
}

/// `None` for deleted calendars.
pub fn map_calendar(raw: &RawCalendarEntry) -> Option<RemoteCalendar> {
    if raw.deleted {
        return None;
    }
    let summary = [&raw.summary_override, &raw.summary]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| raw.id.clone());
    Some(RemoteCalendar {
        id: raw.id.clone(),
        summary,
        color: raw.background_color.clone(),
        default_reminders: popup_reminders(&raw.default_reminders),
    })
}

/// Start/end as unix seconds plus the all-day flag. `None` if the times are missing or malformed.
fn times(raw: &RawEvent) -> Option<(bool, i64, i64)> {
    let start = raw.start.as_ref()?;
    if let Some(d) = &start.date {
        let first = NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
        let start_ts = local_day_bounds(first).0;
        // `end.date` is exclusive; a missing end means a single day.
        let end_ts = match raw.end.as_ref().and_then(|e| e.date.as_deref()) {
            Some(e) => local_day_bounds(NaiveDate::parse_from_str(e, "%Y-%m-%d").ok()?).0,
            None => local_day_bounds(first).1,
        };
        return Some((true, start_ts, end_ts.max(start_ts)));
    }
    let start_ts = DateTime::parse_from_rfc3339(start.date_time.as_deref()?).ok()?.timestamp();
    let end_ts = match raw.end.as_ref().and_then(|e| e.date_time.as_deref()) {
        Some(e) => DateTime::parse_from_rfc3339(e).ok()?.timestamp(),
        None => start_ts,
    };
    Some((false, start_ts, end_ts.max(start_ts)))
}

fn meet_link(raw: &RawEvent) -> Option<String> {
    raw.hangout_link
        .clone()
        .or_else(|| {
            raw.conference_data.as_ref()?.entry_points.iter().find_map(|p| {
                (p.entry_point_type.as_deref() == Some("video")).then(|| p.uri.clone()).flatten()
            })
        })
        .filter(|l| !l.is_empty())
}

/// Map one API event. Cancelled events become deletions (they may carry nothing but an id).
/// `None` = the event cannot be represented (missing or malformed start) and is skipped.
pub fn map_event(calendar_id: &str, raw: &RawEvent) -> Option<EventChange> {
    if raw.status.as_deref() == Some("cancelled") {
        return Some(EventChange::Delete(raw.id.clone()));
    }
    let (all_day, start_ts, end_ts) = times(raw)?;
    let (use_default_reminders, reminders) = match &raw.reminders {
        Some(r) => (r.use_default, popup_reminders(&r.overrides)),
        None => (false, Vec::new()),
    };
    Some(EventChange::Upsert(Event {
        calendar_id: calendar_id.to_owned(),
        id: raw.id.clone(),
        summary: raw.summary.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| NO_TITLE.to_owned()),
        description: raw.description.clone().unwrap_or_default(),
        location: raw.location.clone().filter(|l| !l.is_empty()),
        all_day,
        start_ts,
        end_ts,
        meet_link: meet_link(raw),
        html_link: raw.html_link.clone(),
        use_default_reminders,
        reminders,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn raw(v: serde_json::Value) -> RawEvent {
        serde_json::from_value(v).unwrap()
    }

    fn upsert(c: Option<EventChange>) -> Event {
        match c {
            Some(EventChange::Upsert(e)) => e,
            other => panic!("expected upsert, got {other:?}"),
        }
    }

    #[test]
    fn timed_event_with_meet_and_reminders() {
        let e = upsert(map_event(
            "cal",
            &raw(json!({
                "id": "e1", "status": "confirmed", "summary": "Standup",
                "description": "daily", "location": "Room 4",
                "htmlLink": "https://calendar.google.com/event?eid=abc",
                "hangoutLink": "https://meet.google.com/aaa-bbbb-ccc",
                "start": {"dateTime": "2026-11-13T10:00:00+05:30"},
                "end": {"dateTime": "2026-11-13T10:30:00+05:30"},
                "reminders": {"useDefault": false, "overrides": [
                    {"method": "popup", "minutes": 10},
                    {"method": "email", "minutes": 60},
                    {"method": "popup", "minutes": 1}
                ]}
            })),
        ));
        assert_eq!(e.calendar_id, "cal");
        assert!(!e.all_day);
        // 2026-11-13T04:30:00Z
        assert_eq!(e.start_ts, 1_794_544_200);
        assert_eq!(e.end_ts - e.start_ts, 30 * 60);
        assert_eq!(e.meet_link.as_deref(), Some("https://meet.google.com/aaa-bbbb-ccc"));
        assert_eq!(e.html_link.as_deref(), Some("https://calendar.google.com/event?eid=abc"));
        assert_eq!(e.location.as_deref(), Some("Room 4"));
        assert!(!e.use_default_reminders);
        assert_eq!(e.reminders, vec![Reminder { minutes: 10 }, Reminder { minutes: 1 }]);
    }

    #[test]
    fn all_day_end_is_exclusive_local_midnight() {
        let e = upsert(map_event(
            "cal",
            &raw(json!({
                "id": "e2", "summary": "Trip",
                "start": {"date": "2026-11-13"}, "end": {"date": "2026-11-15"},
                "reminders": {"useDefault": true}
            })),
        ));
        let d = |s: &str| local_day_bounds(NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()).0;
        assert!(e.all_day);
        assert_eq!(e.start_ts, d("2026-11-13"));
        assert_eq!(e.end_ts, d("2026-11-15"));
        assert!(e.use_default_reminders);
        assert!(e.reminders.is_empty());
    }

    #[test]
    fn all_day_without_end_spans_one_day() {
        let e = upsert(map_event("cal", &raw(json!({"id": "e", "start": {"date": "2026-11-13"}}))));
        assert_eq!(e.end_ts, local_day_bounds(NaiveDate::from_ymd_opt(2026, 11, 14).unwrap()).0);
    }

    #[test]
    fn missing_title_and_meet_from_conference_data() {
        let e = upsert(map_event(
            "cal",
            &raw(json!({
                "id": "e3", "summary": "",
                "start": {"dateTime": "2026-11-13T10:00:00Z"}, "end": {"dateTime": "2026-11-13T11:00:00Z"},
                "conferenceData": {"entryPoints": [
                    {"entryPointType": "phone", "uri": "tel:+1555"},
                    {"entryPointType": "video", "uri": "https://zoom.example/j/1"}
                ]}
            })),
        ));
        assert_eq!(e.summary, "(no title)");
        assert_eq!(e.description, "");
        assert_eq!(e.meet_link.as_deref(), Some("https://zoom.example/j/1"));
        assert!(!e.use_default_reminders);
    }

    #[test]
    fn hangout_link_wins_over_conference_data() {
        let e = upsert(map_event(
            "cal",
            &raw(json!({
                "id": "e4", "hangoutLink": "https://meet.google.com/x",
                "start": {"dateTime": "2026-11-13T10:00:00Z"}, "end": {"dateTime": "2026-11-13T11:00:00Z"},
                "conferenceData": {"entryPoints": [{"entryPointType": "video", "uri": "https://other"}]}
            })),
        ));
        assert_eq!(e.meet_link.as_deref(), Some("https://meet.google.com/x"));
    }

    #[test]
    fn cancelled_event_is_a_deletion_even_without_times() {
        assert_eq!(
            map_event("cal", &raw(json!({"id": "gone", "status": "cancelled"}))),
            Some(EventChange::Delete("gone".into()))
        );
    }

    #[test]
    fn unrepresentable_event_is_skipped() {
        assert_eq!(map_event("cal", &raw(json!({"id": "x", "summary": "no times"}))), None);
        assert_eq!(map_event("cal", &raw(json!({"id": "x", "start": {"dateTime": "garbage"}}))), None);
    }

    #[test]
    fn calendar_mapping() {
        let entry: RawCalendarEntry = serde_json::from_value(json!({
            "id": "me@example.com", "summary": "me@example.com", "summaryOverride": "Personal",
            "backgroundColor": "#9fe1e7",
            "defaultReminders": [{"method": "popup", "minutes": 30}, {"method": "email", "minutes": 1440}]
        }))
        .unwrap();
        let c = map_calendar(&entry).unwrap();
        assert_eq!(c.summary, "Personal");
        assert_eq!(c.color.as_deref(), Some("#9fe1e7"));
        assert_eq!(c.default_reminders, vec![Reminder { minutes: 30 }]);

        let deleted: RawCalendarEntry = serde_json::from_value(json!({"id": "d", "deleted": true})).unwrap();
        assert!(map_calendar(&deleted).is_none());

        let bare: RawCalendarEntry = serde_json::from_value(json!({"id": "only-id"})).unwrap();
        assert_eq!(map_calendar(&bare).unwrap().summary, "only-id");
    }
}
