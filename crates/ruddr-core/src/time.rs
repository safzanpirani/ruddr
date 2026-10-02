//! RFC 3339 timestamps without a date-time dependency. Ruddr writes UTC with
//! fractional seconds trimmed the way Go's RFC3339Nano does, and reads any
//! offset so state written by older releases still parses.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The current time as RFC 3339 UTC, e.g. `2026-10-02T09:19:44.096468Z`.
pub fn now_rfc3339() -> String {
    format_rfc3339(SystemTime::now())
}

pub fn format_rfc3339(time: SystemTime) -> String {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let secs = since.as_secs() as i64;
    let nanos = since.subsec_nanos();
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let (year, month, day) = civil_from_days(days);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let mut out = format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}");
    if nanos > 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    out.push('Z');
    out
}

/// Parses RFC 3339 into Unix milliseconds. Go's zero time and malformed
/// values return `None`.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b't' || b[10] == b' ') || b[13] != b':' || b[16] != b':'
    {
        return None;
    }
    let num = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if year <= 1 || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.find(|c: char| !c.is_ascii_digit()).unwrap_or(frac.len());
        let padded = format!("{:0<3}", &frac[..digits.min(3)]);
        millis = padded.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            sign * (rest.get(1..3)?.parse::<i64>().ok()? * 3600 + rest.get(4..6)?.parse::<i64>().ok()? * 60)
        }
    };
    let seconds = days_from_civil(year, month, day) * 86400 + h * 3600 + mi * 60 + s - offset;
    Some(seconds * 1000 + millis)
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Howard Hinnant's civil calendar algorithms.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let t = UNIX_EPOCH + Duration::new(1_791_000_000, 96_468_000);
        let text = format_rfc3339(t);
        assert_eq!(text, "2026-10-03T04:00:00.096468Z");
        assert_eq!(parse_rfc3339_ms(&text), Some(1_791_000_000_096));
    }

    #[test]
    fn parses_offsets_and_rejects_go_zero_time() {
        assert_eq!(
            parse_rfc3339_ms("2026-10-02T15:00:00+05:30"),
            parse_rfc3339_ms("2026-10-02T09:30:00Z")
        );
        assert_eq!(parse_rfc3339_ms("0001-01-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_ms("garbage"), None);
    }
}
