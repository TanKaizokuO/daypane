//! Google Calendar (read-only): OAuth login, Secret Service token storage, and cache sync.
//!
//! Layout: `oauth` (installed-app flow, PKCE), `secret` (refresh token in the keyring), `api`
//! (Calendar API v3 over `ureq`), `map` (pure JSON → cache mapping). The sync algorithm in this
//! file only knows the [`CalendarBackend`] trait, so another backend (CalDAV) can be added later.
//!
//! **Threading:** everything here is blocking (HTTP, keyring). Callers inside an async runtime
//! MUST run [`sync_all`] / [`login`] via `tokio::task::spawn_blocking`. Keyring access drives its
//! own runtime on a dedicated thread, so it never panics with a nested `block_on`.

mod api;
mod map;
mod oauth;
mod secret;

use anyhow::{bail, Result};

use crate::app::APP_NAME;
use crate::config::Config;
use crate::db::{Calendar, Db, Event, Reminder};

/// A calendar as reported by the remote service.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteCalendar {
    pub id: String,
    pub summary: String,
    pub color: Option<String>,
    /// Popup reminders only.
    pub default_reminders: Vec<Reminder>,
}

/// One change to the event cache.
#[derive(Debug, Clone, PartialEq)]
pub enum EventChange {
    Upsert(Event),
    /// Event id.
    Delete(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// All pages were delivered. `next_sync_token` is the cursor for the next incremental sync.
    Complete { next_sync_token: Option<String> },
    /// The sync token is no longer valid (HTTP 410): clear the cache and run a full sync.
    Gone,
}

pub trait CalendarBackend {
    /// All calendars of the account (all pages).
    fn list_calendars(&self) -> Result<Vec<RemoteCalendar>>;

    /// Fetch changes of one calendar, page by page. `sync_token = None` is a full sync.
    /// `on_page` is invoked once per page, in order, so callers can persist incrementally.
    fn sync_events(
        &self,
        calendar_id: &str,
        sync_token: Option<&str>,
        on_page: &mut dyn FnMut(Vec<EventChange>) -> Result<()>,
    ) -> Result<SyncOutcome>;
}

/// Interactive OAuth (loopback + PKCE), stores the refresh token in the Secret Service, then
/// syncs once and prints the calendars found.
pub fn login() -> Result<()> {
    let cfg = Config::load()?;
    let client = oauth::Client::require(&cfg)?;
    let refresh_token = oauth::authorize(client)?;
    secret::store_refresh_token(&refresh_token)?;
    println!("Authorized. Syncing calendars…");

    let db = Db::open_default()?;
    if let Err(e) = sync_all(&db, &cfg) {
        bail!("logged in, but the first sync failed: {e:#}");
    }
    let calendars = db.calendars()?;
    println!("Found {} calendar(s):", calendars.len());
    for c in &calendars {
        println!("  [{}] {}  ({})", if c.selected { "x" } else { " " }, c.summary, c.id);
    }
    println!("\nChoose which are shown with `{APP_NAME} calendars --select <id>` / `--deselect <id>`.");
    Ok(())
}

/// Incremental sync of the calendar list and of the events of every *selected* calendar
/// (deselected calendars are skipped to save quota; they catch up when selected again).
///
/// Returns `Ok(false)` when not configured (no `client_id`) or not logged in (no refresh token),
/// `Ok(true)` after a sync. Blocking; see the module docs for async callers.
pub fn sync_all(db: &Db, cfg: &Config) -> Result<bool> {
    let Some(client) = oauth::Client::from_config(cfg) else { return Ok(false) };
    let Some(refresh_token) = secret::load_refresh_token()? else { return Ok(false) };
    let access_token = oauth::refresh_access_token(client, &refresh_token)?;
    sync_with(db, &api::GoogleBackend::new(access_token))?;
    Ok(true)
}

/// Backend-agnostic sync. A failing calendar does not stop the others; failures are reported
/// together at the end.
fn sync_with(db: &Db, backend: &dyn CalendarBackend) -> Result<()> {
    let remote = backend.list_calendars()?;
    for c in &remote {
        db.upsert_calendar(&c.id, &c.summary, c.color.as_deref(), &c.default_reminders)?;
    }
    let ids: Vec<String> = remote.into_iter().map(|c| c.id).collect();
    db.retain_calendars(&ids)?;

    let failures: Vec<String> = db
        .calendars()?
        .iter()
        .filter(|c| c.selected)
        .filter_map(|c| sync_calendar(db, backend, c).err().map(|e| format!("{}: {e:#}", c.summary)))
        .collect();
    if !failures.is_empty() {
        bail!("sync failed for {} calendar(s): {}", failures.len(), failures.join("; "));
    }
    Ok(())
}

fn sync_calendar(db: &Db, backend: &dyn CalendarBackend, cal: &Calendar) -> Result<()> {
    if let Some(token) = cal.sync_token.as_deref() {
        if run_sync(db, backend, &cal.id, Some(token))? {
            return Ok(());
        }
        // 410 Gone: the token expired. Forget it first so a crash below still ends in a full sync.
        db.set_sync_token(&cal.id, None)?;
    }
    // Full sync: start from an empty cache so deletions missed while out of sync cannot linger.
    db.clear_events(&cal.id)?;
    if !run_sync(db, backend, &cal.id, None)? {
        bail!("Google answered 410 Gone to a full sync");
    }
    Ok(())
}

/// Returns `false` on 410 Gone. The new sync token is stored only after the final page.
fn run_sync(db: &Db, backend: &dyn CalendarBackend, calendar_id: &str, token: Option<&str>) -> Result<bool> {
    let outcome = backend.sync_events(calendar_id, token, &mut |changes| {
        for change in changes {
            match change {
                EventChange::Upsert(e) => db.upsert_event(&e)?,
                EventChange::Delete(id) => db.delete_event(calendar_id, &id)?,
            }
        }
        Ok(())
    })?;
    match outcome {
        SyncOutcome::Gone => Ok(false),
        SyncOutcome::Complete { next_sync_token } => {
            if let Some(t) = next_sync_token {
                db.set_sync_token(calendar_id, Some(&t))?;
            }
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use anyhow::anyhow;

    use super::*;

    enum Script {
        Pages(Vec<Vec<EventChange>>, &'static str),
        /// Deliver one page, then fail mid-pagination.
        FailAfter(Vec<EventChange>),
        Gone,
    }

    #[derive(Default)]
    struct Fake {
        calendars: RefCell<Vec<RemoteCalendar>>,
        scripts: RefCell<VecDeque<Script>>,
        /// (calendar id, token passed) per `sync_events` call.
        calls: RefCell<Vec<(String, Option<String>)>>,
    }

    impl CalendarBackend for Fake {
        fn list_calendars(&self) -> Result<Vec<RemoteCalendar>> {
            Ok(self.calendars.borrow().clone())
        }

        fn sync_events(
            &self,
            calendar_id: &str,
            sync_token: Option<&str>,
            on_page: &mut dyn FnMut(Vec<EventChange>) -> Result<()>,
        ) -> Result<SyncOutcome> {
            self.calls.borrow_mut().push((calendar_id.to_owned(), sync_token.map(str::to_owned)));
            match self.scripts.borrow_mut().pop_front().expect("unscripted sync_events call") {
                Script::Gone => Ok(SyncOutcome::Gone),
                Script::FailAfter(page) => {
                    on_page(page)?;
                    Err(anyhow!("network down"))
                }
                Script::Pages(pages, token) => {
                    for p in pages {
                        on_page(p)?;
                    }
                    Ok(SyncOutcome::Complete { next_sync_token: Some(token.to_owned()) })
                }
            }
        }
    }

    fn cal(id: &str) -> RemoteCalendar {
        RemoteCalendar { id: id.into(), summary: format!("Cal {id}"), color: None, default_reminders: vec![] }
    }

    fn ev(cal: &str, id: &str, start: i64) -> EventChange {
        EventChange::Upsert(Event {
            calendar_id: cal.into(),
            id: id.into(),
            summary: format!("event {id}"),
            description: String::new(),
            location: None,
            all_day: false,
            start_ts: start,
            end_ts: start + 3600,
            meet_link: None,
            html_link: None,
            use_default_reminders: true,
            reminders: vec![],
        })
    }

    fn ids(db: &Db) -> Vec<String> {
        let mut v: Vec<_> = db.events_between(0, i64::MAX).unwrap().into_iter().map(|e| e.id).collect();
        v.sort();
        v
    }

    fn token(db: &Db, id: &str) -> Option<String> {
        db.calendars().unwrap().into_iter().find(|c| c.id == id).unwrap().sync_token
    }

    #[test]
    fn full_then_incremental_sync() {
        let db = Db::open_in_memory().unwrap();
        let fake = Fake::default();
        fake.calendars.borrow_mut().push(cal("a"));
        fake.scripts.borrow_mut().extend([
            Script::Pages(vec![vec![ev("a", "1", 1000)], vec![ev("a", "2", 2000)]], "T1"),
            Script::Pages(vec![vec![EventChange::Delete("1".into()), ev("a", "3", 3000)]], "T2"),
        ]);

        sync_with(&db, &fake).unwrap();
        assert_eq!(ids(&db), ["1", "2"]);
        assert_eq!(token(&db, "a").as_deref(), Some("T1"));

        sync_with(&db, &fake).unwrap();
        assert_eq!(ids(&db), ["2", "3"]);
        assert_eq!(token(&db, "a").as_deref(), Some("T2"));
        assert_eq!(
            *fake.calls.borrow(),
            [("a".to_owned(), None), ("a".to_owned(), Some("T1".to_owned()))]
        );
    }

    #[test]
    fn gone_clears_events_and_token_then_full_resyncs() {
        let db = Db::open_in_memory().unwrap();
        let fake = Fake::default();
        fake.calendars.borrow_mut().push(cal("a"));
        fake.scripts.borrow_mut().extend([
            Script::Pages(vec![vec![ev("a", "stale", 1000), ev("a", "keep", 2000)]], "OLD"),
            Script::Gone,
            Script::Pages(vec![vec![ev("a", "keep", 2000), ev("a", "new", 3000)]], "NEW"),
        ]);

        sync_with(&db, &fake).unwrap();
        assert_eq!(ids(&db), ["keep", "stale"]);

        sync_with(&db, &fake).unwrap();
        assert_eq!(ids(&db), ["keep", "new"], "stale event must not survive a 410 resync");
        assert_eq!(token(&db, "a").as_deref(), Some("NEW"));
        assert_eq!(
            *fake.calls.borrow(),
            [
                ("a".to_owned(), None),
                ("a".to_owned(), Some("OLD".to_owned())),
                ("a".to_owned(), None),
            ]
        );
    }

    #[test]
    fn token_is_not_stored_when_pagination_fails() {
        let db = Db::open_in_memory().unwrap();
        let fake = Fake::default();
        fake.calendars.borrow_mut().push(cal("a"));
        fake.scripts.borrow_mut().push_back(Script::FailAfter(vec![ev("a", "1", 1000)]));

        let err = sync_with(&db, &fake).unwrap_err().to_string();
        assert!(err.contains("network down") && err.contains("Cal a"), "{err}");
        assert_eq!(token(&db, "a"), None);
        assert_eq!(ids(&db), ["1"], "pages already delivered are persisted");
    }

    #[test]
    fn gone_on_full_sync_is_an_error() {
        let db = Db::open_in_memory().unwrap();
        let fake = Fake::default();
        fake.calendars.borrow_mut().push(cal("a"));
        fake.scripts.borrow_mut().push_back(Script::Gone);
        assert!(sync_with(&db, &fake).unwrap_err().to_string().contains("410"));
    }

    #[test]
    fn only_selected_calendars_sync_and_removed_ones_are_dropped() {
        let db = Db::open_in_memory().unwrap();
        let fake = Fake::default();
        fake.calendars.borrow_mut().extend([cal("a"), cal("b"), cal("gone")]);
        db.upsert_calendar("a", "Cal a", None, &[]).unwrap();
        db.upsert_calendar("b", "Cal b", None, &[]).unwrap();
        db.set_calendar_selected("b", false).unwrap();
        fake.scripts.borrow_mut().push_back(Script::Pages(vec![vec![ev("a", "1", 1000)]], "TA"));
        // "gone" is selected by default and syncs too; it is removed in the second run.
        fake.scripts.borrow_mut().push_back(Script::Pages(vec![vec![ev("gone", "g", 1000)]], "TG"));

        sync_with(&db, &fake).unwrap();
        let synced: Vec<_> = fake.calls.borrow().iter().map(|(c, _)| c.clone()).collect();
        assert!(synced.contains(&"a".to_owned()) && synced.contains(&"gone".to_owned()));
        assert!(!synced.contains(&"b".to_owned()), "deselected calendar must not be synced");

        fake.calendars.borrow_mut().retain(|c| c.id != "gone");
        fake.scripts.borrow_mut().push_back(Script::Pages(vec![], "TA2"));
        sync_with(&db, &fake).unwrap();
        let remaining: Vec<_> = db.calendars().unwrap().into_iter().map(|c| c.id).collect();
        assert_eq!(remaining, ["a", "b"]);
        assert_eq!(ids(&db), ["1"], "events of a removed calendar are gone");
    }

    #[test]
    fn one_failing_calendar_does_not_block_the_others() {
        let db = Db::open_in_memory().unwrap();
        let fake = Fake::default();
        fake.calendars.borrow_mut().extend([cal("a"), cal("b")]);
        // Calendars sync in summary order: a fails, b succeeds.
        fake.scripts.borrow_mut().push_back(Script::FailAfter(vec![]));
        fake.scripts.borrow_mut().push_back(Script::Pages(vec![vec![ev("b", "1", 1000)]], "TB"));

        assert!(sync_with(&db, &fake).is_err());
        assert_eq!(token(&db, "a"), None);
        assert_eq!(token(&db, "b").as_deref(), Some("TB"));
    }

    #[test]
    fn sync_all_is_false_without_client_id() {
        let db = Db::open_in_memory().unwrap();
        assert!(!sync_all(&db, &Config::default()).unwrap());
    }
}
