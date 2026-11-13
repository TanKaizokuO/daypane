//! Google Calendar API v3 backend (read-only) over `ureq`.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration as Span, SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use url::Url;

use super::map::{map_calendar, map_event, RawCalendarEntry, RawEvent};
use super::{CalendarBackend, EventChange, RemoteCalendar, SyncOutcome};

const API_BASE: &str = "https://www.googleapis.com/calendar/v3";
const PAGE_SIZE: &str = "250";
/// How far back the initial full sync reaches.
const FULL_SYNC_LOOKBACK_DAYS: i64 = 30;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct GoogleBackend {
    agent: ureq::Agent,
    access_token: String,
}

impl GoogleBackend {
    pub fn new(access_token: String) -> Self {
        Self {
            agent: http_agent(),
            access_token,
        }
    }

    fn get_json<T: DeserializeOwned>(&self, url: &Url) -> Result<Fetched<T>> {
        let mut resp = self
            .agent
            .get(url.as_str())
            .header("Authorization", format!("Bearer {}", self.access_token))
            .call()
            .with_context(|| format!("requesting {}", url.path()))?;
        match resp.status().as_u16() {
            200..=299 => {
                Ok(Fetched::Body(resp.body_mut().read_json().with_context(
                    || format!("decoding response of {}", url.path()),
                )?))
            }
            410 => Ok(Fetched::Gone),
            status => {
                let body = resp.body_mut().read_to_string().unwrap_or_default();
                bail!(
                    "Google Calendar API {} returned HTTP {status}: {}",
                    url.path(),
                    truncate(&body, 300)
                )
            }
        }
    }
}

/// Shared agent: HTTP error statuses are returned, not raised, so 410 can be told apart.
pub(super) fn http_agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(HTTP_TIMEOUT))
            .build(),
    )
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

enum Fetched<T> {
    Body(T),
    Gone,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CalendarListPage {
    #[serde(default)]
    items: Vec<RawCalendarEntry>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsPage {
    #[serde(default)]
    items: Vec<RawEvent>,
    next_page_token: Option<String>,
    next_sync_token: Option<String>,
}

fn base_url(segments: &[&str]) -> Url {
    let mut url = Url::parse(API_BASE).expect("constant URL");
    url.path_segments_mut()
        .expect("https URL has a path")
        .extend(segments);
    url
}

fn calendar_list_url(page_token: Option<&str>) -> Url {
    let mut url = base_url(&["users", "me", "calendarList"]);
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("maxResults", PAGE_SIZE);
        // Free/busy-only calendars cannot list events (403).
        q.append_pair("minAccessRole", "reader");
        if let Some(t) = page_token {
            q.append_pair("pageToken", t);
        }
    }
    url
}

/// Events list URL. Google forbids `timeMin` together with `syncToken`, so the lookback bound
/// applies to full syncs only; incremental syncs ask for deletions explicitly.
fn events_url(
    calendar_id: &str,
    sync_token: Option<&str>,
    page_token: Option<&str>,
    now: DateTime<Utc>,
) -> Url {
    let mut url = base_url(&["calendars", calendar_id, "events"]);
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("singleEvents", "true");
        q.append_pair("maxResults", PAGE_SIZE);
        match sync_token {
            Some(token) => {
                q.append_pair("syncToken", token);
                q.append_pair("showDeleted", "true");
            }
            None => {
                let min = now - Span::days(FULL_SYNC_LOOKBACK_DAYS);
                q.append_pair("timeMin", &min.to_rfc3339_opts(SecondsFormat::Secs, true));
            }
        }
        if let Some(t) = page_token {
            q.append_pair("pageToken", t);
        }
    }
    url
}

impl CalendarBackend for GoogleBackend {
    fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
        let mut out = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let url = calendar_list_url(page_token.as_deref());
            let Fetched::Body(page) = self.get_json::<CalendarListPage>(&url)? else {
                bail!("calendar list request unexpectedly returned 410 Gone");
            };
            out.extend(page.items.iter().filter_map(map_calendar));
            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => return Ok(out),
            }
        }
    }

    fn sync_events(
        &self,
        calendar_id: &str,
        sync_token: Option<&str>,
        on_page: &mut dyn FnMut(Vec<EventChange>) -> Result<()>,
    ) -> Result<SyncOutcome> {
        let mut page_token: Option<String> = None;
        loop {
            let url = events_url(calendar_id, sync_token, page_token.as_deref(), Utc::now());
            let Fetched::Body(page) = self.get_json::<EventsPage>(&url)? else {
                return Ok(SyncOutcome::Gone);
            };
            on_page(
                page.items
                    .iter()
                    .filter_map(|e| map_event(calendar_id, e))
                    .collect(),
            )?;
            match page.next_page_token {
                Some(t) => page_token = Some(t),
                None => {
                    return Ok(SyncOutcome::Complete {
                        next_sync_token: page.next_sync_token,
                    })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 13, 12, 0, 0).unwrap()
    }

    fn query(url: &Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    fn has(q: &[(String, String)], k: &str, v: &str) -> bool {
        q.iter().any(|(a, b)| a == k && b == v)
    }

    #[test]
    fn calendar_ids_are_path_encoded() {
        let url = events_url(
            "en.indian#holiday@group.v.calendar.google.com",
            None,
            None,
            now(),
        );
        assert_eq!(
            url.path(),
            "/calendar/v3/calendars/en.indian%23holiday@group.v.calendar.google.com/events"
        );
        let url = events_url("a/b?c", None, None, now());
        assert_eq!(url.path(), "/calendar/v3/calendars/a%2Fb%3Fc/events");
        assert_eq!(url.host_str(), Some("www.googleapis.com"));
    }

    #[test]
    fn full_sync_is_bounded_and_has_no_sync_token() {
        let q = query(&events_url("c", None, None, now()));
        assert!(has(&q, "singleEvents", "true"));
        assert!(has(&q, "timeMin", "2026-10-14T12:00:00Z"));
        assert!(!q
            .iter()
            .any(|(k, _)| k == "syncToken" || k == "showDeleted"));
    }

    #[test]
    fn incremental_sync_never_sends_time_min() {
        let q = query(&events_url("c", Some("tok=="), Some("page2"), now()));
        assert!(has(&q, "syncToken", "tok=="));
        assert!(has(&q, "showDeleted", "true"));
        assert!(has(&q, "singleEvents", "true"));
        assert!(has(&q, "pageToken", "page2"));
        assert!(!q.iter().any(|(k, _)| k == "timeMin"));
    }

    #[test]
    fn calendar_list_url_paginates() {
        let url = calendar_list_url(Some("next"));
        assert_eq!(url.path(), "/calendar/v3/users/me/calendarList");
        assert!(has(&query(&url), "pageToken", "next"));
        assert!(!query(&calendar_list_url(None))
            .iter()
            .any(|(k, _)| k == "pageToken"));
    }

    #[test]
    fn events_page_fixture_decodes_and_maps() {
        let page: EventsPage = serde_json::from_str(
            r#"{
              "kind": "calendar#events", "nextSyncToken": "SYNC1",
              "items": [
                {"id": "a", "status": "confirmed", "summary": "Lunch",
                 "start": {"dateTime": "2026-11-13T12:00:00Z"}, "end": {"dateTime": "2026-11-13T13:00:00Z"}},
                {"id": "b", "status": "cancelled"},
                {"id": "c", "summary": "broken"}
              ]
            }"#,
        )
        .unwrap();
        assert!(page.next_page_token.is_none());
        assert_eq!(page.next_sync_token.as_deref(), Some("SYNC1"));
        let changes: Vec<_> = page
            .items
            .iter()
            .filter_map(|e| map_event("cal", e))
            .collect();
        assert_eq!(changes.len(), 2);
        assert!(matches!(&changes[0], EventChange::Upsert(e) if e.id == "a"));
        assert_eq!(changes[1], EventChange::Delete("b".into()));
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("héllo wörld", 4), "héll…");
        assert_eq!(truncate("ab", 4), "ab");
    }
}
