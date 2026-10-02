//! Go duration syntax (`3600s`, `20m`, `1h30m`, `500ms`). Bare integers stay
//! invalid so a typo never means nanoseconds.

use std::time::Duration;

pub fn parse(text: &str) -> Result<Duration, String> {
    let original = text;
    let text = text.trim();
    if text == "0" {
        return Ok(Duration::ZERO);
    }
    if text.is_empty() {
        return Err("empty duration".into());
    }
    let mut rest = text;
    let mut total = 0f64;
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        if digits == 0 {
            return Err(format!("invalid duration {original:?}"));
        }
        let value: f64 = rest[..digits].parse().map_err(|_| format!("invalid duration {original:?}"))?;
        rest = &rest[digits..];
        let unit_len = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let unit = &rest[..unit_len];
        rest = &rest[unit_len..];
        let seconds = match unit {
            "ns" => 1e-9,
            "us" | "µs" | "μs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "" => return Err(format!("missing unit in duration {original:?}")),
            other => return Err(format!("unknown unit {other:?} in duration {original:?}")),
        };
        total += value * seconds;
    }
    Ok(Duration::from_secs_f64(total))
}

/// Formats like Go's `time.Duration.String` for whole seconds and up.
pub fn format(duration: Duration) -> String {
    let total = duration.as_secs();
    if total == 0 {
        return if duration.is_zero() {
            "0s".into()
        } else {
            format!("{}ms", duration.as_millis())
        };
    }
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    let mut out = String::new();
    if h > 0 {
        out.push_str(&format!("{h}h"));
    }
    if h > 0 || m > 0 {
        out.push_str(&format!("{m}m"));
    }
    out.push_str(&format!("{s}s"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_syntax() {
        assert_eq!(parse("3600s").unwrap(), Duration::from_secs(3600));
        assert_eq!(parse("1h30m").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse("1.5h").unwrap(), Duration::from_secs(5400));
        assert_eq!(parse("0").unwrap(), Duration::ZERO);
    }

    #[test]
    fn rejects_bare_integers_and_garbage() {
        assert!(parse("30").is_err());
        assert!(parse("").is_err());
        assert!(parse("5x").is_err());
        assert!(parse("m").is_err());
    }

    #[test]
    fn formats_like_go() {
        assert_eq!(format(Duration::from_secs(4 * 3600)), "4h0m0s");
        assert_eq!(format(Duration::from_secs(90)), "1m30s");
        assert_eq!(format(Duration::from_secs(5)), "5s");
    }
}
