// SPDX-License-Identifier: GPL-3.0-or-later
//! Wall-clock timestamps as used on D-Bus: unix time in microseconds (`t`), 0 = unset.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn now_us() -> u64 {
    to_us(SystemTime::now())
}

pub fn to_us(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_micros() as u64)
}

/// Time left until `deadline_us`, saturating at zero. `None` when unset (0).
pub fn remaining(deadline_us: u64) -> Option<Duration> {
    if deadline_us == 0 {
        return None;
    }
    Some(Duration::from_micros(deadline_us.saturating_sub(now_us())))
}

/// `7:42` style countdown, rounding partial seconds up.
pub fn format_countdown(d: Duration) -> String {
    let secs = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Parses `90`, `90s`, `10m`, `1h` into a duration.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (digits, mult) = match s.as_bytes().last()? {
        b's' => (&s[..s.len() - 1], 1),
        b'm' => (&s[..s.len() - 1], 60),
        b'h' => (&s[..s.len() - 1], 3600),
        _ => (s, 1),
    };
    let n: u64 = digits.parse().ok()?;
    Some(Duration::from_secs(n.checked_mul(mult)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration("90"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("15s"), Some(Duration::from_secs(15)));
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_duration("-1"), None);
    }

    #[test]
    fn countdown_format() {
        assert_eq!(format_countdown(Duration::from_secs(462)), "7:42");
        assert_eq!(format_countdown(Duration::from_secs(5)), "0:05");
        assert_eq!(format_countdown(Duration::from_millis(89_990)), "1:30");
    }
}
