//! Grafana 向けの Prometheus 互換 API（読み取りのみ）。
//!
//! Grafana 9 の標準 Prometheus データソースから、URL に `http://<host>:8086` を指定して使う。
//!
//! 系列とラベルの対応:
//! - `__name__`      = field
//! - `_measurement`  = measurement
//! - その他          = タグ
//!
//! 使える PromQL（一部）:
//! - セレクタ: `temp{device="dev01"}`, `{__name__="温度", _measurement=~"plc|line"}`
//! - `avg_over_time(sel[$__interval])` / `min_over_time(...)` / `max_over_time(...)`
//!
//! 範囲クエリは各ステップ幅のバケットに集計して返す。セレクタだけなら平均、
//! `min_over_time`/`max_over_time` なら最小・最大。`[...]` の幅は無視してステップ幅を使う。

use crate::SharedState;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tsd_core::time::{now_ms, parse_duration, MIN};
use tsd_core::{AggQuery, Series, SeriesFilter, SeriesId};

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/api/v1/query_range", get(query_range).post(query_range))
        .route("/api/v1/query", get(query).post(query))
        .route("/api/v1/labels", get(labels).post(labels))
        .route("/api/v1/label/{name}/values", get(label_values))
        .route("/api/v1/series", get(series).post(series))
        .route("/api/v1/metadata", get(|| async { ok(json!({})) }))
        .route("/api/v1/query_exemplars", get(|| async { ok(json!([])) }).post(|| async { ok(json!([])) }))
        .route(
            "/api/v1/status/buildinfo",
            get(|| async { ok(json!({ "version": "2.40.0", "revision": "tsd", "branch": "tsd" })) }),
        )
}

fn ok(data: Value) -> Response {
    Json(json!({ "status": "success", "data": data })).into_response()
}

fn err(msg: impl Into<String>) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "status": "error", "errorType": "bad_data", "error": msg.into() }))).into_response()
}

/// クエリ文字列とフォーム本文（POST）の両方から引数を集める。`match[]` は複数可。
fn params(uri: &Uri, body: &Bytes) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = serde_urlencoded::from_str(uri.query().unwrap_or("")).unwrap_or_default();
    if let Ok(s) = std::str::from_utf8(body) {
        v.extend(serde_urlencoded::from_str::<Vec<(String, String)>>(s).unwrap_or_default());
    }
    v
}

fn get_param<'a>(p: &'a [(String, String)], k: &str) -> Option<&'a str> {
    p.iter().find(|(a, _)| a == k).map(|(_, b)| b.as_str())
}

/// 秒（小数可）か RFC3339 → ミリ秒
fn parse_ts(s: &str) -> Option<i64> {
    if let Ok(f) = s.parse::<f64>() {
        return Some((f * 1000.0).round() as i64);
    }
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis())
}

/// 秒（小数可）か `15s` などの期間 → ミリ秒
fn parse_step(s: &str) -> Option<i64> {
    if let Ok(f) = s.parse::<f64>() {
        return Some((f * 1000.0).round() as i64);
    }
    parse_duration(s)
}

// ---- PromQL（一部） ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Agg {
    Avg,
    Min,
    Max,
}

#[derive(Debug)]
pub enum Expr {
    Number(f64),
    Select { agg: Agg, matchers: Vec<Matcher>, range_ms: Option<i64> },
}

#[derive(Debug)]
pub struct Matcher {
    pub label: String,
    pub op: Op,
    pub value: String,
    re: Option<Regex>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Eq,
    Ne,
    Re,
    Nre,
}

impl Matcher {
    fn new(label: String, op: Op, value: String) -> Result<Self, String> {
        let re = match op {
            Op::Re | Op::Nre => Some(Regex::new(&format!("^(?:{value})$")).map_err(|e| format!("正規表現が不正です: {e}"))?),
            _ => None,
        };
        Ok(Self { label, op, value, re })
    }
    fn matches(&self, v: &str) -> bool {
        match self.op {
            Op::Eq => v == self.value,
            Op::Ne => v != self.value,
            Op::Re => self.re.as_ref().unwrap().is_match(v),
            Op::Nre => !self.re.as_ref().unwrap().is_match(v),
        }
    }
}

struct P<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        while let Some(c) = self.s[self.i..].chars().next() {
            if c.is_whitespace() {
                self.i += c.len_utf8();
            } else {
                break;
            }
        }
    }
    fn peek(&mut self) -> Option<char> {
        self.ws();
        self.s[self.i..].chars().next()
    }
    fn eat(&mut self, t: &str) -> bool {
        self.ws();
        if self.s[self.i..].starts_with(t) {
            self.i += t.len();
            true
        } else {
            false
        }
    }
    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        for c in self.s[self.i..].chars() {
            if c.is_alphanumeric() || c == '_' || c == ':' {
                self.i += c.len_utf8();
            } else {
                break;
            }
        }
        (self.i > start).then(|| self.s[start..self.i].to_string())
    }
    fn string(&mut self) -> Result<String, String> {
        self.ws();
        let q = self.s[self.i..].chars().next().ok_or("文字列がありません")?;
        if q != '"' && q != '\'' && q != '`' {
            return Err(format!("文字列が必要です（位置 {}）", self.i));
        }
        self.i += 1;
        let mut out = String::new();
        let mut chars = self.s[self.i..].char_indices();
        while let Some((k, c)) = chars.next() {
            if c == q {
                self.i += k + 1;
                return Ok(out);
            }
            if c == '\\' && q != '`' {
                if let Some((_, n)) = chars.next() {
                    match n {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        // 正規表現のエスケープ（\. など）は残す
                        '"' | '\'' | '\\' => out.push(n),
                        _ => {
                            out.push('\\');
                            out.push(n);
                        }
                    }
                }
                continue;
            }
            out.push(c);
        }
        Err("文字列が閉じていません".into())
    }
}

pub fn parse(q: &str) -> Result<Expr, String> {
    let q = q.trim();
    // Grafana の接続テスト（1+1）と数値
    if let Ok(n) = q.parse::<f64>() {
        return Ok(Expr::Number(n));
    }
    if let Some((a, b)) = q.split_once('+') {
        if let (Ok(a), Ok(b)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
            return Ok(Expr::Number(a + b));
        }
    }
    let mut p = P { s: q, i: 0 };
    let mut agg = Agg::Avg;
    let mut wrapped = false;
    let save = p.i;
    if let Some(id) = p.ident() {
        let f = match id.as_str() {
            "avg_over_time" => Some(Agg::Avg),
            "min_over_time" => Some(Agg::Min),
            "max_over_time" => Some(Agg::Max),
            _ => None,
        };
        if let (Some(f), true) = (f, p.peek() == Some('(')) {
            p.eat("(");
            agg = f;
            wrapped = true;
        } else {
            p.i = save;
        }
    }
    let (matchers, range_ms) = selector(&mut p)?;
    if wrapped && !p.eat(")") {
        return Err("')' がありません".into());
    }
    p.ws();
    if p.i != q.len() {
        return Err(format!(
            "対応していない式です（位置 {}）。使えるのはセレクタと avg/min/max_over_time だけです",
            p.i
        ));
    }
    if matchers.is_empty() {
        return Err("系列を指定してください".into());
    }
    Ok(Expr::Select { agg, matchers, range_ms })
}

fn selector(p: &mut P) -> Result<(Vec<Matcher>, Option<i64>), String> {
    let mut ms = vec![];
    if p.peek() != Some('{') {
        let name = p.ident().ok_or("系列名か '{' が必要です")?;
        ms.push(Matcher::new("__name__".into(), Op::Eq, name)?);
    }
    if p.eat("{") {
        loop {
            if p.eat("}") {
                break;
            }
            let label = match p.peek() {
                Some('"') | Some('\'') => p.string()?,
                _ => p.ident().ok_or("ラベル名が必要です")?,
            };
            let op = if p.eat("=~") {
                Op::Re
            } else if p.eat("!~") {
                Op::Nre
            } else if p.eat("!=") {
                Op::Ne
            } else if p.eat("=") {
                Op::Eq
            } else {
                return Err(format!("'{label}' の後に =, !=, =~, !~ が必要です"));
            };
            let value = p.string()?;
            ms.push(Matcher::new(label, op, value)?);
            if p.eat(",") {
                continue;
            }
            if !p.eat("}") {
                return Err("'}' がありません".into());
            }
            break;
        }
    }
    let mut range = None;
    if p.eat("[") {
        let start = p.i;
        let end = p.s[start..].find(']').ok_or("']' がありません")? + start;
        let d = p.s[start..end].trim();
        range = Some(parse_duration(d).ok_or_else(|| format!("期間が不正です: {d}"))?);
        p.i = end + 1;
    }
    Ok((ms, range))
}

// ---- 系列の照合 -------------------------------------------------------------

fn label_value<'a>(s: &'a Series, label: &str) -> &'a str {
    match label {
        "__name__" => &s.field,
        "_measurement" => &s.measurement,
        k => s.tags.get(k).map(String::as_str).unwrap_or(""),
    }
}

fn labels_of(s: &Series) -> BTreeMap<String, String> {
    let mut m: BTreeMap<String, String> = s.tags.clone().into_iter().collect();
    m.insert("__name__".into(), s.field.clone());
    m.insert("_measurement".into(), s.measurement.clone());
    m
}

fn select(st: &SharedState, ms: &[Matcher]) -> Result<Vec<Series>, String> {
    let all = st.store.list_series(&SeriesFilter::default()).map_err(|e| e.to_string())?;
    Ok(all.into_iter().filter(|s| ms.iter().all(|m| m.matches(label_value(s, &m.label)))).collect())
}

fn fmt_val(v: f32) -> String {
    format!("{v}")
}

const MAX_SERIES: usize = 500;

// ---- ハンドラ ---------------------------------------------------------------

async fn query_range(State(st): State<SharedState>, uri: Uri, body: Bytes) -> Response {
    let p = params(&uri, &body);
    let (Some(q), Some(start), Some(end), Some(step)) = (
        get_param(&p, "query"),
        get_param(&p, "start").and_then(parse_ts),
        get_param(&p, "end").and_then(parse_ts),
        get_param(&p, "step").and_then(parse_step),
    ) else {
        return err("query, start, end, step が必要です");
    };
    if end < start || step <= 0 {
        return err("start, end, step が不正です");
    }
    let expr = match parse(q) {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    match expr {
        Expr::Number(n) => {
            let mut vals = vec![];
            let mut t = start;
            while t <= end && vals.len() < 11000 {
                vals.push(json!([t as f64 / 1000.0, n.to_string()]));
                t += step;
            }
            ok(json!({ "resultType": "matrix", "result": [{ "metric": {}, "values": vals }] }))
        }
        Expr::Select { agg, matchers, .. } => {
            let list = match select(&st, &matchers) {
                Ok(l) => l,
                Err(e) => return err(e),
            };
            if list.len() > MAX_SERIES {
                return err(format!("一致する系列が多すぎます（{} 件、上限 {MAX_SERIES}）", list.len()));
            }
            // ステップ幅のバケットになるよう幅（点数）を決める
            let width = ((end - start + step - 1) / step).clamp(1, 10_000) as u32;
            let aq = AggQuery {
                series: list.iter().map(|s| s.id).collect(),
                from_ms: start,
                to_ms: end + 1,
                width,
                tier: None,
            };
            let store = st.store.clone();
            let r = match tokio::task::spawn_blocking(move || store.query_agg(&aq)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => return err(format!("{e:#}")),
                Err(e) => return err(e.to_string()),
            };
            let by_id: BTreeMap<SeriesId, &Series> = list.iter().map(|s| (s.id, s)).collect();
            let mut result = vec![];
            for s in &r.series {
                let col = match agg {
                    Agg::Avg => &s.avg,
                    Agg::Min => &s.min,
                    Agg::Max => &s.max,
                };
                let vals: Vec<Value> = s
                    .t
                    .iter()
                    .zip(col)
                    .filter_map(|(t, v)| v.map(|v| json!([*t as f64 / 1000.0, fmt_val(v)])))
                    .collect();
                if vals.is_empty() {
                    continue;
                }
                // Prometheus と違い、関数を通しても __name__ を残す（凡例に項目名を出せるように）
                let metric = labels_of(by_id[&s.id]);
                result.push(json!({ "metric": metric, "values": vals }));
            }
            ok(json!({ "resultType": "matrix", "result": result }))
        }
    }
}

async fn query(State(st): State<SharedState>, uri: Uri, body: Bytes) -> Response {
    let p = params(&uri, &body);
    let Some(q) = get_param(&p, "query") else { return err("query が必要です") };
    let t = get_param(&p, "time").and_then(parse_ts).unwrap_or_else(now_ms);
    let expr = match parse(q) {
        Ok(e) => e,
        Err(e) => return err(e),
    };
    match expr {
        Expr::Number(n) => ok(json!({ "resultType": "scalar", "result": [t as f64 / 1000.0, n.to_string()] })),
        Expr::Select { agg, matchers, range_ms } => {
            let list = match select(&st, &matchers) {
                Ok(l) => l,
                Err(e) => return err(e),
            };
            if list.len() > MAX_SERIES {
                return err(format!("一致する系列が多すぎます（{} 件）", list.len()));
            }
            let ids: Vec<SeriesId> = list.iter().map(|s| s.id).collect();
            let mut result = vec![];
            if range_ms.is_none() {
                // 最新値（5分以内）
                let lat = st.store.latest(&ids).unwrap_or_default();
                let by_id: BTreeMap<SeriesId, &Series> = list.iter().map(|s| (s.id, s)).collect();
                for l in lat {
                    if l.ts <= t && t - l.ts <= 5 * MIN {
                        result.push(json!({ "metric": labels_of(by_id[&l.id]), "value": [l.ts as f64 / 1000.0, fmt_val(l.value)] }));
                    }
                }
            } else {
                let d = range_ms.unwrap();
                let aq = AggQuery { series: ids, from_ms: t - d, to_ms: t, width: 1, tier: None };
                let store = st.store.clone();
                let r = match tokio::task::spawn_blocking(move || store.query_agg(&aq)).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => return err(format!("{e:#}")),
                    Err(e) => return err(e.to_string()),
                };
                let by_id: BTreeMap<SeriesId, &Series> = list.iter().map(|s| (s.id, s)).collect();
                for s in &r.series {
                    let vals: Vec<f32> = match agg {
                        Agg::Avg => s.avg.iter().flatten().copied().collect(),
                        Agg::Min => s.min.iter().flatten().copied().collect(),
                        Agg::Max => s.max.iter().flatten().copied().collect(),
                    };
                    let v = match agg {
                        Agg::Min => vals.iter().copied().reduce(f32::min),
                        Agg::Max => vals.iter().copied().reduce(f32::max),
                        Agg::Avg => vals.last().copied(),
                    };
                    if let Some(v) = v {
                        let metric = labels_of(by_id[&s.id]);
                        result.push(json!({ "metric": metric, "value": [t as f64 / 1000.0, fmt_val(v)] }));
                    }
                }
            }
            ok(json!({ "resultType": "vector", "result": result }))
        }
    }
}

/// `match[]` に一致する系列（無ければ全系列）
fn matched(st: &SharedState, p: &[(String, String)]) -> Result<Vec<Series>, String> {
    let sels: Vec<&str> = p.iter().filter(|(k, _)| k == "match[]").map(|(_, v)| v.as_str()).collect();
    if sels.is_empty() {
        return st.store.list_series(&SeriesFilter::default()).map_err(|e| e.to_string());
    }
    let mut out: BTreeMap<SeriesId, Series> = BTreeMap::new();
    for s in sels {
        let mut pp = P { s, i: 0 };
        let (ms, _) = selector(&mut pp)?;
        for x in select(st, &ms)? {
            out.insert(x.id, x);
        }
    }
    Ok(out.into_values().collect())
}

async fn labels(State(st): State<SharedState>, uri: Uri, body: Bytes) -> Response {
    let p = params(&uri, &body);
    match matched(&st, &p) {
        Ok(list) => {
            let mut names: BTreeSet<String> = BTreeSet::new();
            for s in &list {
                names.extend(labels_of(s).into_keys());
            }
            ok(json!(names))
        }
        Err(e) => err(e),
    }
}

async fn label_values(State(st): State<SharedState>, Path(name): Path<String>, uri: Uri) -> Response {
    let p = params(&uri, &Bytes::new());
    match matched(&st, &p) {
        Ok(list) => {
            let vals: BTreeSet<String> =
                list.iter().map(|s| label_value(s, &name).to_string()).filter(|v| !v.is_empty()).collect();
            ok(json!(vals))
        }
        Err(e) => err(e),
    }
}

async fn series(State(st): State<SharedState>, uri: Uri, body: Bytes) -> Response {
    let p = params(&uri, &body);
    if !p.iter().any(|(k, _)| k == "match[]") {
        return err("match[] が必要です");
    }
    match matched(&st, &p) {
        Ok(list) => ok(json!(list.iter().map(labels_of).collect::<Vec<_>>())),
        Err(e) => err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_selectors() {
        let Expr::Select { agg, matchers, range_ms } = parse(r#"max_over_time(temp{device=~"dev0(1|2)", _measurement!="x"}[5m])"#).unwrap() else {
            panic!()
        };
        assert_eq!(agg, Agg::Max);
        assert_eq!(range_ms, Some(5 * MIN));
        assert_eq!(matchers.len(), 3);
        assert!(matchers[1].matches("dev02"));
        assert!(!matchers[1].matches("dev021"));
        let Expr::Select { matchers, .. } = parse(r#"{__name__="温度", "装置"='炉 1'}"#).unwrap() else { panic!() };
        assert_eq!(matchers[1].label, "装置");
        assert_eq!(matchers[1].value, "炉 1");
        assert!(matches!(parse("1+1").unwrap(), Expr::Number(n) if n == 2.0));
        assert!(parse("rate(x[5m])").is_err());
        assert!(parse("sum(x)").is_err());
        assert!(parse(r#"x{a="b"} + 1"#).is_err());
        // 正規表現のエスケープは残す
        let Expr::Select { matchers, .. } = parse(r#"x{a=~"1\\.5|2"}"#).unwrap() else { panic!() };
        assert!(matchers[1].matches("1.5"));
        assert!(!matchers[1].matches("105"));
    }
}
