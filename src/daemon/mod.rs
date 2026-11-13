//! `daemon`: Google sync, reminder scheduling, Waybar JSON output.
//!
//! One tokio runtime; the main task owns a DB connection and runs a ~15 s wall-clock tick
//! (reminders + Waybar write) plus a debounced DB-file watcher. Google sync runs in its own task
//! on `spawn_blocking` with a separate connection. SIGTERM/SIGINT end the loop.

mod notify;
mod reminders;
mod waybar;

use std::path::PathBuf;
use std::time::Duration;

use ::notify as notify_crate;
use anyhow::{Context, Result};
use chrono::{Local, Utc};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::time::{interval, sleep_until, Instant, MissedTickBehavior};

use crate::app::APP_NAME;
use crate::config::Config;
use crate::db::Db;
use crate::{google, paths};

const MIN_SYNC_INTERVAL_SECS: u64 = 30;
const DB_DEBOUNCE: Duration = Duration::from_millis(250);
const MAINTENANCE_EVERY: Duration = Duration::from_secs(3600);
const FIRED_RETENTION_SECS: i64 = 2 * 24 * 3600;

fn log(msg: impl std::fmt::Display) {
    eprintln!("{APP_NAME} daemon: {msg}");
}

pub fn run() -> Result<()> {
    let cfg = Config::load()?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting tokio runtime")?
        .block_on(serve(cfg))
}

async fn serve(cfg: Config) -> Result<()> {
    let db = Db::open_default()?;
    let waybar_path = paths::waybar_file()?;

    maintenance(&db);
    let mut last_maintenance = Instant::now();

    let notifier = notify::spawn(cfg.reminders.snooze_minutes);
    let sync_task = tokio::spawn(sync_loop(cfg.clone()));

    let (db_tx, mut db_rx) = unbounded_channel::<()>();
    let _watcher = match watch_db(db_tx) {
        Ok(w) => Some(w),
        Err(e) => {
            log(format_args!("cannot watch the database ({e:#}); relying on the periodic tick"));
            None
        }
    };

    let mut sigterm = signal(SignalKind::terminate()).context("installing SIGTERM handler")?;
    let mut sigint = signal(SignalKind::interrupt()).context("installing SIGINT handler")?;

    let mut tick = interval(Duration::from_secs(reminders::TICK_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut last_tick_ts = Utc::now().timestamp();
    let mut last_waybar = String::new();
    let mut debounce: Option<Instant> = None;

    loop {
        let deadline = debounce;
        tokio::select! {
            _ = tick.tick() => {
                let now_ts = Utc::now().timestamp();
                if last_maintenance.elapsed() >= MAINTENANCE_EVERY {
                    maintenance(&db);
                    last_maintenance = Instant::now();
                }
                let earliest = reminders::window_start(now_ts, last_tick_ts, reminders::GRACE_SECS);
                last_tick_ts = now_ts;
                if let Err(e) = fire_due_reminders(&db, &cfg, &notifier, earliest, now_ts) {
                    log(format_args!("reminders: {e:#}"));
                }
                refresh_waybar(&db, &cfg, &waybar_path, &mut last_waybar);
            }
            Some(()) = db_rx.recv() => {
                debounce.get_or_insert_with(|| Instant::now() + DB_DEBOUNCE);
            }
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                debounce = None;
                refresh_waybar(&db, &cfg, &waybar_path, &mut last_waybar);
            }
            _ = sigterm.recv() => { log("SIGTERM, shutting down"); break; }
            _ = sigint.recv() => { log("SIGINT, shutting down"); break; }
        }
    }

    sync_task.abort();
    Ok(())
}

fn maintenance(db: &Db) {
    match db.purge_trash() {
        Ok(0) => {}
        Ok(n) => log(format_args!("purged {n} trashed task(s)")),
        Err(e) => log(format_args!("purge trash: {e:#}")),
    }
    if let Err(e) = db.prune_fired(Utc::now().timestamp() - FIRED_RETENTION_SECS) {
        log(format_args!("prune fired reminders: {e:#}"));
    }
}

fn fire_due_reminders(
    db: &Db,
    cfg: &Config,
    notifier: &UnboundedSender<notify::Notification>,
    earliest_ts: i64,
    now_ts: i64,
) -> Result<()> {
    let events = db.events_between(now_ts - reminders::GRACE_SECS, now_ts + reminders::HORIZON_SECS)?;
    let calendars = db.calendars()?;
    let today = Local::now().date_naive();
    let tasks = db.timed_tasks_from(today.pred_opt().unwrap_or(today))?;

    let due = reminders::due_reminders(
        &events,
        &calendars,
        &tasks,
        cfg.reminders.task_offset_minutes,
        earliest_ts,
        now_ts,
        &Local,
    );
    for r in due {
        if db.mark_fired(&r.key, now_ts)? {
            let _ = notifier.send(notify::Notification { title: r.title, body: r.body });
        }
    }
    Ok(())
}

/// Re-render, write atomically, and nudge Waybar only when the output changed.
fn refresh_waybar(db: &Db, cfg: &Config, path: &std::path::Path, last: &mut String) {
    let rendered = match waybar::collect(db, &Local::now()) {
        Ok(v) => v.to_string(),
        Err(e) => return log(format_args!("waybar render: {e:#}")),
    };
    if let Err(e) = waybar::write_atomic(path, &rendered) {
        return log(format_args!("waybar write: {e:#}"));
    }
    if *last != rendered {
        *last = rendered;
        let n = cfg.waybar.signal;
        tokio::task::spawn_blocking(move || waybar::signal(n));
    }
}

/// Watch the DB directory and report changes to the DB file or its `-wal` (not `-shm`).
fn watch_db(tx: UnboundedSender<()>) -> Result<notify_crate::RecommendedWatcher> {
    use notify_crate::{EventKind, RecursiveMode, Watcher};

    let db_path = paths::db_file()?;
    let dir: PathBuf = db_path.parent().context("db path has no parent")?.to_owned();
    let db_name = db_path.file_name().context("db path has no file name")?.to_string_lossy().into_owned();

    let mut watcher = notify_crate::recommended_watcher(move |res: notify_crate::Result<notify_crate::Event>| {
        let Ok(ev) = res else { return };
        if !matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)) {
            return;
        }
        let relevant = ev.paths.iter().any(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy())
                .is_some_and(|n| n.starts_with(&db_name) && !n.ends_with("-shm"))
        });
        if relevant {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Periodic Google sync. Errors are logged and never stop the daemon; "not configured" is
/// reported once.
async fn sync_loop(startup_cfg: Config) {
    let secs = startup_cfg.google.sync_interval_seconds.max(MIN_SYNC_INTERVAL_SECS);
    let mut tick = interval(Duration::from_secs(secs));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut told_unconfigured = false;

    loop {
        tick.tick().await;
        let fallback = startup_cfg.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool> {
            // Re-read the config so a later `login` / client_id edit is picked up without restart.
            let cfg = Config::load().unwrap_or(fallback);
            let db = Db::open_default()?;
            google::sync_all(&db, &cfg)
        })
        .await;
        match result {
            Ok(Ok(true)) => told_unconfigured = false,
            Ok(Ok(false)) => {
                if !told_unconfigured {
                    log("Google Calendar not configured or not logged in; sync skipped");
                    told_unconfigured = true;
                }
            }
            Ok(Err(e)) => log(format_args!("google sync failed: {e:#}")),
            Err(e) => log(format_args!("google sync task failed: {e}")),
        }
    }
}
