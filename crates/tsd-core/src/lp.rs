//! InfluxDB Line Protocol パーサ。
//!
//! 仕様: <https://docs.influxdata.com/influxdb/v2/reference/syntax/line-protocol/>
//! - measurement のエスケープ対象: `,` と空白
//! - タグキー / タグ値 / フィールドキーのエスケープ対象: `,` `=` 空白
//! - 文字列フィールドのエスケープ対象: `"` と `\`

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    Float(f64),
    Int(i64),
    UInt(u64),
    Bool(bool),
    Str(String),
}

impl FieldValue {
    /// 数値として保存できる値を返す。文字列は None。
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            FieldValue::Float(v) => Some(*v),
            FieldValue::Int(v) => Some(*v as f64),
            FieldValue::UInt(v) => Some(*v as f64),
            FieldValue::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            FieldValue::Str(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub measurement: String,
    /// キー順にソート済み
    pub tags: Vec<(String, String)>,
    pub fields: Vec<(String, FieldValue)>,
    /// 指定精度での生の整数値
    pub timestamp: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LineError {
    /// 1 始まりの行番号
    pub line: usize,
    pub message: String,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Precision {
    Ns,
    Us,
    Ms,
    S,
    M,
    H,
    /// 桁数から判定する（InfluxDB 3 の `precision=auto` と同じ規則）
    Auto,
}

impl Precision {
    /// v1 (`n,u,ms,s,m,h`) と v2 (`ns,us,ms,s`) の両方の表記を受け付ける。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "" | "ns" | "n" => Precision::Ns,
            "us" | "u" | "µ" | "µs" => Precision::Us,
            "ms" => Precision::Ms,
            "s" => Precision::S,
            "m" => Precision::M,
            "h" => Precision::H,
            // InfluxDB 3 の /api/v3/write_lp の表記
            "auto" => Precision::Auto,
            "nanosecond" => Precision::Ns,
            "microsecond" => Precision::Us,
            "millisecond" => Precision::Ms,
            "second" => Precision::S,
            _ => return None,
        })
    }

    pub fn to_ms(self, v: i64) -> i64 {
        match self {
            Precision::Ns => v.div_euclid(1_000_000),
            Precision::Us => v.div_euclid(1_000),
            Precision::Ms => v,
            Precision::S => v.saturating_mul(1_000),
            Precision::M => v.saturating_mul(60_000),
            Precision::H => v.saturating_mul(3_600_000),
            Precision::Auto => {
                let a = v.unsigned_abs();
                if a < 5_000_000_000 {
                    Precision::S.to_ms(v)
                } else if a < 5_000_000_000_000 {
                    v
                } else if a < 5_000_000_000_000_000 {
                    Precision::Us.to_ms(v)
                } else {
                    Precision::Ns.to_ms(v)
                }
            }
        }
    }
}

/// 本文全体を解析する。正しい行と誤りのある行を両方返す（部分書き込み用）。
pub fn parse_lines(body: &str) -> (Vec<Line>, Vec<LineError>) {
    let mut lines = Vec::new();
    let mut errors = Vec::new();
    for (i, raw) in body.split('\n').enumerate() {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = raw.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        match parse_line(trimmed) {
            Ok(l) => lines.push(l),
            Err(message) => errors.push(LineError { line: i + 1, message }),
        }
    }
    (lines, errors)
}

struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }
    fn eof(&self) -> bool {
        self.pos >= self.b.len()
    }

    /// エスケープを考慮して、`stops` のどれかが現れるまで読む。
    fn read_escaped(&mut self, stops: &[u8], escapable: &[u8]) -> String {
        let mut out: Vec<u8> = Vec::new();
        while let Some(c) = self.peek() {
            if c == b'\\' {
                if let Some(&n) = self.b.get(self.pos + 1) {
                    if escapable.contains(&n) || n == b'\\' && escapable.contains(&b'\\') {
                        out.push(n);
                        self.pos += 2;
                        continue;
                    }
                }
                out.push(c);
                self.pos += 1;
                continue;
            }
            if stops.contains(&c) {
                break;
            }
            out.push(c);
            self.pos += 1;
        }
        // 区切りは ASCII なので UTF-8 境界は壊れない
        String::from_utf8(out).unwrap_or_default()
    }
}

pub fn parse_line(s: &str) -> Result<Line, String> {
    let mut c = Cursor { b: s.as_bytes(), pos: 0 };

    let measurement = c.read_escaped(b", ", b", ");
    if measurement.is_empty() {
        return Err("missing measurement".into());
    }

    let mut tags = Vec::new();
    while c.peek() == Some(b',') {
        c.pos += 1;
        let k = c.read_escaped(b"=, ", b",= ");
        if c.peek() != Some(b'=') {
            return Err(format!("tag '{k}' is missing '='"));
        }
        c.pos += 1;
        let v = c.read_escaped(b", ", b",= ");
        if k.is_empty() {
            return Err("empty tag key".into());
        }
        if v.is_empty() {
            return Err(format!("tag '{k}' has empty value"));
        }
        tags.push((k, v));
    }

    if c.peek() != Some(b' ') {
        return Err("missing fields".into());
    }
    while c.peek() == Some(b' ') {
        c.pos += 1;
    }

    let mut fields = Vec::new();
    loop {
        let k = c.read_escaped(b"=, ", b",= ");
        if k.is_empty() {
            return Err("empty field key".into());
        }
        if c.peek() != Some(b'=') {
            return Err(format!("field '{k}' is missing '='"));
        }
        c.pos += 1;
        let v = parse_field_value(&mut c).map_err(|e| format!("field '{k}': {e}"))?;
        fields.push((k, v));
        match c.peek() {
            Some(b',') => {
                c.pos += 1;
                continue;
            }
            _ => break,
        }
    }

    let mut timestamp = None;
    if c.peek() == Some(b' ') {
        while c.peek() == Some(b' ') {
            c.pos += 1;
        }
        let rest = std::str::from_utf8(&c.b[c.pos..]).unwrap_or("").trim_end();
        if !rest.is_empty() {
            timestamp = Some(
                rest.parse::<i64>()
                    .map_err(|_| format!("invalid timestamp '{rest}'"))?,
            );
        }
        c.pos = c.b.len();
    }
    if !c.eof() {
        return Err("unexpected characters after fields".into());
    }

    tags.sort_by(|a, b| a.0.cmp(&b.0));
    for w in tags.windows(2) {
        if w[0].0 == w[1].0 {
            return Err(format!("duplicate tag key '{}'", w[0].0));
        }
    }

    Ok(Line { measurement, tags, fields, timestamp })
}

fn parse_field_value(c: &mut Cursor) -> Result<FieldValue, String> {
    if c.peek() == Some(b'"') {
        c.pos += 1;
        let mut out = Vec::new();
        loop {
            match c.peek() {
                None => return Err("unterminated string".into()),
                Some(b'\\') => {
                    match c.b.get(c.pos + 1) {
                        Some(&n) if n == b'"' || n == b'\\' => {
                            out.push(n);
                            c.pos += 2;
                        }
                        _ => {
                            out.push(b'\\');
                            c.pos += 1;
                        }
                    }
                }
                Some(b'"') => {
                    c.pos += 1;
                    break;
                }
                Some(ch) => {
                    out.push(ch);
                    c.pos += 1;
                }
            }
        }
        return Ok(FieldValue::Str(String::from_utf8(out).unwrap_or_default()));
    }

    let start = c.pos;
    while let Some(ch) = c.peek() {
        if ch == b',' || ch == b' ' {
            break;
        }
        c.pos += 1;
    }
    let tok = std::str::from_utf8(&c.b[start..c.pos]).unwrap_or("");
    if tok.is_empty() {
        return Err("empty value".into());
    }
    match tok {
        "t" | "T" | "true" | "True" | "TRUE" => return Ok(FieldValue::Bool(true)),
        "f" | "F" | "false" | "False" | "FALSE" => return Ok(FieldValue::Bool(false)),
        _ => {}
    }
    if let Some(n) = tok.strip_suffix('i') {
        return n
            .parse::<i64>()
            .map(FieldValue::Int)
            .map_err(|_| format!("invalid integer '{tok}'"));
    }
    if let Some(n) = tok.strip_suffix('u') {
        return n
            .parse::<u64>()
            .map(FieldValue::UInt)
            .map_err(|_| format!("invalid unsigned integer '{tok}'"));
    }
    // Rust の f64 パーサは "inf" や "NaN" も受け付けるので先に弾く
    let first = tok.as_bytes()[0];
    if !(first.is_ascii_digit() || first == b'-' || first == b'+' || first == b'.') {
        return Err(format!("invalid number '{tok}'"));
    }
    let v: f64 = tok.parse().map_err(|_| format!("invalid number '{tok}'"))?;
    if !v.is_finite() {
        return Err(format!("non-finite number '{tok}'"));
    }
    Ok(FieldValue::Float(v))
}

// ---- 系列キー -------------------------------------------------------------

fn escape(s: &str, chars: &[char]) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if chars.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// 正規化した系列キー: `measurement,tagA=x,tagB=y field`（タグはキー順、LP と同じエスケープ）
pub fn series_key(measurement: &str, sorted_tags: &[(String, String)], field: &str) -> String {
    let mut k = escape(measurement, &[',', ' ']);
    for (tk, tv) in sorted_tags {
        k.push(',');
        k.push_str(&escape(tk, &[',', '=', ' ']));
        k.push('=');
        k.push_str(&escape(tv, &[',', '=', ' ']));
    }
    k.push(' ');
    k.push_str(&escape(field, &[',', '=', ' ']));
    k
}

/// 系列キーを分解する（`series_key` の逆）。
pub fn parse_series_key(key: &str) -> Option<(String, Vec<(String, String)>, String)> {
    let line = parse_line(&format!("{key}=0")).ok()?;
    let (field, _) = line.fields.into_iter().next()?;
    Some((line.measurement, line.tags, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        let l = parse_line("cpu,host=a,region=jp usage=1.5,count=3i,ok=t 1700000000000000000").unwrap();
        assert_eq!(l.measurement, "cpu");
        assert_eq!(l.tags, vec![("host".into(), "a".into()), ("region".into(), "jp".into())]);
        assert_eq!(l.fields[0], ("usage".into(), FieldValue::Float(1.5)));
        assert_eq!(l.fields[1], ("count".into(), FieldValue::Int(3)));
        assert_eq!(l.fields[2], ("ok".into(), FieldValue::Bool(true)));
        assert_eq!(l.timestamp, Some(1_700_000_000_000_000_000));
    }

    #[test]
    fn tags_are_sorted_and_no_timestamp() {
        let l = parse_line("m,z=1,a=2 v=1").unwrap();
        assert_eq!(l.tags[0].0, "a");
        assert_eq!(l.timestamp, None);
    }

    #[test]
    fn escapes() {
        let l = parse_line(r#"my\ meas,ta\,g=v\ a\=l fi\ eld="he said \"hi\" \\" 1"#).unwrap();
        assert_eq!(l.measurement, "my meas");
        assert_eq!(l.tags, vec![("ta,g".into(), "v a=l".into())]);
        assert_eq!(l.fields[0].0, "fi eld");
        assert_eq!(l.fields[0].1, FieldValue::Str(r#"he said "hi" \"#.into()));
    }

    #[test]
    fn string_with_comma_and_space() {
        let l = parse_line(r#"m s="a, b c",v=2u"#).unwrap();
        assert_eq!(l.fields[0].1, FieldValue::Str("a, b c".into()));
        assert_eq!(l.fields[1].1, FieldValue::UInt(2));
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_line("m v=-1.5e3").unwrap().fields[0].1, FieldValue::Float(-1500.0));
        assert_eq!(parse_line("m v=.5").unwrap().fields[0].1, FieldValue::Float(0.5));
        assert!(parse_line("m v=NaN").is_err());
        assert!(parse_line("m v=inf").is_err());
        assert!(parse_line("m v=1x").is_err());
        assert!(parse_line("m v=").is_err());
    }

    #[test]
    fn errors() {
        assert!(parse_line("m").is_err());
        assert!(parse_line("m,t v=1").is_err());
        assert!(parse_line("m,t= v=1").is_err());
        assert!(parse_line("m v=1 abc").is_err());
        assert!(parse_line("m,a=1,a=2 v=1").is_err());
        assert!(parse_line(r#"m v="abc"#).is_err());
    }

    #[test]
    fn multi_lines_with_errors() {
        let body = "# comment\nm v=1 1\n\nbad\r\nm v=2 2\r\n";
        let (ok, err) = parse_lines(body);
        assert_eq!(ok.len(), 2);
        assert_eq!(err.len(), 1);
        assert_eq!(err[0].line, 4);
    }

    #[test]
    fn utf8() {
        let l = parse_line("温度,装置=炉1 値=21.5").unwrap();
        assert_eq!(l.measurement, "温度");
        assert_eq!(l.tags[0], ("装置".into(), "炉1".into()));
    }

    #[test]
    fn series_key_roundtrip() {
        let tags = vec![("a b".to_string(), "x,y".to_string()), ("c".to_string(), "=".to_string())];
        let k = series_key("me as", &tags, "f=1");
        let (m, t, f) = parse_series_key(&k).unwrap();
        assert_eq!(m, "me as");
        assert_eq!(t, tags);
        assert_eq!(f, "f=1");
    }

    #[test]
    fn precision() {
        assert_eq!(Precision::parse("s").unwrap().to_ms(2), 2000);
        assert_eq!(Precision::Ns.to_ms(1_999_999), 1);
        assert_eq!(Precision::Ns.to_ms(-1), -1);
        assert!(Precision::parse("x").is_none());
        let a = Precision::parse("auto").unwrap();
        for v in [1_700_000_000i64, 1_700_000_000_000, 1_700_000_000_000_000, 1_700_000_000_000_000_000] {
            assert_eq!(a.to_ms(v), 1_700_000_000_000);
        }
    }
}
