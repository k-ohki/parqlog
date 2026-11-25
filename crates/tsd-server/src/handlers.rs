use crate::auth::{error, Access};
use crate::SharedState;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tsd_core::ingest::IngestError;
use tsd_core::lp::Precision;
use tsd_core::time::{now_ms, parse_time, HOUR};
use tsd_core::*;

type ApiResult = Result<Response, Response>;

fn internal(e: impl std::fmt::Display) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", e.to_string())
}

fn bad(msg: impl Into<String>) -> Response {
    error(StatusCode::BAD_REQUEST, "invalid", msg)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> Result<T, Response> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(internal(format!("{e:#}"))),
        Err(e) => Err(internal(e)),
    }
}

fn parse_ids(s: &str) -> Result<Vec<SeriesId>, Response> {
    s.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<SeriesId>().map_err(|_| bad(format!("系列 ID が不正です: {x}"))))
        .collect()
}

// ---- 取り込み ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct WriteParams {
    precision: Option<String>,
    accept_partial: Option<bool>,
}

pub async fn write_v2(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    Query(p): Query<WriteParams>,
    body: Bytes,
) -> Response {
    write_impl(st, acc, p.precision.as_deref().unwrap_or("ns"), true, body).await
}

/// InfluxDB 3 互換（Telegraf の outputs.influxdb_v3 から使える）。db は無視して単一 DB に保存
pub async fn write_v3(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    Query(p): Query<WriteParams>,
    body: Bytes,
) -> Response {
    write_impl(st, acc, p.precision.as_deref().unwrap_or("auto"), p.accept_partial.unwrap_or(true), body).await
}

pub async fn write_v1(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    Query(p): Query<WriteParams>,
    body: Bytes,
) -> Response {
    write_impl(st, acc, p.precision.as_deref().unwrap_or("n"), true, body).await
}

async fn write_impl(st: SharedState, acc: Access, precision: &str, accept_partial: bool, body: Bytes) -> Response {
    if acc.local && st.cfg.auth.require_token_on_loopback && !acc.token_ok {
        return error(StatusCode::UNAUTHORIZED, "unauthorized", "トークンが無いか正しくありません");
    }
    let Some(precision) = Precision::parse(precision) else {
        return bad(format!("precision が不正です: {precision}"));
    };
    let text = match String::from_utf8(body.to_vec()) {
        Ok(t) => t,
        Err(_) => return bad("本文が UTF-8 ではありません"),
    };
    let ing = st.ingest.clone();
    let res = tokio::task::spawn_blocking(move || ing.ingest_lp_with(&text, precision, accept_partial)).await;
    match res {
        Ok(Ok(r)) if r.errors.is_empty() => StatusCode::NO_CONTENT.into_response(),
        Ok(Ok(r)) => {
            // 正しい行は書き込み済み（InfluxDB の部分書き込みと同じ扱い）
            let first = &r.errors[0];
            tracing::warn!(errors = r.errors.len(), first = %first, "Line Protocol の構文エラー");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "code": "invalid",
                    "message": if accept_partial {
                        format!("partial write: {} 行が不正です（最初: {}）", r.errors.len(), first)
                    } else {
                        format!("{} 行が不正なため、何も書き込みませんでした（最初: {}）", r.errors.len(), first)
                    },
                    "line": first.line,
                    "errors": r.errors.iter().take(100).map(|e| serde_json::json!({"line": e.line, "message": e.message})).collect::<Vec<_>>(),
                    "accepted_points": r.points,
                })),
            )
                .into_response()
        }
        Ok(Err(IngestError::Busy)) => {
            let mut r = error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", IngestError::Busy.to_string());
            r.headers_mut().insert(header::RETRY_AFTER, "5".parse().unwrap());
            r
        }
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

pub async fn ping() -> Response {
    let mut r = StatusCode::NO_CONTENT.into_response();
    r.headers_mut().insert("X-Influxdb-Version", "tsd".parse().unwrap());
    r
}

pub async fn health() -> Response {
    Json(serde_json::json!({ "name": "tsd", "status": "pass", "version": VERSION })).into_response()
}

// ---- クエリ ---------------------------------------------------------------

#[derive(Deserialize)]
pub struct QueryParams {
    series: String,
    from: Option<String>,
    to: Option<String>,
    width: Option<u32>,
    tier: Option<String>,
}

pub async fn query(State(st): State<SharedState>, Query(p): Query<QueryParams>) -> ApiResult {
    let now = now_ms();
    let to = match &p.to {
        Some(s) => parse_time(s, now).ok_or_else(|| bad(format!("to が不正です: {s}")))?,
        None => now,
    };
    let from = match &p.from {
        Some(s) => parse_time(s, now).ok_or_else(|| bad(format!("from が不正です: {s}")))?,
        None => to - HOUR,
    };
    let tier = Tier::parse(p.tier.as_deref().unwrap_or("")).ok_or_else(|| bad("tier は auto / raw / 1m / 1h"))?;
    let q = AggQuery { series: parse_ids(&p.series)?, from_ms: from, to_ms: to, width: p.width.unwrap_or(1000), tier };
    let store = st.store.clone();
    let r = blocking(move || store.query_agg(&q)).await?;
    Ok(Json(r).into_response())
}

#[derive(Serialize)]
struct SeriesOut {
    #[serde(flatten)]
    s: Series,
    last_ts: Option<i64>,
    last_value: Option<f32>,
}

pub async fn series(State(st): State<SharedState>, Query(p): Query<HashMap<String, String>>) -> ApiResult {
    let mut f = SeriesFilter {
        q: p.get("q").filter(|s| !s.is_empty()).cloned(),
        measurement: p.get("measurement").filter(|s| !s.is_empty()).cloned(),
        limit: p.get("limit").and_then(|s| s.parse().ok()),
        ..Default::default()
    };
    if let Some(ids) = p.get("ids") {
        f.ids = Some(parse_ids(ids)?);
    }
    for (k, v) in &p {
        if let Some(tk) = k.strip_prefix("tag.") {
            if !v.is_empty() {
                f.tags.push((tk.to_string(), v.clone()));
            }
        }
    }
    let list = st.store.list_series(&f).map_err(internal)?;
    let lat: HashMap<SeriesId, (i64, f32)> =
        st.store.latest(&[]).map_err(internal)?.into_iter().map(|l| (l.id, (l.ts, l.value))).collect();
    let out: Vec<SeriesOut> = list
        .into_iter()
        .map(|s| {
            let l = lat.get(&s.id).copied();
            SeriesOut { last_ts: l.map(|x| x.0), last_value: l.map(|x| x.1), s }
        })
        .collect();
    Ok(Json(out).into_response())
}

/// `{"unit": "°C"}` で単位、`{"key": "plc,device=dev01 temp"}` で系列キー（名前）を変える。
/// キーの変更はローカルからのみ。
pub async fn series_update(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    UrlPath(id): UrlPath<SeriesId>,
    Json(b): Json<serde_json::Value>,
) -> ApiResult {
    let key = b.get("key").and_then(|v| v.as_str()).map(String::from);
    let unit = b.get("unit").map(|v| v.as_str().map(String::from));
    if key.is_some() {
        require_local(&acc)?;
    }
    let store = st.store.clone();
    let res = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Series>> {
        if let Some(u) = unit {
            store.set_series_unit(id, u)?;
        }
        key.map(|k| store.rename_series(id, &k)).transpose()
    })
    .await
    .map_err(internal)?;
    match res {
        Ok(Some(s)) => Ok(Json(s).into_response()),
        Ok(None) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(e) => Err(bad(format!("{e:#}"))),
    }
}

/// 系列の削除（ローカルからのみ）
pub async fn series_delete(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    UrlPath(id): UrlPath<SeriesId>,
) -> ApiResult {
    require_local(&acc)?;
    let store = st.store.clone();
    let n = blocking(move || store.delete_series(&[id])).await?;
    if n == 0 {
        return Err(error(StatusCode::NOT_FOUND, "not found", format!("系列 {id} がありません")));
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
pub struct LatestParams {
    series: Option<String>,
}

pub async fn latest(State(st): State<SharedState>, Query(p): Query<LatestParams>) -> ApiResult {
    let ids = parse_ids(p.series.as_deref().unwrap_or(""))?;
    Ok(Json(st.store.latest(&ids).map_err(internal)?).into_response())
}

#[derive(Serialize)]
pub struct DeviceOut {
    device: String,
    series_count: usize,
    last_ts: Option<i64>,
    age_ms: Option<i64>,
    stale: bool,
}

pub fn device_of(s: &Series, tag: &str) -> String {
    s.tags.get(tag).or_else(|| s.tags.get("host")).cloned().unwrap_or_else(|| s.measurement.clone())
}

fn compute_devices(st: &SharedState) -> anyhow::Result<Vec<DeviceOut>> {
    let now = now_ms();
    let stale_ms = st.cfg.monitor.stale_seconds as i64 * 1000;
    let lat: HashMap<SeriesId, i64> = st.store.latest(&[])?.into_iter().map(|l| (l.id, l.ts)).collect();
    let mut by: BTreeMap<String, (usize, Option<i64>)> = BTreeMap::new();
    for s in st.store.list_series(&SeriesFilter::default())? {
        let e = by.entry(device_of(&s, &st.cfg.monitor.device_tag)).or_insert((0, None));
        e.0 += 1;
        if let Some(&ts) = lat.get(&s.id) {
            e.1 = Some(e.1.map_or(ts, |x: i64| x.max(ts)));
        }
    }
    Ok(by
        .into_iter()
        .map(|(device, (n, last))| {
            let age = last.map(|t| now - t);
            DeviceOut { device, series_count: n, last_ts: last, age_ms: age, stale: age.map_or(true, |a| a > stale_ms) }
        })
        .collect())
}


pub async fn devices(State(st): State<SharedState>) -> ApiResult {
    Ok(Json(compute_devices(&st).map_err(internal)?).into_response())
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DiskInfo {
    pub data_bytes: u64,
    pub raw_bytes: u64,
    pub m1_bytes: u64,
    pub file_count: u64,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

fn disk_info(st: &SharedState) -> DiskInfo {
    {
        let c = st.disk_cache.lock();
        if let Some((t, d)) = c.as_ref() {
            if t.elapsed() < Duration::from_secs(30) {
                return d.clone();
            }
        }
    }
    let mut d = DiskInfo::default();
    for f in tsd_core::duck::walk_files(&st.data_dir) {
        let len = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0);
        d.data_bytes += len;
        d.file_count += 1;
        let rel = f.strip_prefix(&st.data_dir).unwrap_or(&f);
        match rel.components().next().and_then(|c| c.as_os_str().to_str()) {
            Some("raw") => d.raw_bytes += len,
            Some("1m") => d.m1_bytes += len,
            _ => {}
        }
    }
    if let Ok(s) = fs4::statvfs(&st.data_dir) {
        d.free_bytes = s.available_space();
        d.total_bytes = s.total_space();
    }
    *st.disk_cache.lock() = Some((Instant::now(), d.clone()));
    d
}

pub async fn status(State(st): State<SharedState>) -> ApiResult {
    let st2 = st.clone();
    let (store, disk, devices) =
        blocking(move || Ok((st2.store.status()?, disk_info(&st2), compute_devices(&st2)?))).await?;
    let ingest = st.ingest.stats();

    let mut warnings: Vec<String> = vec![];
    let warn_bytes = st.cfg.monitor.disk_warn_gb as u64 * 1024 * 1024 * 1024;
    if disk.total_bytes > 0 && disk.free_bytes < warn_bytes {
        warnings.push(format!(
            "ディスク残量が少なくなっています（残り {:.1} GB）",
            disk.free_bytes as f64 / 1024f64.powi(3)
        ));
    }
    let stale: Vec<&str> = devices.iter().filter(|d| d.stale).map(|d| d.device.as_str()).collect();
    if !stale.is_empty() {
        let shown = stale.iter().take(10).copied().collect::<Vec<_>>().join(", ");
        let more = if stale.len() > 10 { format!(" ほか {} 台", stale.len() - 10) } else { String::new() };
        warnings.push(format!("受信が途絶えている装置があります（{} 台）: {shown}{more}", stale.len()));
    }
    for j in &store.jobs {
        if let Some(e) = &j.last_error {
            warnings.push(format!("ジョブ {} が失敗しています: {e}", j.name));
        }
    }
    if let Some(e) = &ingest.last_write_error {
        if ingest.buffer_len > 0 {
            warnings.push(format!("書き込みに失敗しています: {e}"));
        }
    }
    if tracing::enabled!(tracing::Level::DEBUG) {
        tracing::debug!(?warnings, "status");
    }

    Ok(Json(serde_json::json!({
        "version": VERSION,
        "now": now_ms(),
        "uptime_s": st.started.elapsed().as_secs(),
        "ingest": ingest,
        "store": store,
        "disk": disk,
        "devices": { "total": devices.len(), "stale": stale.len() },
        "warnings": warnings,
        "config": {
            "listen": st.cfg.http.listen,
            "ingest_listen": st.cfg.http.ingest_listen,
            "data_dir": st.cfg.storage.data_dir,
            "stale_seconds": st.cfg.monitor.stale_seconds,
            "device_tag": st.cfg.monitor.device_tag,
            "config_path": st.config_path.to_string_lossy(),
        }
    }))
    .into_response())
}

// ---- エクスポート -----------------------------------------------------------

#[derive(Deserialize)]
pub struct ExportBody {
    series: Vec<SeriesId>,
    from: serde_json::Value,
    to: serde_json::Value,
    #[serde(default)]
    tier: Option<String>,
    format: ExportFormat,
}

fn json_time(v: &serde_json::Value, now: i64) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => parse_time(s, now),
        _ => None,
    }
}

/// 一時ファイルを読み終えたら削除するストリーム（ファイルを閉じてから削除するためフィールド順が重要）
struct TempFileStream {
    inner: tokio_util::io::ReaderStream<tokio::fs::File>,
    _guard: TempGuard,
}

struct TempGuard(PathBuf);

impl Drop for TempGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl futures_core::Stream for TempFileStream {
    type Item = std::io::Result<Bytes>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

/// 起動時: エクスポート・取り込み用の一時ファイルを消す
pub fn cleanup_tmp(data_dir: &Path) {
    let _ = std::fs::remove_dir_all(data_dir.join("tmp"));
}

pub async fn export(State(st): State<SharedState>, Json(b): Json<ExportBody>) -> ApiResult {
    let now = now_ms();
    let from = json_time(&b.from, now).ok_or_else(|| bad("from が不正です"))?;
    let to = json_time(&b.to, now).ok_or_else(|| bad("to が不正です"))?;
    let tier = match Tier::parse(b.tier.as_deref().unwrap_or("raw")).ok_or_else(|| bad("tier は raw / 1m / 1h"))? {
        Some(t) => t,
        None => {
            // auto: 1日以内なら生データ、1か月以内なら1分、それ以上は1時間
            let span = to - from;
            if span <= tsd_core::time::DAY {
                Tier::Raw
            } else if span <= 31 * tsd_core::time::DAY {
                Tier::M1
            } else {
                Tier::H1
            }
        }
    };
    let ext = match b.format {
        ExportFormat::Csv => "csv",
        ExportFormat::Parquet => "parquet",
    };
    let dir = st.data_dir.join("tmp");
    std::fs::create_dir_all(&dir).map_err(internal)?;
    let path = dir.join(format!("export-{}-{}.{ext}", now, std::process::id()));
    let req = ExportRequest { series: b.series, from_ms: from, to_ms: to, tier, format: b.format };
    let store = st.store.clone();
    let p2 = path.clone();
    let rows = match blocking(move || store.export(&req, &p2)).await {
        Ok(n) => n,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
    };
    let file = tokio::fs::File::open(&path).await.map_err(internal)?;
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    let stream = TempFileStream { inner: tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024), _guard: TempGuard(path) };
    let name = format!("tsd-{}-{}.{ext}", tsd_core::time::day_str(from), tier.name());
    Ok(Response::builder()
        .header(
            header::CONTENT_TYPE,
            if ext == "csv" { "text/csv; charset=utf-8" } else { "application/vnd.apache.parquet" },
        )
        .header(header::CONTENT_LENGTH, len)
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\""))
        .header("X-Tsd-Rows", rows)
        .body(Body::from_stream(stream))
        .map_err(internal)?)
}

// ---- バックアップと設定（ローカルのみ） -------------------------------------

fn require_local(acc: &Access) -> Result<(), Response> {
    if acc.local {
        Ok(())
    } else {
        Err(error(StatusCode::FORBIDDEN, "forbidden", "この操作はサービスを動かしている PC からのみ行えます"))
    }
}

#[derive(Deserialize)]
pub struct BackupBody {
    dest: String,
}

pub async fn backup(State(st): State<SharedState>, Extension(acc): Extension<Access>, Json(b): Json<BackupBody>) -> ApiResult {
    require_local(&acc)?;
    if b.dest.trim().is_empty() {
        return Err(bad("dest を指定してください"));
    }
    let dest = PathBuf::from(b.dest.trim());
    if !dest.is_absolute() {
        return Err(bad("バックアップ先は絶対パス（例: /mnt/backup/tsd）で指定してください"));
    }
    let store = st.store.clone();
    let rep = blocking(move || store.backup(&dest)).await?;
    Ok(Json(rep).into_response())
}

pub async fn config_get(State(st): State<SharedState>, Extension(acc): Extension<Access>) -> ApiResult {
    require_local(&acc)?;
    let cfg = Config::load_or_default(&st.config_path).map_err(|e| internal(format!("{e:#}")))?;
    Ok(Json(serde_json::json!({
        "path": st.config_path.to_string_lossy(),
        "config": cfg,
        "running": st.cfg,
    }))
    .into_response())
}

pub async fn config_put(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    Json(cfg): Json<Config>,
) -> ApiResult {
    require_local(&acc)?;
    cfg.validate().map_err(|e| bad(format!("{e:#}")))?;
    cfg.save(&st.config_path).map_err(|e| internal(format!("{e:#}")))?;
    tracing::info!(path = %st.config_path.display(), "設定を保存しました（サービスの再起動で反映）");
    Ok(Json(serde_json::json!({ "saved": true, "restart_required": cfg != st.cfg })).into_response())
}

// ---- CSV 取り込み（ローカルのみ） -----------------------------------------

#[derive(Deserialize)]
pub struct ImportParams {
    #[serde(default)]
    layout: Option<String>,
    measurement: Option<String>,
    time_column: Option<String>,
    /// タイムゾーンの無い日時の UTC からのずれ（例 +09:00）。省略時はサーバーの地方時。
    utc_offset: Option<String>,
}

/// `+09:00` / `-0500` / `540`（分） → 分
pub fn parse_offset(s: &str) -> Option<i32> {
    let s = s.trim();
    if let Ok(m) = s.parse::<i32>() {
        return Some(m);
    }
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => (1, s),
    };
    let rest = rest.replace(':', "");
    if rest.len() != 4 {
        return None;
    }
    let h: i32 = rest[..2].parse().ok()?;
    let m: i32 = rest[2..].parse().ok()?;
    Some(sign * (h * 60 + m))
}

pub fn local_offset_minutes() -> i32 {
    chrono::Local::now().offset().local_minus_utc() / 60
}

/// 本文の CSV（gzip 可）を一時ファイルへ流し込み、取り込む。`tag.<キー>=<値>` でタグを付ける。
pub async fn import_csv(
    State(st): State<SharedState>,
    Extension(acc): Extension<Access>,
    Query(p): Query<ImportParams>,
    Query(all): Query<HashMap<String, String>>,
    body: Body,
) -> ApiResult {
    require_local(&acc)?;
    let layout = match p.layout.as_deref().unwrap_or("wide") {
        "wide" => CsvLayout::Wide,
        "long" => CsvLayout::Long,
        x => return Err(bad(format!("layout は wide か long です: {x}"))),
    };
    let utc_offset_minutes = match &p.utc_offset {
        Some(s) => parse_offset(s).ok_or_else(|| bad(format!("utc_offset が不正です: {s}")))?,
        None => local_offset_minutes(),
    };
    let mut tags: Vec<(String, String)> =
        all.iter().filter_map(|(k, v)| k.strip_prefix("tag.").map(|k| (k.to_string(), v.clone()))).collect();
    tags.retain(|(k, v)| !k.is_empty() && !v.is_empty());
    let opt = CsvImportOptions {
        layout,
        measurement: p.measurement.unwrap_or_else(|| "csv".into()),
        tags,
        time_column: p.time_column.filter(|s| !s.is_empty()),
        utc_offset_minutes,
    };

    let dir = st.data_dir.join("tmp");
    std::fs::create_dir_all(&dir).map_err(internal)?;
    let path = dir.join(format!("import-{}-{}.csv", now_ms(), std::process::id()));
    let _guard = TempGuard(path.clone());
    {
        use tokio::io::AsyncWriteExt;
        let mut f = tokio::fs::File::create(&path).await.map_err(internal)?;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = futures_util_next(&mut stream).await {
            let chunk = chunk.map_err(|e| bad(format!("本文を読めません: {e}")))?;
            f.write_all(&chunk).await.map_err(internal)?;
        }
        f.flush().await.map_err(internal)?;
    }
    let store = st.store.clone();
    let p2 = path.clone();
    let rep = blocking(move || store.import_csv(&p2, &opt)).await?;
    Ok(Json(rep).into_response())
}

async fn futures_util_next<S: futures_core::Stream + Unpin>(s: &mut S) -> Option<S::Item> {
    std::future::poll_fn(|cx| Pin::new(&mut *s).poll_next(cx)).await
}
