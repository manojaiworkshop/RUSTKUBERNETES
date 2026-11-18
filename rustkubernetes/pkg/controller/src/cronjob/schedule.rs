//! Standard 5-field cron schedules (`minute hour day-of-month month day-of-week`),
//! the subset of `robfig/cron` used by Kubernetes CronJobs.
//!
//! Supports `*`, `?`, lists (`1,2`), ranges (`1-5`), steps (`*/15`, `1-30/5`),
//! month/day names (`JAN`, `MON`) and macros (`@hourly`, `@daily`, ...).
//! All times are UTC.

use chrono::{DateTime, Datelike, Duration, TimeZone, Timelike, Utc};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_star: bool,
    dow_star: bool,
}

const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn value(s: &str, names: &[&str], offset: u32) -> Result<u32, String> {
    if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(s)) {
        return Ok(i as u32 + offset);
    }
    s.parse().map_err(|_| format!("failed to parse int from {s}"))
}

/// Parses one field into a bitmask over `[min, max]`.
fn field(spec: &str, min: u32, max: u32, names: &[&str], offset: u32) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let star = spec == "*" || spec == "?";
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| format!("bad step {s}"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err("step of 0".into());
        }
        let (lo, hi) = if range == "*" || range == "?" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a, names, offset)?, value(b, names, offset)?)
        } else {
            let v = value(range, names, offset)?;
            (v, if part.contains('/') { max } else { v })
        };
        // Day-of-week 7 is Sunday.
        let (lo, hi) = if max == 6 { (lo % 7, if hi == 7 { 6 } else { hi }) } else { (lo, hi) };
        if lo < min || hi > max || lo > hi {
            return Err(format!("{part}: values must be within [{min}, {max}]"));
        }
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, star))
}

impl Schedule {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let expanded = match spec {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            s => s,
        };
        let f: Vec<&str> = expanded.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!("expected exactly 5 fields, found {}: {spec}", f.len()));
        }
        let (minute, _) = field(f[0], 0, 59, &[], 0)?;
        let (hour, _) = field(f[1], 0, 23, &[], 0)?;
        let (dom, dom_star) = field(f[2], 1, 31, &[], 0)?;
        let (month, _) = field(f[3], 1, 12, &MONTHS, 1)?;
        let (dow, dow_star) = field(f[4], 0, 6, &DAYS, 0)?;
        Ok(Self { minute, hour, dom, month, dow, dom_star, dow_star })
    }

    fn day_matches(&self, t: &DateTime<Utc>) -> bool {
        let d = self.dom & (1 << t.day()) != 0;
        let w = self.dow & (1 << t.weekday().num_days_from_sunday()) != 0;
        // Both restricted: either may match (cron semantics); otherwise both must.
        if !self.dom_star && !self.dow_star {
            d || w
        } else {
            d && w
        }
    }

    /// The first activation strictly after `after`.
    pub fn next(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut t = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let limit = after + Duration::days(366 * 5);
        while t <= limit {
            if self.month & (1 << t.month()) == 0 {
                let (y, m) = if t.month() == 12 { (t.year() + 1, 1) } else { (t.year(), t.month() + 1) };
                t = Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).single()?;
                continue;
            }
            if !self.day_matches(&t) {
                t = (t + Duration::days(1)).with_hour(0)?.with_minute(0)?;
                continue;
            }
            if self.hour & (1 << t.hour()) == 0 {
                t = (t + Duration::hours(1)).with_minute(0)?;
                continue;
            }
            if self.minute & (1 << t.minute()) == 0 {
                t += Duration::minutes(1);
                continue;
            }
            return Some(t);
        }
        None
    }

    /// Most recent activation in `(earliest, now]` and how many were missed.
    pub fn most_recent(&self, earliest: DateTime<Utc>, now: DateTime<Utc>) -> (Option<DateTime<Utc>>, usize) {
        let mut last = None;
        let mut count = 0;
        let mut t = earliest;
        while let Some(n) = self.next(t) {
            if n > now {
                break;
            }
            last = Some(n);
            count += 1;
            if count > 1000 {
                break;
            }
            t = n;
        }
        (last, count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn next_times() {
        let s = Schedule::parse("*/15 * * * *").unwrap();
        assert_eq!(s.next(at("2026-01-01T10:07:30Z")), Some(at("2026-01-01T10:15:00Z")));
        let s = Schedule::parse("0 9 * * MON-FRI").unwrap();
        // 2026-01-03 is a Saturday → next Monday 2026-01-05
        assert_eq!(s.next(at("2026-01-03T12:00:00Z")), Some(at("2026-01-05T09:00:00Z")));
        let s = Schedule::parse("@monthly").unwrap();
        assert_eq!(s.next(at("2026-01-15T00:00:00Z")), Some(at("2026-02-01T00:00:00Z")));
        let s = Schedule::parse("30 2 29 2 *").unwrap(); // leap day
        assert_eq!(s.next(at("2026-03-01T00:00:00Z")), Some(at("2028-02-29T02:30:00Z")));
        assert!(Schedule::parse("61 * * * *").is_err());
        assert!(Schedule::parse("* * *").is_err());
    }

    #[test]
    fn missed_runs() {
        let s = Schedule::parse("* * * * *").unwrap();
        let (last, n) = s.most_recent(at("2026-01-01T10:00:00Z"), at("2026-01-01T10:05:30Z"));
        assert_eq!(last, Some(at("2026-01-01T10:05:00Z")));
        assert_eq!(n, 5);
    }
}
