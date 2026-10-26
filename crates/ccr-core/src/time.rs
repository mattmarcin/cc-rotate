use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The latest epoch second a reading may name (the year 3000): anything later, negative, or not a
/// number is not a time, and arithmetic on it in milliseconds could overflow.
const MAX_EPOCH_S: f64 = 32_503_680_000.0;

/// Epoch seconds from a number Claude Code or the API reported, if it is a plausible time.
pub fn epoch_s(v: f64) -> Option<i64> {
    (v.is_finite() && (0.0..=MAX_EPOCH_S).contains(&v)).then_some(v as i64)
}

/// Epoch milliseconds the same way.
pub fn epoch_ms(v: f64) -> Option<i64> {
    (v.is_finite() && (0.0..=MAX_EPOCH_S * 1000.0).contains(&v)).then_some(v as i64)
}

/// Seconds since the Unix epoch.
pub fn now_s() -> i64 {
    now_ms() / 1000
}

/// A duration for people: "3h 12m", "45m", "2d 4h"; "now" when it is not positive.
pub fn span(secs: i64) -> String {
    if secs <= 0 {
        return "now".into();
    }
    let (d, h, m) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60);
    match (d, h) {
        (0, 0) => format!("{}m", m.max(1)),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// An epoch-seconds time in the local time zone, formatted with `strftime` syntax
/// ("%H:%M:%S", "%a %b %-d").
pub fn local(ts_s: i64, format: &str) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(ts_s, 0).single() {
        Some(t) => t.format(format).to_string(),
        None => "-".into(),
    }
}

/// Epoch seconds from an RFC 3339 timestamp (`2026-10-24T18:00:00Z`, with an optional fraction
/// and offset), the form Claude Code's status line and usage readings use.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d, h, mi, se) = (
        n(0..4)?,
        n(5..7)?,
        n(8..10)?,
        n(11..13)?,
        n(14..16)?,
        n(17..19)?,
    );
    let mut rest = &s[19..];
    if let Some(stripped) = rest.strip_prefix('.') {
        let digits = stripped
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(stripped.len());
        rest = &stripped[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        o if o.len() == 6 && (o.starts_with('+') || o.starts_with('-')) => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            let part = |r: std::ops::Range<usize>| o.get(r)?.parse::<i64>().ok();
            sign * (part(1..3)? * 3600 + part(4..6)? * 60)
        }
        _ => return None,
    };
    // Days from civil (Howard Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se - offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plausible_epoch_times_are_read() {
        assert_eq!(epoch_s(1_800_000_000.7), Some(1_800_000_000));
        for v in [f64::NAN, f64::INFINITY, -1.0, 1e30] {
            assert_eq!(epoch_s(v), None, "{v}");
            assert_eq!(epoch_ms(v), None, "{v}");
        }
        assert_eq!(epoch_ms(1_800_000_000_000.0), Some(1_800_000_000_000));
    }

    #[test]
    fn spans() {
        assert_eq!(span(0), "now");
        assert_eq!(span(20), "1m");
        assert_eq!(span(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(span(2 * 86_400 + 4 * 3600 + 5), "2d 4h");
    }

    #[test]
    fn rfc3339_offsets() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(parse_rfc3339("2000-03-01T00:00:00.123Z"), Some(951_868_800));
        // The usage endpoint's form: microseconds and a numeric offset.
        assert_eq!(
            parse_rfc3339("2026-10-25T05:00:00.147485+00:00"),
            parse_rfc3339("2026-10-25T05:00:00Z")
        );
        assert_eq!(parse_rfc3339("garbage"), None);
        assert_eq!(
            parse_rfc3339("2026-10-25T05:00:00+\u{e9}9:0"),
            None,
            "no panic"
        );
    }
}
