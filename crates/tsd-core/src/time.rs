//! UTC エポックミリ秒を扱う小さなヘルパー。

use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};

pub const SEC: i64 = 1_000;
pub const MIN: i64 = 60_000;
pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 86_400_000;

pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

pub fn floor_to(ms: i64, unit: i64) -> i64 {
    ms.div_euclid(unit) * unit
}

pub fn to_dt(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

pub fn month_start(ms: i64) -> i64 {
    let d = to_dt(ms);
    Utc.with_ymd_and_hms(d.year(), d.month(), 1, 0, 0, 0)
        .single()
        .map(|d| d.timestamp_millis())
        .unwrap_or(ms)
}

pub fn next_month(ms: i64) -> i64 {
    let d = to_dt(month_start(ms));
    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0)
        .single()
        .map(|d| d.timestamp_millis())
        .unwrap_or(ms)
}

/// `YYYY-MM-DD`
pub fn day_str(ms: i64) -> String {
    to_dt(ms).format("%Y-%m-%d").to_string()
}

/// `YYYY-MM`
pub fn month_str(ms: i64) -> String {
    to_dt(ms).format("%Y-%m").to_string()
}

pub fn parse_day(s: &str) -> Option<i64> {
    let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?;
    Some(d.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis())
}

pub fn parse_month(s: &str) -> Option<i64> {
    parse_day(&format!("{s}-01"))
}

pub fn iso(ms: i64) -> String {
    to_dt(ms).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// クエリ文字列の時刻: エポックミリ秒、RFC3339、または `now`, `now-1h` などの相対表記。
pub fn parse_time(s: &str, now: i64) -> Option<i64> {
    let s = s.trim();
    if let Ok(v) = s.parse::<i64>() {
        return Some(v);
    }
    if let Some(rest) = s.strip_prefix("now") {
        if rest.is_empty() {
            return Some(now);
        }
        let (sign, num) = match rest.as_bytes()[0] {
            b'-' => (-1, &rest[1..]),
            b'+' => (1, &rest[1..]),
            _ => return None,
        };
        return parse_duration(num).map(|d| now + sign * d);
    }
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

/// `30s`, `5m`, `1h`, `7d`, `1y` など。
pub fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let idx = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(idx);
    let n: i64 = n.parse().ok()?;
    let mul = match unit {
        "ms" => 1,
        "s" => SEC,
        "m" => MIN,
        "h" => HOUR,
        "d" => DAY,
        "w" => 7 * DAY,
        "M" => 30 * DAY,
        "y" => 365 * DAY,
        _ => return None,
    };
    Some(n * mul)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months() {
        let t = parse_day("2026-12-15").unwrap() + 5 * HOUR;
        assert_eq!(month_start(t), parse_day("2026-12-01").unwrap());
        assert_eq!(next_month(t), parse_day("2027-01-01").unwrap());
        assert_eq!(month_str(t), "2026-12");
        assert_eq!(day_str(t), "2026-12-15");
    }

    #[test]
    fn rel_time() {
        assert_eq!(parse_time("now-1h", 10 * HOUR), Some(9 * HOUR));
        assert_eq!(parse_time("1234", 0), Some(1234));
        assert_eq!(parse_time("1970-01-01T00:00:01Z", 0), Some(1000));
        assert_eq!(parse_duration("7d"), Some(7 * DAY));
    }
}
