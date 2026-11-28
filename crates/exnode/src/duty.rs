//! What the node is doing about the exchange right now, and how much of the
//! machine it brings.
//!
//! Four things decide it, in this order: the safety checks (a failing one
//! keeps the node out of service), a pause (`exnode pause`, a local decision
//! that outranks everything else), the weekly schedule, and the blackout
//! dates.  When all allow it the node serves, up to a share of
//! `max_concurrency` that the schedule gives for that moment.
//!
//! The node reports its duty to the hub with every heartbeat, so that the hub
//! can tell a machine that is off duty from one that is idle: it doesn't count
//! the first as supply, and it tells a buyer when the next window opens.  The
//! hub is told; it is never in charge.  The limits here are the owner's, set in
//! `node.toml` on the machine itself.

use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{watch, Notify};

use ex_common::schedule::{fmt_clock, Schedule, Why, Zone};

/// What the node is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Taking jobs.
    Serving,
    /// Off: the weekly schedule has no window now.
    Scheduled,
    /// Off: a blackout date.
    Blackout,
    /// Off: `exnode pause`.
    Paused,
    /// Off: a safety check failed (a vulnerable or exposed backend).
    Checks,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Serving   => "serving",
            Self::Scheduled => "scheduled",
            Self::Blackout  => "blackout",
            Self::Paused    => "paused",
            Self::Checks    => "checks",
        }
    }
}

/// The node's duty at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duty {
    pub state:          State,
    /// Jobs at once it takes now: `max_concurrency` × the share, rounded up, and never 0 while serving.
    pub capacity:       usize,
    /// Jobs at once the machine is set up for (`max_concurrency`).
    pub of:             usize,
    /// Percent of the machine the schedule gives now.
    pub share:          u8,
    /// When this changes next, if it is known: the end of a pause, or the schedule's next change.
    pub next_change_at: Option<u64>,
}

impl Duty {
    pub fn serving(&self) -> bool { self.state == State::Serving && self.capacity > 0 }

    /// What a heartbeat tells the hub.
    pub fn to_json(&self) -> Value {
        json!({
            "state":          self.state.as_str(),
            "serving":        self.serving(),
            "capacity":       self.capacity,
            "of":             self.of,
            "next_change_at": self.next_change_at,
        })
    }

    /// One sentence, for the log and for `exnode doctor`.
    pub fn sentence(&self, zone: Zone) -> String {
        let at = |t: u64| DateTime::from_timestamp(t as i64, 0).map(|d| zone.wall(d).format("%a %d %b %H:%M").to_string());
        let until = |what: &str| self.next_change_at.and_then(at).map(|t| format!("; {what} {t}")).unwrap_or_default();
        match self.state {
            State::Serving => format!("serving: up to {} of {} jobs at once ({}% of the machine){}",
                                      self.capacity, self.of, self.share, until("changes")),
            State::Scheduled => format!("off by the schedule{}", until("next window opens")),
            State::Blackout  => format!("off for a blackout date{}", until("serving resumes")),
            State::Paused    => format!("paused (exnode resume to end it){}", until("the pause ends")),
            State::Checks    => "not serving: a safety check failed (exnode doctor says which)".into(),
        }
    }
}

/// `max × share%`, rounded up and at least 1: a machine that serves at all serves one job.
pub fn capacity_for(share: u8, max: usize) -> usize {
    if share == 0 { 0 } else { (max * share as usize).div_ceil(100).clamp(1, max.max(1)) }
}

/// The duty at `now`.
pub fn evaluate(schedule: &Schedule, max: usize, control: &Control, checks_ok: bool, now: DateTime<Utc>) -> Duty {
    let off = |state, next_change_at| Duty { state, capacity: 0, of: max, share: 0, next_change_at };
    if !checks_ok {
        return off(State::Checks, None);
    }
    let unix = |t: DateTime<Utc>| t.timestamp().max(0) as u64;
    if control.pausing(unix(now)) {
        return off(State::Paused, control.until);
    }
    let slot = schedule.at(now);
    let next = schedule.next_change(now).map(unix);
    match (slot.share, slot.why) {
        (0, Why::Blackout) => off(State::Blackout, next),
        (0, _)             => off(State::Scheduled, next),
        (share, _)         => Duty { state: State::Serving, capacity: capacity_for(share, max), of: max, share, next_change_at: next },
    }
}

// ── the local pause ───────────────────────────────────────────────────────────

/// A pause the owner set on this machine with `exnode pause`, kept in a small file beside the key.
/// The agent never listens on a port, so the command and the agent meet in this file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    #[serde(default)]
    pub paused: bool,
    /// When the pause ends (unix seconds); absent, it lasts until `exnode resume`.
    #[serde(default)]
    pub until:  Option<u64>,
}

impl Control {
    /// Where the control file lives for a node whose key is `key_file`.
    pub fn path_for(key_file: &str) -> PathBuf {
        Path::new(key_file).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).join("control.json")
    }

    /// The file's content; a missing or unreadable file is no pause.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    pub fn pausing(&self, now: u64) -> bool { self.paused && self.until.is_none_or(|u| now < u) }

    /// Write it atomically, readable by the agent's user (the file holds no secret).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).expect("a small struct"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{chown, MetadataExt, PermissionsExt};
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?;
            // written by root, it belongs to whoever owns the directory: the agent's user
            if let Ok(dir) = std::fs::metadata(path.parent().unwrap_or_else(|| Path::new("."))) {
                let _ = chown(&tmp, Some(dir.uid()), Some(dir.gid()));
            }
        }
        std::fs::rename(&tmp, path)
    }
}

/// `30m`, `2h`, `1d`, `90s`: a length of time.
pub fn parse_span(raw: &str) -> Result<Duration, String> {
    let s = raw.trim();
    let bad = || format!("{raw:?} is not a length of time: use a number and s, m, h or d, such as 30m or 2h");
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?);
    let n: u64 = n.parse().map_err(|_| bad())?;
    let secs = match unit { "s" => 1, "m" => 60, "h" => 3600, "d" => 86_400, _ => return Err(bad()) };
    if n == 0 || n > 366 * 86_400 / secs {
        return Err(bad());
    }
    Ok(Duration::from_secs(n * secs))
}

// ── the schedule, in words ────────────────────────────────────────────────────

/// The next seven days of a schedule as a person reads them, for `exnode schedule`.
pub fn week_report(schedule: &Schedule, max: usize, now: DateTime<Utc>) -> String {
    let zone = schedule.zone();
    let mut out = format!("Times are {}.\n", zone.describe(now));
    out += &format!("At full capacity this machine takes {max} job{} at once.\n", if max == 1 { "" } else { "s" });
    if schedule.is_always() {
        out += "No schedule: it serves all the time, at full capacity.\n";
        return out;
    }
    let start: NaiveDate = zone.wall(now).date();
    out += "\n";
    for d in 0..7 {
        let date = start + chrono::Duration::days(d);
        let segs = schedule.day_segments(date);
        let text = if segs.is_empty() {
            "not serving".to_string()
        } else {
            segs.iter().map(|s| format!("{}-{} {}% ({})", fmt_clock(s.start), fmt_clock(s.end), s.share, capacity_for(s.share, max)))
                .collect::<Vec<_>>().join("   ")
        };
        out += &format!("  {}   {text}\n", date.format("%a %d %b"));
    }
    out
}

// ── how many jobs at once ─────────────────────────────────────────────────────

/// Places for jobs that follow the duty: a job takes one, and none is given while the node is off
/// duty or full.  A place is held while a job runs and while the node waits for one, so a node that
/// serves several models never waits on more at once than it could run.
#[derive(Debug, Default)]
pub struct Limiter {
    in_flight: AtomicUsize,
    freed:     Notify,
}

/// A place taken; giving it back (dropping it) wakes a waiter.
#[derive(Debug)]
pub struct Place(Arc<Limiter>);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.freed.notify_waiters();
    }
}

impl Limiter {
    pub fn in_flight(&self) -> usize { self.in_flight.load(Ordering::SeqCst) }

    /// Wait until the node is serving and has a place to spare, and take it.  `None` when the duty's
    /// source has gone away, which means the agent is shutting down.
    pub async fn acquire(self: &Arc<Self>, duty: &mut watch::Receiver<Duty>) -> Option<Place> {
        loop {
            // listen before looking, so a place freed between the look and the wait is not missed
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            let d = duty.borrow_and_update().clone();
            if d.serving() {
                let now = self.in_flight.load(Ordering::SeqCst);
                if now < d.capacity {
                    if self.in_flight.compare_exchange(now, now + 1, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
                        return Some(Place(self.clone()));
                    }
                    continue; // another waiter took it: look again
                }
            }
            tokio::select! {
                changed = duty.changed() => { if changed.is_err() { return None; } }
                _ = &mut freed => {}
                _ = tokio::time::sleep(Duration::from_secs(5)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use ex_common::schedule::Rule;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Utc> { Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap() }

    /// Weekday nights in full, weekends at half, on UTC.  2026-11-06 is a Friday.
    fn nights_and_weekends() -> Schedule {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        Schedule::new(Zone::parse(Some("UTC")).unwrap(), vec![
            Rule::parse(&s(&["weekdays"]), "18:00-08:00", 100).unwrap(),
            Rule::parse(&s(&["weekends"]), "all day", 50).unwrap(),
        ], vec![]).unwrap()
    }

    #[test]
    fn a_share_of_the_machine_is_a_number_of_jobs_rounded_up() {
        assert_eq!([capacity_for(100, 4), capacity_for(50, 4), capacity_for(25, 4), capacity_for(10, 4), capacity_for(1, 4)], [4, 2, 1, 1, 1]);
        assert_eq!([capacity_for(50, 1), capacity_for(100, 1)], [1, 1], "one job is the least a serving machine takes");
        assert_eq!(capacity_for(30, 10), 3);
        assert_eq!(capacity_for(0, 4), 0);
    }

    #[test]
    fn safety_outranks_a_pause_outranks_the_schedule() {
        let sch = nights_and_weekends();
        let night = utc(2026, 11, 6, 20, 0);
        let none = Control::default();
        let paused = Control { paused: true, until: None };
        let d = evaluate(&sch, 4, &none, true, night);
        assert_eq!((d.state, d.capacity, d.share), (State::Serving, 4, 100));
        assert_eq!(d.next_change_at, Some(utc(2026, 11, 7, 8, 0).timestamp() as u64), "100% becomes 50% on Saturday morning");
        assert_eq!(evaluate(&sch, 4, &paused, true, night).state, State::Paused);
        assert_eq!(evaluate(&sch, 4, &paused, false, night).state, State::Checks, "a failing check is reported over a pause");
        assert_eq!(evaluate(&sch, 4, &none, false, night).capacity, 0);
        let day = evaluate(&sch, 4, &none, true, utc(2026, 11, 6, 12, 0));
        assert_eq!((day.state, day.capacity, day.next_change_at), (State::Scheduled, 0, Some(utc(2026, 11, 6, 18, 0).timestamp() as u64)));
        assert_eq!(evaluate(&sch, 4, &none, true, utc(2026, 11, 7, 12, 0)).capacity, 2, "a weekend at half");
    }

    #[test]
    fn a_timed_pause_ends_by_itself_and_an_open_one_does_not() {
        let sch = Schedule::always();
        let now = utc(2026, 11, 6, 12, 0);
        let t = now.timestamp() as u64;
        let timed = Control { paused: true, until: Some(t + 600) };
        assert_eq!(evaluate(&sch, 2, &timed, true, now).state, State::Paused);
        assert_eq!(evaluate(&sch, 2, &timed, true, now).next_change_at, Some(t + 600));
        assert_eq!(evaluate(&sch, 2, &timed, true, now + chrono::Duration::seconds(601)).state, State::Serving);
        assert_eq!(evaluate(&sch, 2, &Control { paused: true, until: None }, true, now + chrono::Duration::days(30)).state, State::Paused);
        assert_eq!(evaluate(&sch, 2, &Control::default(), true, now).capacity, 2, "no schedule: all the time, in full");
    }

    #[test]
    fn a_blackout_is_off_and_says_so() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let sch = Schedule::new(Zone::parse(Some("UTC")).unwrap(), vec![],
                                ex_common::schedule::parse_blackout(&s(&["2026-12-25"])).unwrap()).unwrap();
        let d = evaluate(&sch, 2, &Control::default(), true, utc(2026, 12, 25, 12, 0));
        assert_eq!((d.state, d.capacity, d.next_change_at), (State::Blackout, 0, Some(utc(2026, 12, 26, 0, 0).timestamp() as u64)));
    }

    #[test]
    fn the_pause_file_round_trips_and_a_missing_one_is_no_pause() {
        let dir = std::env::temp_dir().join(format!("exnode-duty-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = Control::path_for(dir.join("node.key").to_str().unwrap());
        assert_eq!(path, dir.join("control.json"));
        assert_eq!(Control::load(&path), Control::default(), "no file: not paused");
        let c = Control { paused: true, until: Some(1_800_000_000) };
        c.save(&path).unwrap();
        assert_eq!(Control::load(&path), c);
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Control::load(&path), Control::default(), "a damaged file is no pause");
        Control::default().save(&path).unwrap();
        assert!(!Control::load(&path).pausing(0));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lengths_of_time_are_a_number_and_a_unit() {
        assert_eq!(parse_span("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_span("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_span("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_span("1d"), Ok(Duration::from_secs(86_400)));
        for bad in ["", "m", "0m", "5", "5x", "-5m", "1.5h", "999d"] {
            assert!(parse_span(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_week_report_reads_as_a_calendar() {
        let r = week_report(&nights_and_weekends(), 4, utc(2026, 11, 6, 12, 0));
        assert!(r.contains("UTC+00:00") || r.contains("(UTC"), "{r}");
        assert!(r.contains("takes 4 jobs at once"), "{r}");
        assert!(r.contains("Fri 06 Nov   00:00-08:00 100% (4)   18:00-24:00 100% (4)"), "{r}");
        assert!(r.contains("Sat 07 Nov   00:00-08:00 100% (4)   08:00-24:00 50% (2)"), "{r}");
        assert!(r.contains("Mon 09 Nov   18:00-24:00 100% (4)"), "{r}");
        assert!(week_report(&Schedule::always(), 2, utc(2026, 11, 6, 12, 0)).contains("serves all the time"));
    }

    #[test]
    fn a_duty_is_told_in_a_sentence_and_to_the_hub_in_json() {
        let d = evaluate(&nights_and_weekends(), 4, &Control::default(), true, utc(2026, 11, 6, 20, 0));
        assert_eq!(d.sentence(Zone::parse(Some("UTC")).unwrap()),
                   "serving: up to 4 of 4 jobs at once (100% of the machine); changes Sat 07 Nov 08:00");
        assert_eq!(d.to_json(), json!({"state": "serving", "serving": true, "capacity": 4, "of": 4, "next_change_at": utc(2026, 11, 7, 8, 0).timestamp()}));
        let off = evaluate(&nights_and_weekends(), 4, &Control::default(), true, utc(2026, 11, 6, 12, 0));
        assert_eq!(off.sentence(Zone::parse(Some("UTC")).unwrap()), "off by the schedule; next window opens Fri 06 Nov 18:00");
        assert_eq!(off.to_json()["serving"], false);
    }

    #[tokio::test]
    async fn places_follow_the_duty_and_wait_when_full_or_off() {
        let on = |capacity| Duty { state: State::Serving, capacity, of: 4, share: 100, next_change_at: None };
        let off = Duty { state: State::Scheduled, capacity: 0, of: 4, share: 0, next_change_at: None };
        let (tx, rx) = watch::channel(on(2));
        let limiter = Arc::new(Limiter::default());
        let (mut a, mut b, mut c) = (rx.clone(), rx.clone(), rx.clone());
        let p1 = limiter.acquire(&mut a).await.unwrap();
        let _p2 = limiter.acquire(&mut b).await.unwrap();
        assert_eq!(limiter.in_flight(), 2);
        // full: the third waits until one is given back
        let l = limiter.clone();
        let waiter = tokio::spawn(async move { l.acquire(&mut c).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "full: no place");
        drop(p1);
        let _p3 = tokio::time::timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap().expect("a freed place goes to the waiter");
        assert_eq!(limiter.in_flight(), 2, "full again");
        // raising the capacity lets another in at once
        let mut d = rx.clone();
        let l = limiter.clone();
        let more = tokio::spawn(async move { l.acquire(&mut d).await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!more.is_finished(), "still full at two");
        tx.send(on(4)).unwrap();
        let _p4 = tokio::time::timeout(Duration::from_secs(2), more).await.unwrap().unwrap().expect("room now");
        assert_eq!(limiter.in_flight(), 3);
        // off duty: nothing is given, however many places are free
        let (tx2, rx2) = watch::channel(off.clone());
        let fresh = Arc::new(Limiter::default());
        let mut e = rx2.clone();
        let f = fresh.clone();
        let blocked = tokio::spawn(async move { f.acquire(&mut e).await.is_some() });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!blocked.is_finished(), "off duty: no place");
        tx2.send(on(1)).unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(2), blocked).await.unwrap().unwrap());
        // the end of the agent ends the wait
        let (tx3, rx3) = watch::channel(off);
        let mut g = rx3.clone();
        let l3 = Arc::new(Limiter::default());
        let ended = tokio::spawn(async move { l3.acquire(&mut g).await.is_none() });
        drop(tx3);
        assert!(tokio::time::timeout(Duration::from_secs(2), ended).await.unwrap().unwrap());
    }
}
