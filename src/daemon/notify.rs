//! Desktop notifications over `org.freedesktop.Notifications` (zbus, tokio).
//!
//! A single task owns the D-Bus connection. The scheduler sends [`Notification`]s over a channel;
//! the task shows them, listens for `ActionInvoked` / `NotificationClosed`, and implements:
//! `open` → spawn `<exe> popup`, `snooze` → re-show after the configured minutes (in memory only,
//! lost on restart). Without a notification server everything is logged and dropped.

use std::collections::HashMap;
use std::future::poll_fn;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use zbus::export::futures_core::Stream;
use zbus::message::Type as MessageType;
use zbus::{Connection, MatchRule, Message, MessageStream, Variant};

use crate::app::{APP_ID, APP_NAME};

const DEST: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub title: String,
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    Snooze,
}

impl Action {
    fn parse(key: &str) -> Option<Self> {
        match key {
            "open" => Some(Self::Open),
            "snooze" => Some(Self::Snooze),
            _ => None,
        }
    }
}

/// In-memory snooze queue, ordered by insertion; `take_due` is pure over `now_ts`.
#[derive(Default)]
struct Snoozes(Vec<(i64, Notification)>);

impl Snoozes {
    fn add(&mut self, due_ts: i64, n: Notification) {
        self.0.push((due_ts, n));
    }

    fn take_due(&mut self, now_ts: i64) -> Vec<Notification> {
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.0)
            .into_iter()
            .partition(|(ts, _)| *ts <= now_ts);
        self.0 = rest;
        due.into_iter().map(|(_, n)| n).collect()
    }
}

fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Start the notification task. Sending never fails while the daemon runs; if there is no
/// notification server the task logs each failure and carries on.
pub fn spawn(snooze_minutes: i64) -> UnboundedSender<Notification> {
    let (tx, rx) = unbounded_channel();
    tokio::spawn(run(rx, snooze_minutes));
    tx
}

async fn run(mut rx: UnboundedReceiver<Notification>, snooze_minutes: i64) {
    let conn = match Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{APP_NAME} daemon: no session bus ({e}); notifications disabled");
            while rx.recv().await.is_some() {}
            return;
        }
    };

    let mut actions = signal_stream(&conn, "ActionInvoked").await;
    let mut closed = signal_stream(&conn, "NotificationClosed").await;
    let mut supports_actions: Option<bool> = None;
    let mut shown: HashMap<u32, Notification> = HashMap::new();
    let mut snoozed = Snoozes::default();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        let mut to_show: Vec<Notification> = Vec::new();
        tokio::select! {
            n = rx.recv() => match n {
                Some(n) => to_show.push(n),
                None => return,
            },
            msg = next(&mut actions) => {
                if let Some((id, key)) = msg.and_then(|m| m.body().deserialize::<(u32, String)>().ok()) {
                    match (Action::parse(&key), shown.get(&id)) {
                        (Some(Action::Open), Some(_)) => open_popup(),
                        (Some(Action::Snooze), Some(_)) => {
                            if let Some(n) = shown.remove(&id) {
                                snoozed.add(now_ts() + snooze_minutes.max(1) * 60, n);
                            }
                        }
                        _ => {}
                    }
                }
            }
            msg = next(&mut closed) => {
                if let Some((id, _reason)) = msg.and_then(|m| m.body().deserialize::<(u32, u32)>().ok()) {
                    shown.remove(&id);
                }
            }
            _ = tick.tick() => to_show.extend(snoozed.take_due(now_ts())),
        }

        for n in to_show {
            if supports_actions.is_none() {
                supports_actions = capabilities(&conn)
                    .await
                    .map(|c| c.iter().any(|c| c == "actions"))
                    .ok();
            }
            match notify(&conn, &n, supports_actions.unwrap_or(false)).await {
                Ok(id) => {
                    shown.insert(id, n);
                }
                Err(e) => eprintln!(
                    "{APP_NAME} daemon: notification failed: {e:#} (title: {})",
                    n.title
                ),
            }
        }
    }
}

async fn signal_stream(conn: &Connection, member: &'static str) -> Option<MessageStream> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .interface(DEST)
        .member(member)
        .path(PATH)
        .build();
    let stream = match rule {
        Ok(rule) => MessageStream::for_match_rule(rule, conn, None).await,
        Err(e) => Err(e),
    };
    match stream {
        Ok(s) => Some(s),
        Err(e) => {
            eprintln!("{APP_NAME} daemon: cannot subscribe to {member}: {e}");
            None
        }
    }
}

/// Next message of an optional stream; pends forever when there is no stream or it ended.
async fn next(stream: &mut Option<MessageStream>) -> Option<Message> {
    let Some(s) = stream.as_mut() else {
        return std::future::pending().await;
    };
    match poll_fn(|cx| Pin::new(&mut *s).poll_next(cx)).await {
        Some(Ok(m)) => Some(m),
        Some(Err(_)) => None,
        None => {
            *stream = None;
            None
        }
    }
}

async fn capabilities(conn: &Connection) -> Result<Vec<String>> {
    let reply = conn
        .call_method(Some(DEST), PATH, Some(DEST), "GetCapabilities", &())
        .await
        .context("GetCapabilities (is a notification server running?)")?;
    Ok(reply.body().deserialize::<Vec<String>>()?)
}

async fn notify(conn: &Connection, n: &Notification, with_actions: bool) -> Result<u32> {
    let actions: Vec<&str> = if with_actions {
        vec!["open", "Open", "snooze", "Snooze"]
    } else {
        vec![]
    };
    let hints: HashMap<&str, Variant> = HashMap::from([("desktop-entry", Variant::from(APP_ID))]);
    let reply = conn
        .call_method(
            Some(DEST),
            PATH,
            Some(DEST),
            "Notify",
            &(
                APP_NAME,
                0u32,
                "",
                n.title.as_str(),
                n.body.as_str(),
                actions,
                hints,
                -1i32,
            ),
        )
        .await
        .context("Notify")?;
    Ok(reply.body().deserialize::<u32>()?)
}

fn open_popup() {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => return eprintln!("{APP_NAME} daemon: cannot locate own executable: {e}"),
    };
    match tokio::process::Command::new(exe).arg("popup").spawn() {
        Ok(mut child) => {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(e) => eprintln!("{APP_NAME} daemon: cannot start popup: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(t: &str) -> Notification {
        Notification {
            title: t.into(),
            body: String::new(),
        }
    }

    #[test]
    fn action_keys() {
        assert_eq!(Action::parse("open"), Some(Action::Open));
        assert_eq!(Action::parse("snooze"), Some(Action::Snooze));
        assert_eq!(Action::parse("default"), None);
    }

    #[test]
    fn snoozes_release_only_when_due_and_only_once() {
        let mut s = Snoozes::default();
        s.add(100, n("a"));
        s.add(200, n("b"));
        s.add(100, n("c"));
        assert!(s.take_due(99).is_empty());
        let due: Vec<_> = s.take_due(100).into_iter().map(|x| x.title).collect();
        assert_eq!(due, ["a", "c"]);
        assert!(s.take_due(150).is_empty());
        assert_eq!(s.take_due(1000).len(), 1);
        assert!(s.take_due(2000).is_empty());
    }
}
