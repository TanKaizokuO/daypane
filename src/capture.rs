//! Quick-capture parser. CONTRACT STUB — replaced by the capture implementation.
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

use crate::db::{NewTask, Priority};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capture {
    pub title: String,
    pub due_date: Option<NaiveDate>,
    pub due_time: Option<NaiveTime>,
    pub priority: Priority,
    pub tags: Vec<String>,
    pub links: Vec<String>,
}

impl Capture {
    /// Human-readable chip text for the live preview, e.g. "Fri 14 Nov 17:00 · #home · !high".
    pub fn preview(&self) -> String { String::new() }
    pub fn into_new_task(self) -> NewTask { NewTask::default() }
}

/// Parse capture syntax relative to local `now`.
pub fn parse(_input: &str, _now: NaiveDateTime) -> Capture { Capture::default() }
