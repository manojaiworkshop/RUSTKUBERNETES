//! Simplified port of `k8s.io/apimachinery/pkg/api/resource.Quantity`.
//!
//! Parses the common suffixes into milli-units (for CPU) or bytes (for memory):
//! `100m`, `0.5`, `2`, `128Mi`, `1Gi`, `500M`, `1e3`.

/// Parses a quantity string into a value scaled by 1000 ("milli" units).
/// `"250m"` → 250, `"1"` → 1000, `"1Ki"` → 1_024_000.
pub fn parse_milli(q: &str) -> Result<i128, String> {
    let q = q.trim();
    if q.is_empty() {
        return Err("quantity is empty".into());
    }
    let suffixes: &[(&str, i128, i128)] = &[
        // (suffix, multiplier numerator, divisor) applied to milli value
        ("Ki", 1 << 10, 1),
        ("Mi", 1 << 20, 1),
        ("Gi", 1 << 30, 1),
        ("Ti", 1 << 40, 1),
        ("Pi", 1 << 50, 1),
        ("Ei", 1 << 60, 1),
        ("n", 1, 1_000_000_000),
        ("u", 1, 1_000_000),
        ("m", 1, 1000),
        ("k", 1_000, 1),
        ("M", 1_000_000, 1),
        ("G", 1_000_000_000, 1),
        ("T", 1_000_000_000_000, 1),
        ("P", 1_000_000_000_000_000, 1),
        ("E", 1_000_000_000_000_000_000, 1),
    ];
    let (num, mul, div) = suffixes.iter().find_map(|(s, m, d)| q.strip_suffix(s).map(|n| (n, *m, *d))).unwrap_or((q, 1, 1));
    let base: f64 = num
        .parse()
        .map_err(|_| format!("quantities must match the regular expression '^([+-]?[0-9.]+)([eEinumkKMGTP]*[-+]?[0-9]*)$' (got {q:?})"))?;
    if base < 0.0 {
        return Err(format!("quantity must not be negative: {q}"));
    }
    Ok(((base * 1000.0) as i128) * mul / div)
}

/// Parses to whole units (rounding up), e.g. memory bytes.
pub fn parse_value(q: &str) -> Result<i128, String> {
    let m = parse_milli(q)?;
    Ok((m + 999) / 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantities() {
        assert_eq!(parse_milli("250m").unwrap(), 250);
        assert_eq!(parse_milli("1").unwrap(), 1000);
        assert_eq!(parse_milli("0.5").unwrap(), 500);
        assert_eq!(parse_value("128Mi").unwrap(), 128 * 1024 * 1024);
        assert_eq!(parse_value("1G").unwrap(), 1_000_000_000);
        assert!(parse_milli("abc").is_err());
    }
}
