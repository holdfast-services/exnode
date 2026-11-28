//! Serving schedules: when a machine brings its capacity to the exchange, and
//! how much of it.
//!
//! A schedule is a list of weekly rules, each a set of days, a window of the
//! day and a share of the machine's capacity, plus dates when nothing is
//! served.  The first rule that covers a moment decides it.  No rule covering
//! it means off; no rules at all means on, in full.  A window may cross
//! midnight: `18:00-08:00` on Monday runs until Tuesday 08:00, and the morning
//! part belongs to the day the window started on.  A blackout date turns the
//! whole local date off, whatever the rules say.
//!
//! Times are read on a [`Zone`]: this machine's local time, or a named zone
//! such as `America/Chicago`, because a server in a datacenter usually runs
//! on UTC while the people who own it work on local time.
//!
//! Everything here is pure: the node evaluates it to decide whether to take a
//! job, and the hub uses the same code to check what a node says it will do.

use std::str::FromStr;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc, Weekday};

/// How far ahead [`Schedule::next_change`] looks.
pub const HORIZON_DAYS: i64 = 8;
/// Most rules, and most blackout entries, one schedule may hold.
pub const MAX_RULES: usize = 32;
pub const MAX_BLACKOUTS: usize = 100;
/// Longest single blackout span, in days.
pub const MAX_BLACKOUT_DAYS: i64 = 400;
const DAY: u16 = 24 * 60;

// ── the clock ─────────────────────────────────────────────────────────────────

/// The clock a schedule is read on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Zone {
    /// This machine's local time.
    #[default]
    Local,
    /// An IANA zone such as `America/Chicago` or `UTC`.
    Named(chrono_tz::Tz),
}

impl Zone {
    /// `None` or empty is the machine's local time; anything else must be a zone name.
    pub fn parse(name: Option<&str>) -> Result<Self, String> {
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            None    => Ok(Self::Local),
            Some(n) => chrono_tz::Tz::from_str(n).map(Self::Named)
                .map_err(|_| format!("timezone {n:?} is not a zone name such as \"America/Chicago\" or \"UTC\"")),
        }
    }

    /// The wall-clock time in this zone at `t`.
    pub fn wall(&self, t: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Self::Local    => t.with_timezone(&chrono::Local).naive_local(),
            Self::Named(z) => t.with_timezone(z).naive_local(),
        }
    }

    /// The zone and its offset at `t`, for showing what a schedule's times mean.
    pub fn describe(&self, t: DateTime<Utc>) -> String {
        match self {
            Self::Local    => format!("this machine's local time (UTC{})", t.with_timezone(&chrono::Local).format("%:z")),
            Self::Named(z) => format!("{} (UTC{})", z.name(), t.with_timezone(z).format("%:z")),
        }
    }
}

// ── days, hours and dates ─────────────────────────────────────────────────────

/// A set of weekdays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Days(u8);

impl Days {
    pub const ALL: Days = Days(0b111_1111);

    pub fn has(self, d: Weekday) -> bool { self.0 & (1 << d.num_days_from_monday()) != 0 }

    /// Day names (`mon`, `tuesday`, …) and the groups `weekdays`, `weekends` and `daily`.  Empty is every day.
    pub fn parse(items: &[String]) -> Result<Self, String> {
        if items.is_empty() {
            return Ok(Self::ALL);
        }
        let mut bits = 0u8;
        for raw in items {
            bits |= match raw.trim().to_ascii_lowercase().as_str() {
                "mon" | "monday"                  => 1 << 0,
                "tue" | "tues" | "tuesday"        => 1 << 1,
                "wed" | "wednesday"               => 1 << 2,
                "thu" | "thur" | "thurs" | "thursday" => 1 << 3,
                "fri" | "friday"                  => 1 << 4,
                "sat" | "saturday"                => 1 << 5,
                "sun" | "sunday"                  => 1 << 6,
                "weekday" | "weekdays"            => 0b001_1111,
                "weekend" | "weekends"            => 0b110_0000,
                "daily" | "all" | "every day"     => 0b111_1111,
                _ => return Err(format!("{raw:?} is not a day: use mon, tue, wed, thu, fri, sat, sun, weekdays or weekends")),
            };
        }
        Ok(Self(bits))
    }
}

/// `H:MM` or `HH:MM`; `24:00` only as an end.
fn clock(s: &str, allow_24: bool) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    match (h, m) {
        (24, 0) if allow_24 => Some(DAY),
        (0..=23, 0..=59)    => Some(h * 60 + m),
        _                   => None,
    }
}

/// A window of the day as minutes after midnight: `18:00-08:00`, or `all day`.
/// The end may be `24:00`; a start after the end crosses midnight.
pub fn parse_hours(raw: &str) -> Result<(u16, u16), String> {
    let s = raw.trim();
    if matches!(s.to_ascii_lowercase().as_str(), "all day" | "all-day" | "always" | "24h" | "24 hours") {
        return Ok((0, DAY));
    }
    let bad = || format!("hours must look like \"18:00-08:00\" (24-hour time) or be \"all day\", got {s:?}");
    let (a, b) = s.split_once('-').ok_or_else(bad)?;
    let (start, end) = (clock(a, false).ok_or_else(bad)?, clock(b, true).ok_or_else(bad)?);
    // 00:00-24:00 is the whole day; any other window that ends where it starts is empty or ambiguous
    if start == end % DAY && !(start == 0 && end == DAY) {
        return Err(format!("hours {s:?} start and end at the same time; use \"all day\" to serve all day"));
    }
    Ok((start, end))
}

/// `HH:MM`, for showing minutes after midnight.
pub fn fmt_clock(minute: u16) -> String { format!("{:02}:{:02}", minute / 60, minute % 60) }

/// An inclusive span of dates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DateSpan {
    pub from: NaiveDate,
    pub to:   NaiveDate,
}

/// Blackout dates: each `2026-12-25` or a span, `2026-12-24..2027-01-02`.
pub fn parse_blackout(items: &[String]) -> Result<Vec<DateSpan>, String> {
    if items.len() > MAX_BLACKOUTS {
        return Err(format!("at most {MAX_BLACKOUTS} blackout dates or spans"));
    }
    let date = |s: &str, whole: &str| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| format!("blackout {whole:?} must be a date like 2026-12-25 or a span like 2026-12-24..2027-01-02"));
    items.iter().map(|raw| {
        let (from, to) = match raw.split_once("..") {
            Some((a, b)) => (date(a, raw)?, date(b, raw)?),
            None         => { let d = date(raw, raw)?; (d, d) }
        };
        if to < from {
            return Err(format!("blackout {raw:?} ends before it starts"));
        }
        if (to - from).num_days() >= MAX_BLACKOUT_DAYS {
            return Err(format!("blackout {raw:?} is longer than {MAX_BLACKOUT_DAYS} days"));
        }
        Ok(DateSpan { from, to })
    }).collect()
}

// ── rules and the schedule ────────────────────────────────────────────────────

/// One weekly rule: on these days, in this window, serve this share of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rule {
    pub days:  Days,
    pub start: u16,
    pub end:   u16,
    /// Percent of the machine's capacity, 1 to 100.
    pub share: u8,
}

impl Rule {
    /// A rule from what a person writes: day names, a window and a percent.
    pub fn parse(days: &[String], hours: &str, share: u32) -> Result<Self, String> {
        let (start, end) = parse_hours(hours)?;
        if !(1..=100).contains(&share) {
            return Err(format!("share must be 1 to 100 percent, got {share}"));
        }
        Ok(Self { days: Days::parse(days)?, start, end, share: share as u8 })
    }

    fn covers(&self, wall: NaiveDateTime) -> bool {
        let minute = (wall.hour() * 60 + wall.minute()) as u16;
        let today = wall.weekday();
        if self.start < self.end {
            self.days.has(today) && minute >= self.start && minute < self.end
        } else {
            // crossing midnight: the evening belongs to the day it starts on, the morning to the day before
            (self.days.has(today) && minute >= self.start) || (self.days.has(today.pred()) && minute < self.end)
        }
    }
}

/// Why a moment is on or off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// On, by a rule (or by having no rules).
    Open,
    /// Off: no rule covers it.
    OffSchedule,
    /// Off: a blackout date.
    Blackout,
}

/// What a schedule says about one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Percent of the machine's capacity to bring; 0 is off.
    pub share: u8,
    pub why:   Why,
}

impl Slot {
    pub fn serving(&self) -> bool { self.share > 0 }
}

/// A stretch of a day that serves, for showing a schedule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub start: u16,
    pub end:   u16,
    pub share: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schedule {
    zone:     Zone,
    rules:    Vec<Rule>,
    blackout: Vec<DateSpan>,
}

impl Schedule {
    pub fn always() -> Self { Self::default() }

    pub fn new(zone: Zone, rules: Vec<Rule>, blackout: Vec<DateSpan>) -> Result<Self, String> {
        if rules.len() > MAX_RULES {
            return Err(format!("at most {MAX_RULES} schedule rules"));
        }
        if blackout.len() > MAX_BLACKOUTS {
            return Err(format!("at most {MAX_BLACKOUTS} blackout dates or spans"));
        }
        Ok(Self { zone, rules, blackout })
    }

    /// No rules and no blackout dates: on, in full, all the time.
    pub fn is_always(&self) -> bool { self.rules.is_empty() && self.blackout.is_empty() }

    pub fn zone(&self) -> Zone { self.zone }

    /// The slot at a wall-clock time in the schedule's zone.
    pub fn slot_at_wall(&self, wall: NaiveDateTime) -> Slot {
        let date = wall.date();
        if self.blackout.iter().any(|b| b.from <= date && date <= b.to) {
            return Slot { share: 0, why: Why::Blackout };
        }
        if self.rules.is_empty() {
            return Slot { share: 100, why: Why::Open };
        }
        match self.rules.iter().find(|r| r.covers(wall)) {
            Some(r) => Slot { share: r.share, why: Why::Open },
            None    => Slot { share: 0, why: Why::OffSchedule },
        }
    }

    pub fn at(&self, t: DateTime<Utc>) -> Slot { self.slot_at_wall(self.zone.wall(t)) }

    /// The first minute, within [`HORIZON_DAYS`], at which the share differs from what it is at `t`;
    /// `None` when it doesn't change that soon.  Stepped a minute at a time on UTC, so a change of
    /// clocks (daylight saving) lands where it really falls.
    pub fn next_change(&self, t: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let start = t.with_second(0)?.with_nanosecond(0)?;
        let now = self.at(start).share;
        (1..=HORIZON_DAYS * 24 * 60).map(|m| start + Duration::minutes(m)).find(|u| self.at(*u).share != now)
    }

    /// The stretches of `date` (a local date in the schedule's zone) that serve, with the share of each.
    pub fn day_segments(&self, date: NaiveDate) -> Vec<Segment> {
        let mut out: Vec<Segment> = Vec::new();
        for m in 0..DAY {
            let wall = date.and_hms_opt((m / 60) as u32, (m % 60) as u32, 0).expect("a minute of a day");
            let slot = self.slot_at_wall(wall);
            if !slot.serving() {
                continue;
            }
            match out.last_mut() {
                Some(last) if last.end == m && last.share == slot.share => last.end = m + 1,
                _ => out.push(Segment { start: m, end: m + 1, share: slot.share }),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::Tz;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> { Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap() }
    fn s(v: &[&str]) -> Vec<String> { v.iter().map(|x| x.to_string()).collect() }

    /// Weekday nights in full, weekends at half.  2026-11-06 is a Friday.
    fn nights_and_weekends(zone: Zone) -> Schedule {
        Schedule::new(zone, vec![
            Rule::parse(&s(&["weekdays"]), "18:00-08:00", 100).unwrap(),
            Rule::parse(&s(&["sat", "sun"]), "all day", 50).unwrap(),
        ], vec![]).unwrap()
    }

    #[test]
    fn a_window_that_crosses_midnight_belongs_to_the_day_it_started_on() {
        let sch = nights_and_weekends(Zone::Named(Tz::UTC));
        let share = |d, h, m| sch.at(utc(2026, 11, d, h, m)).share;
        assert_eq!(share(6, 17, 59), 0, "Friday before 18:00");
        assert_eq!(share(6, 18, 0), 100, "Friday evening opens");
        assert_eq!(share(7, 3, 0), 100, "Saturday small hours are Friday night's");
        assert_eq!(share(7, 7, 59), 100);
        assert_eq!(share(7, 8, 0), 50, "then the weekend rule, at half");
        assert_eq!(share(8, 23, 59), 50, "Sunday night");
        assert_eq!(share(9, 0, 0), 0, "Monday small hours belong to Sunday, which has no night rule");
        assert_eq!(share(9, 17, 59), 0);
        assert_eq!(share(9, 18, 0), 100, "Monday evening");
        assert_eq!(share(10, 7, 59), 100, "Tuesday morning is Monday night's");
        assert_eq!(share(10, 8, 0), 0);
    }

    #[test]
    fn the_first_rule_that_covers_a_moment_decides_it() {
        let both = Schedule::new(Zone::Named(Tz::UTC), vec![
            Rule::parse(&s(&["mon"]), "09:00-10:00", 25).unwrap(),
            Rule::parse(&[], "08:00-12:00", 75).unwrap(),
        ], vec![]).unwrap();
        // 2026-11-09 is a Monday
        assert_eq!(both.at(utc(2026, 11, 9, 9, 30)).share, 25);
        assert_eq!(both.at(utc(2026, 11, 9, 10, 0)).share, 75);
        assert_eq!(both.at(utc(2026, 11, 10, 9, 30)).share, 75, "Tuesday falls through to the second rule");
        assert_eq!(both.at(utc(2026, 11, 9, 12, 0)).share, 0);
    }

    #[test]
    fn a_blackout_date_turns_the_whole_date_off_and_says_why() {
        let sch = Schedule::new(Zone::Named(Tz::UTC),
            vec![Rule::parse(&[], "all day", 100).unwrap()],
            parse_blackout(&s(&["2026-12-24..2026-12-26", "2027-01-01"])).unwrap()).unwrap();
        assert_eq!(sch.at(utc(2026, 12, 23, 23, 59)), Slot { share: 100, why: Why::Open });
        for (d, h) in [(24, 0), (25, 12), (26, 23)] {
            assert_eq!(sch.at(utc(2026, 12, d, h, 0)), Slot { share: 0, why: Why::Blackout }, "Dec {d}");
        }
        assert_eq!(sch.at(utc(2026, 12, 27, 0, 0)).share, 100);
        assert_eq!(sch.at(utc(2027, 1, 1, 8, 0)).why, Why::Blackout);
        assert_eq!(sch.next_change(utc(2026, 12, 23, 12, 0)), Some(utc(2026, 12, 24, 0, 0)));
    }

    #[test]
    fn no_rules_means_on_in_full_and_a_rule_set_with_no_match_means_off() {
        assert!(Schedule::always().is_always());
        assert_eq!(Schedule::always().at(utc(2026, 11, 6, 3, 0)), Slot { share: 100, why: Why::Open });
        assert_eq!(Schedule::always().next_change(utc(2026, 11, 6, 3, 0)), None, "nothing to wait for");
        let weekend = Schedule::new(Zone::Named(Tz::UTC), vec![Rule::parse(&s(&["weekends"]), "all day", 100).unwrap()], vec![]).unwrap();
        assert_eq!(weekend.at(utc(2026, 11, 6, 12, 0)), Slot { share: 0, why: Why::OffSchedule });
    }

    #[test]
    fn the_next_change_is_the_next_minute_the_share_differs() {
        let sch = nights_and_weekends(Zone::Named(Tz::UTC));
        assert_eq!(sch.next_change(utc(2026, 11, 6, 12, 0)), Some(utc(2026, 11, 6, 18, 0)), "opens");
        assert_eq!(sch.next_change(utc(2026, 11, 6, 20, 30)), Some(utc(2026, 11, 7, 8, 0)), "100 becomes 50");
        assert_eq!(sch.next_change(utc(2026, 11, 8, 12, 0)), Some(utc(2026, 11, 9, 0, 0)), "Sunday ends");
        // seconds are ignored: a change is reported on a minute
        assert_eq!(sch.next_change(utc(2026, 11, 6, 12, 0) + Duration::seconds(37)), Some(utc(2026, 11, 6, 18, 0)));
    }

    #[test]
    fn times_are_read_on_the_named_zone_and_daylight_saving_lands_where_it_falls() {
        let chicago = Schedule::new(Zone::parse(Some("America/Chicago")).unwrap(),
            vec![Rule::parse(&[], "06:00-22:00", 100).unwrap()], vec![]).unwrap();
        // CST (UTC-6) in November 2026: 17:30 local is 23:30 UTC
        assert_eq!(chicago.at(utc(2026, 11, 6, 23, 30)).share, 100);
        assert_eq!(chicago.at(utc(2026, 11, 7, 3, 59)).share, 100, "21:59 local");
        assert_eq!(chicago.at(utc(2026, 11, 7, 4, 0)).share, 0, "22:00 local is off");
        // the night before clocks go forward (2027-03-14): 22:00 CST is 04:00 UTC
        assert_eq!(chicago.next_change(utc(2027, 3, 13, 22, 0)), Some(utc(2027, 3, 14, 4, 0)));
        // the evening after: 22:00 CDT (UTC-5) is 03:00 UTC the next day
        assert_eq!(chicago.next_change(utc(2027, 3, 14, 23, 0)), Some(utc(2027, 3, 15, 3, 0)));
    }

    #[test]
    fn a_days_segments_are_the_stretches_that_serve() {
        let sch = nights_and_weekends(Zone::Named(Tz::UTC));
        let date = |d| NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        // Friday: Thursday night's tail, nothing through the day, then the evening
        assert_eq!(sch.day_segments(date(6)), vec![Segment { start: 0, end: 8 * 60, share: 100 }, Segment { start: 18 * 60, end: DAY, share: 100 }]);
        // Saturday: Friday night's tail at full, then the weekend at half
        assert_eq!(sch.day_segments(date(7)), vec![Segment { start: 0, end: 8 * 60, share: 100 }, Segment { start: 8 * 60, end: DAY, share: 50 }]);
        // Monday: the small hours are Sunday's, and Sunday has no night rule, so just the evening
        assert_eq!(sch.day_segments(date(9)), vec![Segment { start: 18 * 60, end: DAY, share: 100 }]);
    }

    #[test]
    fn what_people_write_is_checked_and_the_errors_say_what_is_wrong() {
        assert!(Zone::parse(Some("America/Chicago")).is_ok() && Zone::parse(Some("UTC")).is_ok());
        assert_eq!(Zone::parse(None).unwrap(), Zone::Local);
        assert_eq!(Zone::parse(Some("  ")).unwrap(), Zone::Local);
        assert!(Zone::parse(Some("Mars/Olympus")).unwrap_err().contains("America/Chicago"));
        assert_eq!(parse_hours("18:00-08:00"), Ok((18 * 60, 8 * 60)));
        assert_eq!(parse_hours("all day"), Ok((0, DAY)));
        assert_eq!(parse_hours("00:00-24:00"), Ok((0, DAY)));
        for bad in ["6pm-8am", "18:00", "08:00-08:00", "25:00-26:00", "18:00-24:30", ""] {
            assert!(parse_hours(bad).is_err(), "{bad:?}");
        }
        assert!(Days::parse(&s(&["Mon", "weekends"])).unwrap().has(Weekday::Sun));
        assert!(Days::parse(&s(&["someday"])).unwrap_err().contains("weekdays"));
        assert!(Rule::parse(&[], "all day", 0).is_err() && Rule::parse(&[], "all day", 101).is_err());
        for bad in ["25-12-2026", "2026-12-24..", "2026-12-26..2026-12-24", "2026-01-01..2028-01-01", "christmas"] {
            assert!(parse_blackout(&s(&[bad])).is_err(), "{bad:?}");
        }
        assert!(Schedule::new(Zone::Local, (0..=MAX_RULES).map(|_| Rule::parse(&[], "all day", 100).unwrap()).collect(), vec![]).is_err());
    }
}
