//! Serving hours: `serve_hours = "18:00-08:00"` in node.toml.
//!
//! Inside the window the agent asks the hub for jobs as usual.  Outside it the
//! agent stops asking (a job already taken finishes) and keeps heartbeating,
//! so the node stays registered and shows as online but idle.  The window is
//! in this machine's local time, minutes resolution, and may cross midnight
//! (`22:00-06:00`).  Empty, absent or `always` means always.  A job long-poll
//! that started just before the window closed can still bring one job, up to
//! `poll_secs` later.
//!
//! This module is pure apart from [`local_minute`].

use std::fmt;

/// Minutes in a day.
const DAY: u16 = 24 * 60;

/// A daily window, `start` inclusive, `end` exclusive, in minutes after
/// local midnight.  `end` may be 1440 (`24:00`).  `start > end` crosses midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeHours {
    start: u16,
    end:   u16,
}

/// `H:MM` or `HH:MM`; `24:00` only when `allow_24` (an end).
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

impl ServeHours {
    /// `None` for always (empty, or `always`); an error that says what is wrong.
    pub fn parse(raw: &str) -> Result<Option<Self>, String> {
        let s = raw.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("always") {
            return Ok(None);
        }
        let bad = || format!("serve_hours must look like \"18:00-08:00\" (24-hour local time) or be empty for always, got {s:?}");
        let (a, b) = s.split_once('-').ok_or_else(bad)?;
        let start = clock(a, false).ok_or_else(bad)?;
        let end   = clock(b, true).ok_or_else(bad)?;
        if start == end % DAY {
            return Err(format!("serve_hours {s:?} starts and ends at the same time; leave it empty to serve always"));
        }
        Ok(Some(Self { start, end }))
    }

    /// Whether `minute` (after local midnight, 0..1440) is inside the window.
    pub fn contains(&self, minute: u16) -> bool {
        let m = minute % DAY;
        if self.start < self.end {
            m >= self.start && m < self.end
        } else {
            m >= self.start || m < self.end
        }
    }

    /// Minutes from `minute` until the window next opens; 0 inside it.
    pub fn minutes_until_open(&self, minute: u16) -> u16 {
        if self.contains(minute) {
            0
        } else {
            (self.start + DAY - minute % DAY) % DAY
        }
    }

    pub fn start(&self) -> String { hhmm(self.start) }
    pub fn end(&self) -> String { hhmm(self.end) }
}

fn hhmm(m: u16) -> String { format!("{:02}:{:02}", m / 60, m % 60) }

impl fmt::Display for ServeHours {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", hhmm(self.start), hhmm(self.end))
    }
}

/// Minutes after midnight now, in this machine's local time.
pub fn local_minute() -> u16 {
    use chrono::Timelike;
    let now = chrono::Local::now();
    (now.hour() * 60 + now.minute()) as u16
}

/// This machine's UTC offset now, e.g. `UTC-06:00`, for showing what "local" means.
pub fn local_offset() -> String {
    format!("UTC{}", chrono::Local::now().format("%:z"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> ServeHours { ServeHours::parse(s).unwrap().unwrap() }
    fn at(s: &str) -> u16 { clock(s, false).unwrap() }

    #[test]
    fn empty_and_always_mean_always() {
        for s in ["", "  ", "always", "ALWAYS"] {
            assert_eq!(ServeHours::parse(s), Ok(None), "{s:?}");
        }
    }

    #[test]
    fn valid_windows_parse_and_print_canonically() {
        assert_eq!(h("18:00-08:00").to_string(), "18:00-08:00");
        assert_eq!(h(" 9:30 - 17:00 ").to_string(), "09:30-17:00");
        assert_eq!(h("00:00-23:59").to_string(), "00:00-23:59");
        assert_eq!(h("22:15-23:45").to_string(), "22:15-23:45");
    }

    #[test]
    fn bad_windows_are_refused() {
        for s in ["18:00", "18-08", "18:00-08", "25:00-08:00", "18:60-08:00", "24:00-08:00", "18:00-08:00-09:00",
                  "1800-0800", "18:0-08:00", "-18:00-08:00", "+1:00-02:00", "18:00 to 08:00", "١٨:٠٠-٠٨:٠٠"] {
            assert!(ServeHours::parse(s).is_err(), "{s:?}");
        }
        assert!(ServeHours::parse("08:00-08:00").unwrap_err().contains("same time"));
        assert!(ServeHours::parse("00:00-00:00").is_err());
        assert!(ServeHours::parse("00:00-24:00").unwrap_err().contains("empty to serve always"));
    }

    #[test]
    fn a_window_across_midnight() {
        let w = h("18:00-08:00");
        for t in ["18:00", "23:59", "00:00", "07:59"] {
            assert!(w.contains(at(t)), "{t}");
        }
        for t in ["08:00", "12:00", "17:59"] {
            assert!(!w.contains(at(t)), "{t}");
        }
        assert_eq!(w.minutes_until_open(at("17:00")), 60);
        assert_eq!(w.minutes_until_open(at("08:00")), 600);
        assert_eq!(w.minutes_until_open(at("19:00")), 0);
    }

    #[test]
    fn a_window_within_a_day() {
        let w = h("09:00-17:00");
        assert!(w.contains(at("09:00")) && w.contains(at("16:59")));
        assert!(!w.contains(at("17:00")) && !w.contains(at("08:59")) && !w.contains(at("23:00")));
        assert_eq!(w.minutes_until_open(at("17:00")), 16 * 60);
        assert_eq!(w.minutes_until_open(at("08:00")), 60);
        let late = h("18:00-24:00");
        assert!(late.contains(at("23:59")) && !late.contains(at("00:00")));
    }
}
