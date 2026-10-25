use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans() {
        assert_eq!(span(0), "now");
        assert_eq!(span(20), "1m");
        assert_eq!(span(3 * 3600 + 12 * 60), "3h 12m");
        assert_eq!(span(2 * 86_400 + 4 * 3600 + 5), "2d 4h");
    }
}
