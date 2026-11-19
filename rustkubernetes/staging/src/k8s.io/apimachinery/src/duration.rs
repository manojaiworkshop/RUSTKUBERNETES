//! Go's `time.ParseDuration` and `Duration.String`, used by command-line flags
//! (`metav1.Duration`).

use std::time::Duration;

/// `time.ParseDuration`: `300ms`, `1.5h`, `2h45m`, `8760h0m0s`; `0` needs no unit.
pub fn parse_go_duration(s: &str) -> Result<Duration, String> {
    let orig = s;
    let err = || format!("time: invalid duration {orig:?}");
    let s = s.strip_prefix('+').unwrap_or(s);
    if s.starts_with('-') {
        // Negative durations are not representable as std::time::Duration.
        return Err(format!("negative duration {orig:?} is not allowed"));
    }
    if s == "0" {
        return Ok(Duration::ZERO);
    }
    if s.is_empty() {
        return Err(err());
    }
    let mut rest = s;
    let mut total = 0f64;
    while !rest.is_empty() {
        let num_end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let num = &rest[..num_end];
        if num.is_empty() || num == "." {
            return Err(err());
        }
        let n: f64 = num.parse().map_err(|_| err())?;
        rest = &rest[num_end..];
        let unit_end = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let unit = &rest[..unit_end];
        if unit.is_empty() {
            return Err(format!("time: missing unit in duration {orig:?}"));
        }
        let scale = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return Err(format!("time: unknown unit {unit:?} in duration {orig:?}")),
        };
        total += n * scale;
        rest = &rest[unit_end..];
    }
    Ok(Duration::from_secs_f64(total))
}

/// `Duration.String` for whole seconds: `8760h0m0s`, `5m0s`, `30s`, `0s`.
pub fn go_duration_string(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_durations() {
        assert_eq!(parse_go_duration("8760h0m0s").unwrap(), Duration::from_secs(8760 * 3600));
        assert_eq!(parse_go_duration("1.5h").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse_go_duration("300ms").unwrap(), Duration::from_millis(300));
        assert_eq!(parse_go_duration("2h45m").unwrap(), Duration::from_secs(9900));
        assert_eq!(parse_go_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_go_duration("5").unwrap_err(), "time: missing unit in duration \"5\"");
        assert_eq!(parse_go_duration("1d").unwrap_err(), "time: unknown unit \"d\" in duration \"1d\"");
        assert_eq!(parse_go_duration("").unwrap_err(), "time: invalid duration \"\"");
        assert_eq!(go_duration_string(Duration::from_secs(8760 * 3600)), "8760h0m0s");
        assert_eq!(go_duration_string(Duration::from_secs(90)), "1m30s");
    }
}
