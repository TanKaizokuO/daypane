//! Quick-capture parser: `"pay rent fri 5pm #home !high https://…"` → a structured task.
//!
//! Everything is parsed locally, case-insensitively, anywhere in the text. Recognised tokens are
//! removed from the title; the remaining words are re-joined with single spaces.
//!
//! # Grammar
//!
//! * `#tag` → tag (lowercased, de-duplicated). `!high|!med|!low` (also `!h|!m|!l`, `!medium`) → priority
//!   (the last one wins). Bare `http://` / `https://` tokens → links (trailing punctuation dropped).
//! * Dates (the first one found wins, later ones stay in the title):
//!   `today`, `tomorrow`/`tmr`/`tmrw`, `tonight` (today, 20:00 unless a time is given),
//!   weekdays (`mon`, `monday`, `tues`, …), `next <weekday>`, `in N day(s)|week(s)|month(s)` (`in a week`),
//!   `nov 20` / `20 nov` / `november 20th` / `nov 20 2027`, and ISO `2026-11-20`.
//!   A month name only counts when a day number is next to it, so a lone `may` stays in the title.
//!   A month/day without a year that already passed this year rolls to the next year.
//! * Weekday rule: a bare weekday is the nearest day **strictly after today**. The only exception is
//!   when today *is* that weekday and a time later than now is also given (`fri 5pm` on a Friday
//!   morning means today). `next <weekday>` never means today.
//! * Times (the first one found wins): `5pm`, `5 pm`, `5:30pm`, `17:00`, `noon`, `midnight`, and the
//!   `at` forms `at 9` (24-hour clock, so `at 9` = 09:00), `at 5pm`, `at noon`.
//! * A time without a date means today if it is still ahead of `now`, otherwise tomorrow.
//! * The title may end up empty (`"tomorrow"`); callers decide whether that is an error.

use chrono::{Datelike, Duration, Months, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

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
    /// Human-readable chip text for the live preview, e.g. `Fri 14 Nov · 17:00 · #home · !high · 1 link`.
    /// Empty pieces are omitted; an undated capture shows `Inbox` in the date slot.
    pub fn preview(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match self.due_date {
            Some(d) => parts.push(d.format("%a %-d %b").to_string()),
            None => parts.push("Inbox".to_string()),
        }
        if let Some(t) = self.due_time {
            parts.push(t.format("%H:%M").to_string());
        }
        parts.extend(self.tags.iter().map(|t| format!("#{t}")));
        match self.priority {
            Priority::None => {}
            Priority::Low => parts.push("!low".into()),
            Priority::Med => parts.push("!med".into()),
            Priority::High => parts.push("!high".into()),
        }
        match self.links.len() {
            0 => {}
            1 => parts.push("1 link".into()),
            n => parts.push(format!("{n} links")),
        }
        parts.join(" · ")
    }

    pub fn into_new_task(self) -> NewTask {
        NewTask {
            title: self.title,
            due_date: self.due_date,
            due_time: self.due_time,
            notes: String::new(),
            priority: self.priority,
            tags: self.tags,
            links: self.links,
        }
    }
}

/// Parse capture syntax relative to local `now`.
pub fn parse(input: &str, now: NaiveDateTime) -> Capture {
    let today = now.date();
    let mut toks: Vec<Tok> = input.split_whitespace().map(Tok::new).collect();
    let mut cap = Capture::default();

    // Pass 1: tokens that are unambiguous on their own.
    for t in toks.iter_mut() {
        if let Some(url) = as_link(t.raw) {
            cap.links.push(url);
        } else if let Some(tag) = as_tag(t.raw) {
            if !cap.tags.contains(&tag) {
                cap.tags.push(tag);
            }
        } else if let Some(p) = as_priority(&t.norm) {
            cap.priority = p;
        } else {
            continue;
        }
        t.used = true;
    }

    // Pass 2: first date, then first time, scanning left to right.
    let mut date = None;
    for i in 0..toks.len() {
        if toks[i].used {
            continue;
        }
        if let Some((d, len)) = try_date(&toks, i, today) {
            mark(&mut toks, i, len);
            date = Some(d);
            break;
        }
    }
    let mut time = None;
    for i in 0..toks.len() {
        if toks[i].used {
            continue;
        }
        if let Some((t, len)) = try_time(&toks, i) {
            mark(&mut toks, i, len);
            time = Some(t);
            break;
        }
    }

    // Resolve.
    match date {
        Some(DateSpec::On(d)) => cap.due_date = Some(d),
        Some(DateSpec::Tonight) => {
            cap.due_date = Some(today);
            time = time.or_else(|| NaiveTime::from_hms_opt(20, 0, 0));
        }
        Some(DateSpec::Weekday { wd, strict }) => {
            let delta = (wd.num_days_from_monday() as i64 - today.weekday().num_days_from_monday() as i64)
                .rem_euclid(7);
            let later_today = !strict && time.is_some_and(|t| t > now.time());
            let days = if delta == 0 && !later_today { 7 } else { delta };
            cap.due_date = Some(today + Duration::days(days));
        }
        None => {
            if let Some(t) = time {
                cap.due_date = Some(if t > now.time() { today } else { today + Duration::days(1) });
            }
        }
    }
    cap.due_time = if cap.due_date.is_some() { time } else { None };
    cap.title = toks.iter().filter(|t| !t.used).map(|t| t.raw).collect::<Vec<_>>().join(" ");
    cap
}

// ─── Tokens ─────────────────────────────────────────────────────────────────

struct Tok<'a> {
    raw: &'a str,
    /// Lowercased, surrounding punctuation trimmed; what the NL matchers look at.
    norm: String,
    used: bool,
}

impl<'a> Tok<'a> {
    fn new(raw: &'a str) -> Self {
        let norm = raw
            .trim_start_matches('(')
            .trim_end_matches([',', '.', ';', ':', '!', '?', ')'])
            .to_lowercase();
        Self { raw, norm, used: false }
    }
}

fn mark(toks: &mut [Tok], i: usize, len: usize) {
    for t in &mut toks[i..i + len] {
        t.used = true;
    }
}

/// Normalised text of the token at `i`, if it exists and is still unclaimed.
fn word<'a>(toks: &'a [Tok], i: usize) -> Option<&'a str> {
    toks.get(i).filter(|t| !t.used).map(|t| t.norm.as_str())
}

fn as_link(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let scheme = ["https://", "http://"].into_iter().find(|s| lower.starts_with(s))?;
    let mut url = raw.trim_end_matches(['.', ',', ';', ':', '!', '?']);
    if !url.contains('(') {
        url = url.trim_end_matches(')');
    }
    (url.len() > scheme.len()).then(|| url.to_string())
}

fn as_tag(raw: &str) -> Option<String> {
    let body = raw.strip_prefix('#')?.trim_end_matches([',', '.', ';', ':', '!', '?']);
    (!body.is_empty() && body.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-'))
        .then(|| body.to_lowercase())
}

fn as_priority(norm: &str) -> Option<Priority> {
    Some(match norm.strip_prefix('!')? {
        "high" | "h" => Priority::High,
        "med" | "medium" | "m" => Priority::Med,
        "low" | "l" => Priority::Low,
        _ => return None,
    })
}

// ─── Dates ──────────────────────────────────────────────────────────────────

enum DateSpec {
    On(NaiveDate),
    Tonight,
    Weekday { wd: Weekday, strict: bool },
}

fn weekday(w: &str) -> Option<Weekday> {
    Some(match w {
        "mon" | "monday" => Weekday::Mon,
        "tue" | "tues" | "tuesday" => Weekday::Tue,
        "wed" | "weds" | "wednesday" => Weekday::Wed,
        "thu" | "thur" | "thurs" | "thursday" => Weekday::Thu,
        "fri" | "friday" => Weekday::Fri,
        "sat" | "saturday" => Weekday::Sat,
        "sun" | "sunday" => Weekday::Sun,
        _ => return None,
    })
}

fn month(w: &str) -> Option<u32> {
    Some(match w {
        "jan" | "january" => 1,
        "feb" | "february" => 2,
        "mar" | "march" => 3,
        "apr" | "april" => 4,
        "may" => 5,
        "jun" | "june" => 6,
        "jul" | "july" => 7,
        "aug" | "august" => 8,
        "sep" | "sept" | "september" => 9,
        "oct" | "october" => 10,
        "nov" | "november" => 11,
        "dec" | "december" => 12,
        _ => return None,
    })
}

/// `20`, `3rd`, `31st` → 1..=31.
fn day_num(w: &str) -> Option<u32> {
    let digits = ["st", "nd", "rd", "th"]
        .iter()
        .find_map(|s| w.strip_suffix(s))
        .unwrap_or(w);
    if digits.is_empty() || digits.len() > 2 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|d| (1..=31).contains(d))
}

fn explicit_year(w: Option<&str>) -> Option<i32> {
    let w = w?;
    (w.len() == 4 && w.bytes().all(|b| b.is_ascii_digit()))
        .then(|| w.parse().ok())
        .flatten()
        .filter(|y| (1970..=2200).contains(y))
}

/// `month`/`day` → the first matching date on or after `today` (or exactly the given year).
fn month_day(m: u32, d: u32, year: Option<i32>, today: NaiveDate) -> Option<NaiveDate> {
    match year {
        Some(y) => NaiveDate::from_ymd_opt(y, m, d),
        // Up to 8 years ahead so Feb 29 finds the next leap year.
        None => (today.year()..today.year() + 8)
            .filter_map(|y| NaiveDate::from_ymd_opt(y, m, d))
            .find(|date| *date >= today),
    }
}

fn is_meridiem(w: Option<&str>) -> bool {
    matches!(w, Some("am" | "pm"))
}

fn try_date(toks: &[Tok], i: usize, today: NaiveDate) -> Option<(DateSpec, usize)> {
    let w = word(toks, i)?;
    let w1 = word(toks, i + 1);
    let w2 = word(toks, i + 2);
    match w {
        "today" => return Some((DateSpec::On(today), 1)),
        "tomorrow" | "tmr" | "tmrw" => return Some((DateSpec::On(today + Duration::days(1)), 1)),
        "tonight" => return Some((DateSpec::Tonight, 1)),
        "next" => {
            if let Some(wd) = w1.and_then(weekday) {
                return Some((DateSpec::Weekday { wd, strict: true }, 2));
            }
        }
        "in" => {
            let n: Option<u32> = match w1 {
                Some("a" | "an") => Some(1),
                Some(n) if !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()) => n.parse().ok(),
                _ => None,
            };
            if let (Some(n), Some(unit)) = (n, w2) {
                let date = match unit {
                    "day" | "days" => today.checked_add_signed(Duration::days(n.into())),
                    "week" | "weeks" => today.checked_add_signed(Duration::weeks(n.into())),
                    "month" | "months" => today.checked_add_months(Months::new(n)),
                    _ => None,
                };
                if let Some(d) = date {
                    return Some((DateSpec::On(d), 3));
                }
            }
        }
        _ => {}
    }
    if let Some(wd) = weekday(w) {
        return Some((DateSpec::Weekday { wd, strict: false }, 1));
    }
    if w.len() == 10 {
        if let Ok(d) = NaiveDate::parse_from_str(w, "%Y-%m-%d") {
            return Some((DateSpec::On(d), 1));
        }
    }
    // `nov 20`, `november 20th`, `nov 20 2027`
    if let Some(m) = month(w) {
        if let Some(d) = w1.and_then(day_num) {
            if !is_meridiem(w2) {
                let year = explicit_year(w2);
                let len = if year.is_some() { 3 } else { 2 };
                if let Some(date) = month_day(m, d, year, today) {
                    return Some((DateSpec::On(date), len));
                }
            }
        }
    }
    // `20 nov`, `20th november 2027`
    if let (Some(d), Some(m)) = (day_num(w), w1.and_then(month)) {
        let year = explicit_year(w2);
        let len = if year.is_some() { 3 } else { 2 };
        if let Some(date) = month_day(m, d, year, today) {
            return Some((DateSpec::On(date), len));
        }
    }
    None
}

// ─── Times ──────────────────────────────────────────────────────────────────

/// `H` or `H:MM` → (hour, minute, had_colon).
fn hour_min(s: &str) -> Option<(u32, u32, bool)> {
    let (h, m, colon) = match s.split_once(':') {
        Some((h, m)) if m.len() == 2 => (h, m, true),
        Some(_) => return None,
        None => (s, "0", false),
    };
    let all_digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
    if !all_digits(h) || h.len() > 2 || !all_digits(m) {
        return None;
    }
    Some((h.parse().ok()?, m.parse().ok()?, colon))
}

fn clock12(h: u32, m: u32, pm: bool) -> Option<NaiveTime> {
    if !(1..=12).contains(&h) {
        return None;
    }
    NaiveTime::from_hms_opt(h % 12 + if pm { 12 } else { 0 }, m, 0)
}

/// Single-token times: `5pm`, `5:30pm`, `17:00`, `noon`, `midnight`.
fn time_token(w: &str) -> Option<NaiveTime> {
    match w {
        "noon" => return NaiveTime::from_hms_opt(12, 0, 0),
        "midnight" => return NaiveTime::from_hms_opt(0, 0, 0),
        _ => {}
    }
    for (suffix, pm) in [("am", false), ("pm", true)] {
        if let Some(body) = w.strip_suffix(suffix) {
            let (h, m, _) = hour_min(body)?;
            return clock12(h, m, pm);
        }
    }
    let (h, m, colon) = hour_min(w)?;
    if !colon {
        return None;
    }
    NaiveTime::from_hms_opt(h, m, 0)
}

/// `5` / `5:30` followed by `am` / `pm` as separate tokens.
fn split_meridiem(a: &str, b: Option<&str>) -> Option<NaiveTime> {
    let pm = match b? {
        "am" => false,
        "pm" => true,
        _ => return None,
    };
    let (h, m, _) = hour_min(a)?;
    clock12(h, m, pm)
}

fn try_time(toks: &[Tok], i: usize) -> Option<(NaiveTime, usize)> {
    let w = word(toks, i)?;
    let w1 = word(toks, i + 1);
    if w == "at" {
        let a = w1?;
        if let Some(t) = split_meridiem(a, word(toks, i + 2)) {
            return Some((t, 3));
        }
        if let Some(t) = time_token(a) {
            return Some((t, 2));
        }
        // `at 9` / `at 17`: 24-hour clock.
        let (h, m, colon) = hour_min(a)?;
        if colon {
            return None;
        }
        return NaiveTime::from_hms_opt(h, m, 0).map(|t| (t, 2));
    }
    if let Some(t) = split_meridiem(w, w1) {
        return Some((t, 2));
    }
    time_token(w).map(|t| (t, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Friday 2026-11-13, 10:00.
    fn now() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 11, 13).unwrap().and_hms_opt(10, 0, 0).unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> Option<NaiveDate> {
        NaiveDate::from_ymd_opt(y, m, day)
    }

    fn t(h: u32, m: u32) -> Option<NaiveTime> {
        NaiveTime::from_hms_opt(h, m, 0)
    }

    fn p(s: &str) -> Capture {
        parse(s, now())
    }

    #[test]
    fn fixture_is_a_friday() {
        assert_eq!(now().date().weekday(), Weekday::Fri);
    }

    #[test]
    fn full_example() {
        let c = p("pay rent fri 5pm #home !high");
        assert_eq!(c.title, "pay rent");
        assert_eq!(c.due_date, d(2026, 11, 13)); // today is Friday and 5pm is still ahead
        assert_eq!(c.due_time, t(17, 0));
        assert_eq!(c.tags, vec!["home"]);
        assert_eq!(c.priority, Priority::High);
        assert!(c.links.is_empty());
    }

    #[test]
    fn plain_title() {
        let c = p("buy milk");
        assert_eq!(c, Capture { title: "buy milk".into(), ..Default::default() });
    }

    #[test]
    fn today_tomorrow() {
        assert_eq!(p("a today").due_date, d(2026, 11, 13));
        assert_eq!(p("a tomorrow").due_date, d(2026, 11, 14));
        assert_eq!(p("a tmr").due_date, d(2026, 11, 14));
        assert_eq!(p("a tmrw").due_date, d(2026, 11, 14));
        let c = p("call mom tomorrow 9am");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("call mom", d(2026, 11, 14), t(9, 0)));
    }

    #[test]
    fn today_with_past_time_stays_today() {
        let c = p("a today 8am");
        assert_eq!((c.due_date, c.due_time), (d(2026, 11, 13), t(8, 0)));
    }

    #[test]
    fn tonight() {
        let c = p("movie tonight");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("movie", d(2026, 11, 13), t(20, 0)));
        let c = p("movie tonight 9pm");
        assert_eq!((c.due_date, c.due_time), (d(2026, 11, 13), t(21, 0)));
    }

    #[test]
    fn weekdays_are_strictly_upcoming() {
        assert_eq!(p("mon").due_date, d(2026, 11, 16));
        assert_eq!(p("tue").due_date, d(2026, 11, 17));
        assert_eq!(p("wednesday").due_date, d(2026, 11, 18));
        assert_eq!(p("thurs").due_date, d(2026, 11, 19));
        assert_eq!(p("a sat").due_date, d(2026, 11, 14));
        assert_eq!(p("a sun").due_date, d(2026, 11, 15));
        // Today is Friday: bare `fri` means next week...
        assert_eq!(p("a fri").due_date, d(2026, 11, 20));
        // ...unless a time still ahead today is given.
        assert_eq!(p("a fri 5pm").due_date, d(2026, 11, 13));
        assert_eq!(p("a fri 9am").due_date, d(2026, 11, 20));
        assert_eq!(p("a fri 10:00").due_date, d(2026, 11, 20));
    }

    #[test]
    fn next_weekday_never_means_today() {
        assert_eq!(p("a next fri 5pm").due_date, d(2026, 11, 20));
        let c = p("sync next mon 5pm");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("sync", d(2026, 11, 16), t(17, 0)));
    }

    #[test]
    fn relative_offsets() {
        let c = p("renew in 3 days");
        assert_eq!((c.title.as_str(), c.due_date), ("renew", d(2026, 11, 16)));
        assert_eq!(p("a in 1 day").due_date, d(2026, 11, 14));
        assert_eq!(p("a in 2 weeks").due_date, d(2026, 11, 27));
        assert_eq!(p("a in a week").due_date, d(2026, 11, 20));
        assert_eq!(p("a in 1 month").due_date, d(2026, 12, 13));
        assert_eq!(p("a in 3 months").due_date, d(2027, 2, 13));
        let c = p("a in 3 days at 9");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("a", d(2026, 11, 16), t(9, 0)));
    }

    #[test]
    fn in_without_a_unit_is_title() {
        assert_eq!(p("check in 3 times").title, "check in 3 times");
        assert_eq!(p("in time").title, "in time");
    }

    #[test]
    fn month_day_forms() {
        assert_eq!(p("a nov 20").due_date, d(2026, 11, 20));
        assert_eq!(p("a 20 nov").due_date, d(2026, 11, 20));
        assert_eq!(p("a november 20th").due_date, d(2026, 11, 20));
        assert_eq!(p("a 3rd december").due_date, d(2026, 12, 3));
        assert_eq!(p("a Dec 1st").due_date, d(2026, 12, 1));
        assert_eq!(p("a sept 9").due_date, d(2027, 9, 9));
        let c = p("taxes nov 20 5pm");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("taxes", d(2026, 11, 20), t(17, 0)));
        let c = p("taxes 20 nov 17:30");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("taxes", d(2026, 11, 20), t(17, 30)));
    }

    #[test]
    fn past_month_day_rolls_to_next_year() {
        assert_eq!(p("a nov 1").due_date, d(2027, 11, 1));
        assert_eq!(p("a jan 5").due_date, d(2027, 1, 5));
        assert_eq!(p("a nov 13").due_date, d(2026, 11, 13)); // today stays today
        assert_eq!(p("a feb 29").due_date, d(2028, 2, 29)); // next leap year
    }

    #[test]
    fn explicit_year_is_respected() {
        let c = p("a nov 1 2026");
        assert_eq!((c.title.as_str(), c.due_date), ("a", d(2026, 11, 1)));
        assert_eq!(p("a 5 mar 2030").due_date, d(2030, 3, 5));
    }

    #[test]
    fn invalid_month_day_stays_in_title() {
        let c = p("a feb 30");
        assert_eq!((c.title.as_str(), c.due_date), ("a feb 30", None));
        assert_eq!(p("a nov 32").due_date, None);
    }

    #[test]
    fn iso_dates() {
        let c = p("ship 2026-11-20");
        assert_eq!((c.title.as_str(), c.due_date), ("ship", d(2026, 11, 20)));
        assert_eq!(p("old 2020-01-02").due_date, d(2020, 1, 2));
        assert_eq!(p("bad 2026-13-40").due_date, None);
    }

    #[test]
    fn ambiguous_words_are_not_dates() {
        assert_eq!(p("may I buy milk"), Capture { title: "may I buy milk".into(), ..Default::default() });
        assert_eq!(p("a may 5").due_date, d(2027, 5, 5));
        assert_eq!(p("mar").due_date, None);
        assert_eq!(p("sat").due_date, d(2026, 11, 14));
        assert_eq!(p("next").title, "next");
        assert_eq!(p("look at me").title, "look at me");
        assert_eq!(p("a 5").title, "a 5");
    }

    #[test]
    fn month_followed_by_meridiem_is_a_time_not_a_day() {
        let c = p("a mar 5 pm");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("a mar", d(2026, 11, 13), t(17, 0)));
    }

    #[test]
    fn time_forms() {
        let time = |s: &str| p(s).due_time;
        assert_eq!(time("a 5pm"), t(17, 0));
        assert_eq!(time("a 5 pm"), t(17, 0));
        assert_eq!(time("a 5:30pm"), t(17, 30));
        assert_eq!(time("a 5:30 PM"), t(17, 30));
        assert_eq!(time("a 17:00"), t(17, 0));
        assert_eq!(time("a 9:05"), t(9, 5));
        assert_eq!(time("a at 9"), t(9, 0));
        assert_eq!(time("a at 17"), t(17, 0));
        assert_eq!(time("a at 9:30"), t(9, 30));
        assert_eq!(time("a at 9 pm"), t(21, 0));
        assert_eq!(time("a at 5pm"), t(17, 0));
        assert_eq!(time("a noon"), t(12, 0));
        assert_eq!(time("a at noon"), t(12, 0));
        assert_eq!(time("a midnight"), t(0, 0));
        assert_eq!(time("a 12am"), t(0, 0));
        assert_eq!(time("a 12pm"), t(12, 0));
        assert_eq!(time("a 12:30am"), t(0, 30));
        assert_eq!(time("a 11pm"), t(23, 0));
    }

    #[test]
    fn invalid_times_stay_in_title() {
        for s in ["a 13pm", "a 0pm", "a 25:00", "a 9:61", "a 9:5", "a 17", "a 5.30pm"] {
            let c = p(s);
            assert_eq!((c.title.as_str(), c.due_time), (s, None), "{s}");
        }
    }

    #[test]
    fn time_without_date_is_today_if_ahead_else_tomorrow() {
        let c = p("call 5pm");
        assert_eq!((c.title.as_str(), c.due_date, c.due_time), ("call", d(2026, 11, 13), t(17, 0)));
        let c = p("call 9am");
        assert_eq!((c.due_date, c.due_time), (d(2026, 11, 14), t(9, 0)));
        // Exactly now is not ahead.
        assert_eq!(p("call 10:00").due_date, d(2026, 11, 14));
        assert_eq!(p("call 10:01").due_date, d(2026, 11, 13));
        assert_eq!(p("call midnight").due_date, d(2026, 11, 14));
    }

    #[test]
    fn date_without_time_has_no_time() {
        assert_eq!(p("a tomorrow").due_time, None);
    }

    #[test]
    fn case_insensitive() {
        let c = p("Call Mom TOMORROW 5PM #Home !HIGH");
        assert_eq!(c.title, "Call Mom");
        assert_eq!((c.due_date, c.due_time), (d(2026, 11, 14), t(17, 0)));
        assert_eq!(c.tags, vec!["home"]);
        assert_eq!(c.priority, Priority::High);
        assert_eq!(p("a NEXT FRIDAY").due_date, d(2026, 11, 20));
        assert_eq!(p("a Nov 20").due_date, d(2026, 11, 20));
    }

    #[test]
    fn tokens_anywhere_in_the_text() {
        let c = p("tomorrow 5pm #work call the bank !low");
        assert_eq!(c.title, "call the bank");
        assert_eq!(c.tags, vec!["work"]);
        assert_eq!(c.priority, Priority::Low);
        let c = p("call the #bank tomorrow about the loan");
        assert_eq!(c.title, "call the about the loan");
    }

    #[test]
    fn priorities() {
        assert_eq!(p("a !high").priority, Priority::High);
        assert_eq!(p("a !h").priority, Priority::High);
        assert_eq!(p("a !med").priority, Priority::Med);
        assert_eq!(p("a !medium").priority, Priority::Med);
        assert_eq!(p("a !m").priority, Priority::Med);
        assert_eq!(p("a !low").priority, Priority::Low);
        assert_eq!(p("a !l").priority, Priority::Low);
        assert_eq!(p("a").priority, Priority::None);
        assert_eq!(p("a !low !high").priority, Priority::High);
        let c = p("wow !urgent");
        assert_eq!((c.title.as_str(), c.priority), ("wow !urgent", Priority::None));
        assert_eq!(p("a !high,").priority, Priority::High);
    }

    #[test]
    fn tags() {
        let c = p("a #home #Work #home, #deep-dive #a_b");
        assert_eq!(c.title, "a");
        assert_eq!(c.tags, vec!["home", "work", "deep-dive", "a_b"]);
        // `#` alone and non-tag characters are not tags.
        assert_eq!(p("a # b").title, "a # b");
        assert_eq!(p("a #x/y").title, "a #x/y");
    }

    #[test]
    fn links() {
        let c = p("read https://example.com/a?b=1#frag, then HTTP://X.org");
        assert_eq!(c.links, vec!["https://example.com/a?b=1#frag", "HTTP://X.org"]);
        assert!(c.tags.is_empty(), "fragment must not become a tag");
        assert_eq!(c.title, "read then");
        assert_eq!(p("(see https://a.io/x).").links, vec!["https://a.io/x"]);
        assert_eq!(p("https://en.wikipedia.org/wiki/Foo_(bar)").links, vec!["https://en.wikipedia.org/wiki/Foo_(bar)"]);
        assert_eq!(p("not a link http:// here").links, Vec::<String>::new());
    }

    #[test]
    fn urls_do_not_leak_dates() {
        let c = p("see https://example.com/mon/5pm");
        assert_eq!(c.due_date, None);
        assert_eq!(c.due_time, None);
    }

    #[test]
    fn whitespace_collapses() {
        let c = p("   pay \t rent   fri   #home   ");
        assert_eq!(c.title, "pay rent");
        assert_eq!(p("").title, "");
        assert_eq!(p("   ").title, "");
    }

    #[test]
    fn punctuation_around_date_words() {
        let c = p("dinner fri, 7pm.");
        assert_eq!(c.title, "dinner");
        assert_eq!((c.due_date, c.due_time), (d(2026, 11, 13), t(19, 0)));
    }

    #[test]
    fn only_first_date_and_first_time_are_consumed() {
        let c = p("sync mon tue");
        assert_eq!((c.title.as_str(), c.due_date), ("sync tue", d(2026, 11, 16)));
        let c = p("a 5pm 6pm");
        assert_eq!((c.title.as_str(), c.due_time), ("a 6pm", t(17, 0)));
    }

    #[test]
    fn title_can_be_empty() {
        let c = p("tomorrow");
        assert_eq!((c.title.as_str(), c.due_date), ("", d(2026, 11, 14)));
    }

    #[test]
    fn preview_full() {
        let c = Capture {
            title: "x".into(),
            due_date: d(2026, 11, 14),
            due_time: t(17, 0),
            priority: Priority::High,
            tags: vec!["home".into()],
            links: vec!["https://a.io".into()],
        };
        assert_eq!(c.preview(), "Sat 14 Nov · 17:00 · #home · !high · 1 link");
    }

    #[test]
    fn preview_omits_empty_pieces() {
        assert_eq!(p("buy milk").preview(), "Inbox");
        assert_eq!(p("a tomorrow").preview(), "Sat 14 Nov");
        assert_eq!(p("a #x #y !low").preview(), "Inbox · #x · #y · !low");
        assert_eq!(p("a !med https://a.io https://b.io").preview(), "Inbox · !med · 2 links");
        assert_eq!(p("a 9am").preview(), "Sat 14 Nov · 09:00");
    }

    #[test]
    fn into_new_task_maps_fields() {
        let task = p("pay rent mon 5pm #home !high https://a.io").into_new_task();
        assert_eq!(
            task,
            NewTask {
                title: "pay rent".into(),
                due_date: d(2026, 11, 16),
                due_time: t(17, 0),
                notes: String::new(),
                priority: Priority::High,
                tags: vec!["home".into()],
                links: vec!["https://a.io".into()],
            }
        );
    }
}
