//! DuckDB + 日付単位 Parquet によるストレージ実装。
//!
//! 保存層（仕様3章）:
//! - `raw_hot`   : 直近の生データ（DuckDB）。確定した日は `raw/YYYY/MM/YYYY-MM-DD.parquet` へ。
//!                 1年を超えた日は `YYYY-MM-DD.q16.parquet`（16bit 量子化）へ再圧縮。
//! - `r1m_hot`   : 当月の1分集計（DuckDB）。確定した月は `1m/YYYY-MM.parquet` へ。
//! - `r1h`       : 1時間集計（DuckDB のみ）。
//!
//! 境界（ウォーターマーク）は `meta` テーブルに保存し、各ジョブは何度実行しても同じ結果になる。
//!
//! ロック順序: `writer` → `registry`/`layout` → 読み取り接続プール（逆順で取らない）。
//! クエリは `layout` の読み取りロックを保持したまま SQL を実行するので、
//! ファイル削除や `raw_hot` からの削除（`layout` 書き込みロック中に行う）と競合しない。

use crate::lp;
use crate::store::*;
use crate::time::*;
use anyhow::{anyhow, bail, Context, Result};
use duckdb::types::{TimeUnit, Value};
use duckdb::{params, Connection};
use parking_lot::{Mutex, MutexGuard, RwLock};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use tracing::{debug, info, warn};

mod import;

const SCHEMA_VERSION: i64 = 1;
const READERS: usize = 4;
/// Parquet V2 のエンコーディング（DELTA_BINARY_PACKED など）で時刻列がほぼ0バイトになる
const PARQUET_OPTS: &str = "FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 9, ROW_GROUP_SIZE 122880, PARQUET_VERSION V2";

#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub data_dir: PathBuf,
    pub memory_limit: String,
    pub threads: u32,
    pub grace_days: i64,
    pub raw_days: i64,
    pub m1_days: i64,
    pub h1_days: i64,
    /// 0 で再圧縮しない
    pub compact_after_days: i64,
}

impl StoreOptions {
    pub fn for_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: dir.into(),
            memory_limit: "1GB".into(),
            threads: 0,
            grace_days: 1,
            raw_days: 1826,
            m1_days: 1826,
            h1_days: 1826,
            compact_after_days: 365,
        }
    }
}

#[derive(Default)]
struct Registry {
    by_key: HashMap<String, SeriesId>,
    by_id: BTreeMap<SeriesId, Series>,
    next_id: SeriesId,
}

pub struct DuckStore {
    opt: StoreOptions,
    db_path: PathBuf,
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    next_reader: AtomicUsize,
    layout: RwLock<Watermarks>,
    registry: RwLock<Registry>,
    latest: RwLock<HashMap<SeriesId, (i64, f32)>>,
    seq: AtomicU64,
    written: AtomicU64,
    rejected: AtomicU64,
    jobs: Mutex<BTreeMap<&'static str, JobState>>,
    maint: Mutex<()>,
    last_retention_day: AtomicI64,
}

// ---- SQL ヘルパー -----------------------------------------------------------

/// TIMESTAMP_MS 列と比較するための定数（列側のキャストを避けて Parquet の統計を効かせる）
fn tsms(ms: i64) -> String {
    format!("make_timestamp({}::BIGINT)::TIMESTAMP_MS", ms.saturating_mul(1000))
}

/// TIMESTAMP 列と比較するための定数
fn tsus(ms: i64) -> String {
    format!("make_timestamp({}::BIGINT)", ms.saturating_mul(1000))
}

pub fn sql_str(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn sql_path(p: &Path) -> String {
    sql_str(&p.to_string_lossy())
}

fn map_literal(tags: &[(String, String)]) -> String {
    let items = tags.iter().map(|(k, v)| format!("{}: {}", sql_str(k), sql_str(v))).collect::<Vec<_>>().join(", ");
    format!("MAP {{{items}}}")
}

fn id_list(ids: &[SeriesId]) -> String {
    let mut s = String::new();
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        let _ = write!(s, "{id}");
    }
    s
}

fn file_list(files: &[PathBuf]) -> String {
    let v: Vec<String> = files.iter().map(|p| sql_path(p)).collect();
    format!("[{}]", v.join(","))
}

fn tx<T>(c: &Connection, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    c.execute_batch("BEGIN TRANSACTION")?;
    match f(c) {
        Ok(v) => {
            c.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = c.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn get_meta(c: &Connection, key: &str) -> Result<Option<i64>> {
    let mut st = c.prepare("SELECT value FROM meta WHERE key = ?")?;
    let mut rows = st.query(params![key])?;
    Ok(match rows.next()? {
        Some(r) => Some(r.get(0)?),
        None => None,
    })
}

fn set_meta(c: &Connection, key: &str, v: i64) -> Result<()> {
    c.execute("INSERT OR REPLACE INTO meta VALUES (?, ?)", params![key, v])?;
    Ok(())
}

fn count(c: &Connection, sql: &str) -> Result<u64> {
    let v: i64 = c.query_row(sql, [], |r| r.get(0))?;
    Ok(v.max(0) as u64)
}

/// 重複を後勝ちで取り除いた `raw_hot` の範囲
fn raw_hot_dedup(ids: Option<&[SeriesId]>, a: i64, b: i64) -> String {
    let f = ids.map(|i| format!("series_id IN ({}) AND ", id_list(i))).unwrap_or_default();
    format!(
        "SELECT series_id, ts, arg_max(value, seq) AS value FROM raw_hot \
         WHERE {f}ts >= {} AND ts < {} GROUP BY series_id, ts",
        tsms(a),
        tsms(b)
    )
}

const M1_AGG: &str = "min(value), max(value), sum(value::DOUBLE), count(*)::UINTEGER, arg_min(value, ts), arg_max(value, ts)";
const UP_AGG: &str = "min(vmin), max(vmax), sum(vsum), sum(vcount)::UINTEGER, arg_min(vfirst, bucket), arg_max(vlast, bucket)";

/// `select` の結果（series_id, ts, value）を 16bit に量子化する SQL。
/// value = q_off + q * q_scale（系列ごと・ファイルごとの係数。列は RLE でほぼ容量を取らない）
fn q16_sql(select: &str) -> String {
    format!(
        "WITH s AS ({select}), r AS (SELECT series_id, min(value)::DOUBLE AS lo, max(value)::DOUBLE AS hi FROM s GROUP BY series_id) \
         SELECT s.series_id, s.ts, \
           CASE WHEN r.hi > r.lo THEN (round((s.value - r.lo) / ((r.hi - r.lo) / 65535.0)) - 32768)::SMALLINT ELSE 0::SMALLINT END AS q, \
           CASE WHEN r.hi > r.lo THEN r.lo + 32768.0 * ((r.hi - r.lo) / 65535.0) ELSE r.lo END AS q_off, \
           CASE WHEN r.hi > r.lo THEN (r.hi - r.lo) / 65535.0 ELSE 0.0 END AS q_scale \
         FROM s JOIN r USING (series_id) ORDER BY s.series_id, s.ts"
    )
}

/// SELECT の結果を Parquet に書く。一時ファイルに書いてからリネームする。
pub fn copy_to_parquet(c: &Connection, select: &str, path: &Path) -> Result<usize> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
    let _ = std::fs::remove_file(&tmp);
    let n = c
        .execute(&format!("COPY ({select}) TO {} ({PARQUET_OPTS})", sql_path(&tmp)), [])
        .with_context(|| format!("Parquet 書き出しに失敗: {}", path.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(n)
}

impl DuckStore {
    pub fn open(opt: StoreOptions) -> Result<Self> {
        Self::open_at(opt, now_ms())
    }

    /// `now` は初回作成時のウォーターマーク初期値に使う（テスト用に差し替え可能）
    pub fn open_at(opt: StoreOptions, now: i64) -> Result<Self> {
        std::fs::create_dir_all(&opt.data_dir)
            .with_context(|| format!("データフォルダを作れません: {}", opt.data_dir.display()))?;
        std::fs::create_dir_all(opt.data_dir.join("raw"))?;
        std::fs::create_dir_all(opt.data_dir.join("1m"))?;
        cleanup_partial_files(&opt.data_dir);

        let db_path = opt.data_dir.join("tsd.duckdb");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("DuckDB を開けません（他のプロセスが使用中の可能性）: {}", db_path.display()))?;
        conn.execute_batch(&format!(
            "SET memory_limit = {}; SET preserve_insertion_order = false;",
            sql_str(&opt.memory_limit)
        ))?;
        if opt.threads > 0 {
            conn.execute_batch(&format!("SET threads = {};", opt.threads))?;
        }
        migrate(&conn)?;

        // ウォーターマーク
        let base = floor_to(now - opt.grace_days * DAY, DAY);
        let mut wm = Watermarks::default();
        {
            let load = |key: &str, default: i64| -> Result<i64> {
                Ok(match get_meta(&conn, key)? {
                    Some(v) => v,
                    None => {
                        set_meta(&conn, key, default)?;
                        default
                    }
                })
            };
            wm.raw_accept_from = load("raw_accept_from", base)?;
            wm.raw_flushed_until = load("raw_flushed_until", base)?;
            wm.raw_compacted_until = load("raw_compacted_until", base)?;
            wm.m1_until = load("m1_until", base)?;
            wm.h1_until = load("h1_until", base)?;
            wm.m1_flushed_until = load("m1_flushed_until", month_start(base))?;
        }

        let seq: i64 = conn.query_row("SELECT coalesce(max(seq), 0)::BIGINT FROM raw_hot", [], |r| r.get(0))?;

        let mut readers = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            readers.push(Mutex::new(conn.try_clone()?));
        }

        let store = Self {
            opt,
            db_path,
            writer: Mutex::new(conn),
            readers,
            next_reader: AtomicUsize::new(0),
            layout: RwLock::new(wm),
            registry: RwLock::new(Registry::default()),
            latest: RwLock::new(HashMap::new()),
            seq: AtomicU64::new(seq.max(0) as u64),
            written: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            jobs: Mutex::new(BTreeMap::new()),
            maint: Mutex::new(()),
            last_retention_day: AtomicI64::new(i64::MIN),
        };
        store.reload_registry()?;
        store.load_latest()?;
        Ok(store)
    }

    pub fn options(&self) -> &StoreOptions {
        &self.opt
    }

    pub fn data_dir(&self) -> &Path {
        &self.opt.data_dir
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// 書き込み用接続（一括投入ツール向け）
    pub fn writer(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock()
    }

    pub fn watermarks(&self) -> Watermarks {
        self.layout.read().clone()
    }

    /// ウォーターマークを直接設定する（一括投入ツール向け。サービス停止中に使う）
    pub fn set_watermarks(&self, wm: &Watermarks) -> Result<()> {
        let w = self.writer.lock();
        let mut l = self.layout.write();
        tx(&w, |c| {
            set_meta(c, "raw_accept_from", wm.raw_accept_from)?;
            set_meta(c, "raw_flushed_until", wm.raw_flushed_until)?;
            set_meta(c, "raw_compacted_until", wm.raw_compacted_until)?;
            set_meta(c, "m1_until", wm.m1_until)?;
            set_meta(c, "h1_until", wm.h1_until)?;
            set_meta(c, "m1_flushed_until", wm.m1_flushed_until)?;
            Ok(())
        })?;
        *l = wm.clone();
        Ok(())
    }

    pub fn raw_path(&self, day: i64) -> PathBuf {
        let d = day_str(day);
        self.opt.data_dir.join("raw").join(&d[0..4]).join(&d[5..7]).join(format!("{d}.parquet"))
    }

    pub fn raw_q16_path(&self, day: i64) -> PathBuf {
        let d = day_str(day);
        self.opt.data_dir.join("raw").join(&d[0..4]).join(&d[5..7]).join(format!("{d}.q16.parquet"))
    }

    pub fn m1_path(&self, month: i64) -> PathBuf {
        self.opt.data_dir.join("1m").join(format!("{}.parquet", month_str(month)))
    }

    /// 生データの日次ファイルを書く。`compact` なら 16bit 形式。
    pub fn write_raw_day(&self, c: &Connection, select: &str, day: i64, compact: bool) -> Result<usize> {
        if compact {
            let n = copy_to_parquet(c, &q16_sql(select), &self.raw_q16_path(day))?;
            let _ = std::fs::remove_file(self.raw_path(day));
            Ok(n)
        } else {
            copy_to_parquet(
                c,
                &format!("SELECT series_id, ts, value FROM ({select}) ORDER BY series_id, ts"),
                &self.raw_path(day),
            )
        }
    }

    /// 登録されている（削除されていない）ID だけを残す
    fn known_ids(&self, ids: &[SeriesId]) -> Vec<SeriesId> {
        let r = self.registry.read();
        ids.iter().copied().filter(|id| r.by_id.contains_key(id)).collect()
    }

    fn reader(&self) -> MutexGuard<'_, Connection> {
        let n = self.readers.len();
        let start = self.next_reader.fetch_add(1, Ordering::Relaxed) % n;
        for i in 0..n {
            if let Some(g) = self.readers[(start + i) % n].try_lock() {
                return g;
            }
        }
        self.readers[start].lock()
    }

    pub fn reload_registry(&self) -> Result<()> {
        let w = self.writer.lock();
        let mut st = w.prepare("SELECT id, key, unit, epoch_ms(created_at) FROM series ORDER BY id")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<i64>>(3)?))
        })?;
        let mut reg = Registry { next_id: 1, ..Default::default() };
        for row in rows {
            let (id, key, unit, created) = row?;
            let (measurement, tags, field) =
                lp::parse_series_key(&key).ok_or_else(|| anyhow!("系列キーが壊れています: {key}"))?;
            reg.by_key.insert(key.clone(), id);
            reg.by_id.insert(
                id,
                Series { id, key, measurement, field, tags: tags.into_iter().collect(), unit, created_at: created.unwrap_or(0) },
            );
            reg.next_id = reg.next_id.max(id + 1);
        }
        // 削除した系列の ID は使い回さない（Parquet に古い行が残っているため）
        if let Some(n) = get_meta(&w, "next_series_id")? {
            reg.next_id = reg.next_id.max(n as SeriesId);
        }
        *self.registry.write() = reg;
        Ok(())
    }

    fn load_latest(&self) -> Result<()> {
        let r = self.reader();
        let mut map = HashMap::new();
        let mut st = r.prepare(
            "SELECT series_id, epoch_ms(max(bucket)), arg_max(vlast, bucket) FROM r1m_hot GROUP BY series_id",
        )?;
        for row in st.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?, r.get::<_, f32>(2)?)))? {
            let (id, ts, v) = row?;
            map.insert(id, (ts, v));
        }
        let mut st = r.prepare("SELECT series_id, epoch_ms(max(ts)::TIMESTAMP), arg_max(value, ts) FROM raw_hot GROUP BY series_id")?;
        for row in st.query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, i64>(1)?, r.get::<_, f32>(2)?)))? {
            let (id, ts, v) = row?;
            map.insert(id, (ts, v));
        }
        *self.latest.write() = map;
        Ok(())
    }

    /// 書き込み本体。`now` は拒否判定に使う。
    pub fn write_points_at(&self, points: &[Point], now: i64) -> Result<WriteOutcome> {
        let mut out = WriteOutcome::default();
        if points.is_empty() {
            return Ok(out);
        }
        let w = self.writer.lock();
        let (accept_from, m1_until) = {
            let l = self.layout.read();
            (l.raw_accept_from, l.m1_until)
        };
        let max_ts = now + DAY;
        // 削除済みの系列宛ての点（削除前にバッファに入っていたもの）は捨てる
        let reg = self.registry.read();

        // 同じ (series_id, ts) はバッチ内でも後勝ち
        let mut idx: HashMap<(SeriesId, i64), usize> = HashMap::with_capacity(points.len());
        let mut accepted: Vec<Point> = Vec::with_capacity(points.len());
        for p in points {
            if !reg.by_id.contains_key(&p.series_id) {
                continue;
            }
            if p.ts_ms < accept_from {
                out.rejected_old += 1;
                continue;
            }
            if p.ts_ms > max_ts {
                out.rejected_future += 1;
                continue;
            }
            match idx.get(&(p.series_id, p.ts_ms)) {
                Some(&i) => accepted[i] = *p,
                None => {
                    idx.insert((p.series_id, p.ts_ms), accepted.len());
                    accepted.push(*p);
                }
            }
        }
        drop(reg);
        let rejected = out.rejected_old + out.rejected_future;
        if rejected > 0 {
            self.rejected.fetch_add(rejected as u64, Ordering::Relaxed);
        }
        if accepted.is_empty() {
            return Ok(out);
        }

        let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut app = w.appender("raw_hot")?;
            for p in &accepted {
                app.append_row(params![p.series_id, Value::Timestamp(TimeUnit::Millisecond, p.ts_ms), p.value, seq])?;
            }
            app.flush()?;
        }

        // 集計済みの分に届いたデータは再集計対象として記録
        let dirty: BTreeSet<i64> =
            accepted.iter().filter(|p| p.ts_ms < m1_until).map(|p| floor_to(p.ts_ms, MIN)).collect();
        if !dirty.is_empty() {
            let mut st = w.prepare(&format!("INSERT OR IGNORE INTO dirty_1m VALUES ({})", "make_timestamp(?::BIGINT)"))?;
            for m in &dirty {
                st.execute(params![m * 1000])?;
            }
            debug!(minutes = dirty.len(), "遅れデータを受信し再集計対象に追加");
        }
        drop(w);

        {
            let mut lat = self.latest.write();
            for p in &accepted {
                let e = lat.entry(p.series_id).or_insert((i64::MIN, 0.0));
                if p.ts_ms >= e.0 {
                    *e = (p.ts_ms, p.value);
                }
            }
        }
        out.written = accepted.len();
        self.written.fetch_add(accepted.len() as u64, Ordering::Relaxed);
        Ok(out)
    }

    // ---- メンテナンス ----------------------------------------------------

    fn job(&self, name: &'static str, now: i64, f: impl FnOnce() -> Result<bool>) {
        let t0 = Instant::now();
        let res = f();
        let wall = now_ms();
        let mut jobs = self.jobs.lock();
        let j = jobs.entry(name).or_insert_with(|| JobState { name: name.into(), ..Default::default() });
        j.last_run = Some(wall);
        j.runs += 1;
        match res {
            Ok(did) => {
                j.last_ok = Some(wall);
                j.last_error = None;
                if did {
                    j.last_work = Some(wall);
                    j.last_duration_ms = Some(t0.elapsed().as_millis() as u64);
                }
            }
            Err(e) => {
                warn!(job = name, error = %format!("{e:#}"), "ジョブが失敗しました（次回再実行します）");
                j.last_error = Some(format!("{e:#}"));
                j.last_duration_ms = Some(t0.elapsed().as_millis() as u64);
            }
        }
        let _ = now;
    }

    fn rollup_1m(&self, now: i64) -> Result<bool> {
        let end = floor_to(now - 10 * SEC, MIN);
        let mut did = false;
        loop {
            let w = self.writer.lock();
            let start = self.layout.read().m1_until;
            if start >= end {
                break;
            }
            let chunk_end = end.min(start + 6 * HOUR);
            tx(&w, |c| {
                c.execute(
                    &format!("DELETE FROM r1m_hot WHERE bucket >= {} AND bucket < {}", tsus(start), tsus(chunk_end)),
                    [],
                )?;
                c.execute(
                    &format!(
                        "INSERT INTO r1m_hot SELECT series_id, date_trunc('minute', ts)::TIMESTAMP AS b, {M1_AGG} \
                         FROM ({}) GROUP BY series_id, b",
                        raw_hot_dedup(None, start, chunk_end)
                    ),
                    [],
                )?;
                set_meta(c, "m1_until", chunk_end)
            })?;
            self.layout.write().m1_until = chunk_end;
            did = true;
        }
        Ok(self.process_dirty_1m()? || did)
    }

    fn process_dirty_1m(&self) -> Result<bool> {
        let w = self.writer.lock();
        if count(&w, "SELECT count(*) FROM dirty_1m")? == 0 {
            return Ok(false);
        }
        let h1_until = self.layout.read().h1_until;
        tx(&w, |c| {
            c.execute("DELETE FROM r1m_hot WHERE bucket IN (SELECT bucket FROM dirty_1m)", [])?;
            c.execute(
                &format!(
                    "INSERT INTO r1m_hot SELECT series_id, date_trunc('minute', ts)::TIMESTAMP AS b, {M1_AGG} FROM ( \
                       SELECT series_id, ts, arg_max(value, seq) AS value FROM raw_hot \
                       WHERE date_trunc('minute', ts)::TIMESTAMP IN (SELECT bucket FROM dirty_1m) GROUP BY series_id, ts \
                     ) GROUP BY series_id, b"
                ),
                [],
            )?;
            c.execute(
                &format!(
                    "INSERT OR IGNORE INTO dirty_1h SELECT DISTINCT date_trunc('hour', bucket)::TIMESTAMP FROM dirty_1m WHERE bucket < {}",
                    tsus(h1_until)
                ),
                [],
            )?;
            c.execute("DELETE FROM dirty_1m", [])?;
            Ok(())
        })?;
        Ok(true)
    }

    fn rollup_1h(&self, now: i64) -> Result<bool> {
        let mut did = false;
        loop {
            let w = self.writer.lock();
            let (start, m1_until) = {
                let l = self.layout.read();
                (l.h1_until, l.m1_until)
            };
            let end = floor_to(now - MIN, HOUR).min(floor_to(m1_until, HOUR));
            if start >= end {
                break;
            }
            let chunk_end = end.min(start + 7 * DAY);
            tx(&w, |c| {
                c.execute(&format!("DELETE FROM r1h WHERE bucket >= {} AND bucket < {}", tsus(start), tsus(chunk_end)), [])?;
                c.execute(
                    &format!(
                        "INSERT INTO r1h SELECT series_id, date_trunc('hour', bucket)::TIMESTAMP AS b, {UP_AGG} \
                         FROM r1m_hot WHERE bucket >= {} AND bucket < {} GROUP BY series_id, b",
                        tsus(start),
                        tsus(chunk_end)
                    ),
                    [],
                )?;
                set_meta(c, "h1_until", chunk_end)
            })?;
            self.layout.write().h1_until = chunk_end;
            did = true;
        }

        let w = self.writer.lock();
        if count(&w, "SELECT count(*) FROM dirty_1h")? > 0 {
            tx(&w, |c| {
                c.execute("DELETE FROM r1h WHERE bucket IN (SELECT bucket FROM dirty_1h)", [])?;
                c.execute(
                    &format!(
                        "INSERT INTO r1h SELECT series_id, date_trunc('hour', bucket)::TIMESTAMP AS b, {UP_AGG} FROM r1m_hot \
                         WHERE date_trunc('hour', bucket)::TIMESTAMP IN (SELECT bucket FROM dirty_1h) GROUP BY series_id, b"
                    ),
                    [],
                )?;
                c.execute("DELETE FROM dirty_1h", [])?;
                Ok(())
            })?;
            did = true;
        }
        Ok(did)
    }

    fn flush_days(&self, now: i64) -> Result<bool> {
        let mut did = false;
        loop {
            let (day, accept, m1_until) = {
                let l = self.layout.read();
                (l.raw_flushed_until, l.raw_accept_from, l.m1_until)
            };
            let next = day + DAY;
            if next + self.opt.grace_days * DAY > now || next > m1_until {
                break;
            }
            // 1. この日の受け付けを締め切る（以後この日のデータは拒否される）
            if accept < next {
                let w = self.writer.lock();
                let mut l = self.layout.write();
                tx(&w, |c| set_meta(c, "raw_accept_from", next))?;
                l.raw_accept_from = next;
            }
            // 締め切り直前に届いた遅れデータを集計に反映
            self.process_dirty_1m()?;

            // 2. Parquet へ書き出し（この日の行はもう変化しないのでロック不要）
            let compact = self.opt.compact_after_days > 0 && day < floor_to(now, DAY) - self.opt.compact_after_days * DAY;
            {
                let r = self.reader();
                let n = count(&r, &format!("SELECT count(*) FROM raw_hot WHERE ts >= {} AND ts < {}", tsms(day), tsms(next)))?;
                if n > 0 {
                    let t0 = Instant::now();
                    let rows = self.write_raw_day(&r, &raw_hot_dedup(None, day, next), day, compact)?;
                    info!(day = %day_str(day), rows, ms = t0.elapsed().as_millis() as u64, "日次フラッシュ");
                }
            }

            // 3. raw_hot から削除して境界を進める
            {
                let w = self.writer.lock();
                let mut l = self.layout.write();
                tx(&w, |c| {
                    c.execute(&format!("DELETE FROM raw_hot WHERE ts < {}", tsms(next)), [])?;
                    set_meta(c, "raw_flushed_until", next)?;
                    // 再圧縮の境界は連続して進める（手前の日が未処理なら圧縮ジョブに任せる）
                    if compact && l.raw_compacted_until == day {
                        set_meta(c, "raw_compacted_until", next)?;
                    }
                    Ok(())
                })?;
                l.raw_flushed_until = next;
                if compact && l.raw_compacted_until == day {
                    l.raw_compacted_until = next;
                }
            }
            did = true;
        }
        if did {
            self.writer.lock().execute_batch("CHECKPOINT")?;
        }
        Ok(did)
    }

    fn flush_months(&self) -> Result<bool> {
        let mut did = false;
        loop {
            let (m, raw_flushed, h1_until) = {
                let l = self.layout.read();
                (l.m1_flushed_until, l.raw_flushed_until, l.h1_until)
            };
            let nm = next_month(m);
            if nm > raw_flushed || nm > h1_until {
                break;
            }
            {
                let r = self.reader();
                let pending = count(&r, &format!("SELECT count(*) FROM dirty_1m WHERE bucket < {}", tsus(nm)))?
                    + count(&r, &format!("SELECT count(*) FROM dirty_1h WHERE bucket < {}", tsus(nm)))?;
                if pending > 0 {
                    break;
                }
                let n = count(&r, &format!("SELECT count(*) FROM r1m_hot WHERE bucket >= {} AND bucket < {}", tsus(m), tsus(nm)))?;
                if n > 0 {
                    let rows = copy_to_parquet(
                        &r,
                        &format!(
                            "SELECT * FROM r1m_hot WHERE bucket >= {} AND bucket < {} ORDER BY series_id, bucket",
                            tsus(m),
                            tsus(nm)
                        ),
                        &self.m1_path(m),
                    )?;
                    info!(month = %month_str(m), rows, "月次フラッシュ");
                }
            }
            {
                let w = self.writer.lock();
                let mut l = self.layout.write();
                tx(&w, |c| {
                    c.execute(&format!("DELETE FROM r1m_hot WHERE bucket < {}", tsus(nm)), [])?;
                    set_meta(c, "m1_flushed_until", nm)
                })?;
                l.m1_flushed_until = nm;
            }
            did = true;
        }
        Ok(did)
    }

    /// 1年（設定値）より古い日次ファイルを 16bit 形式に再圧縮する。1回あたり最大10秒。
    fn compact(&self, now: i64) -> Result<bool> {
        if self.opt.compact_after_days <= 0 {
            return Ok(false);
        }
        let limit = floor_to(now, DAY) - self.opt.compact_after_days * DAY;
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut did = false;
        loop {
            let (d, flushed) = {
                let l = self.layout.read();
                (l.raw_compacted_until, l.raw_flushed_until)
            };
            if d >= limit || d >= flushed || Instant::now() > deadline {
                break;
            }
            let plain = self.raw_path(d);
            let q = self.raw_q16_path(d);
            if plain.exists() && !q.exists() {
                let r = self.reader();
                let src = format!("SELECT series_id, ts, value FROM read_parquet({})", sql_path(&plain));
                let rows = copy_to_parquet(&r, &q16_sql(&src), &q)?;
                debug!(day = %day_str(d), rows, "16bit に再圧縮");
            }
            {
                let w = self.writer.lock();
                let mut l = self.layout.write();
                if q.exists() && plain.exists() {
                    std::fs::remove_file(&plain).with_context(|| format!("削除できません: {}", plain.display()))?;
                }
                tx(&w, |c| set_meta(c, "raw_compacted_until", d + DAY))?;
                l.raw_compacted_until = d + DAY;
            }
            did = true;
        }
        Ok(did)
    }

    fn retention(&self, now: i64) -> Result<bool> {
        let today = floor_to(now, DAY);
        if self.last_retention_day.load(Ordering::Relaxed) == today {
            return Ok(false);
        }
        let cut_raw = today - self.opt.raw_days * DAY;
        let cut_m1 = now - self.opt.m1_days * DAY;
        let cut_h1 = now - self.opt.h1_days * DAY;
        let mut removed = 0usize;
        {
            let w = self.writer.lock();
            let _l = self.layout.write();
            for (day, path) in list_raw_files(&self.opt.data_dir) {
                if day < cut_raw {
                    std::fs::remove_file(&path).with_context(|| format!("削除できません: {}", path.display()))?;
                    removed += 1;
                }
            }
            remove_empty_dirs(&self.opt.data_dir.join("raw"));
            for (month, path) in list_m1_files(&self.opt.data_dir) {
                if next_month(month) <= cut_m1 {
                    std::fs::remove_file(&path)?;
                    removed += 1;
                }
            }
            let n = w.execute(&format!("DELETE FROM r1h WHERE bucket < {}", tsus(cut_h1)), [])?;
            if removed > 0 || n > 0 {
                info!(files = removed, r1h_rows = n, "保持期間を過ぎたデータを削除");
            }
        }
        self.last_retention_day.store(today, Ordering::Relaxed);
        Ok(removed > 0)
    }

    // ---- クエリ ------------------------------------------------------------

    fn raw_files(&self, a: i64, b: i64) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut plain = vec![];
        let mut q16 = vec![];
        let mut d = floor_to(a, DAY);
        while d < b {
            let q = self.raw_q16_path(d);
            if q.exists() {
                q16.push(q);
            } else {
                let p = self.raw_path(d);
                if p.exists() {
                    plain.push(p);
                }
            }
            d += DAY;
        }
        (plain, q16)
    }

    fn m1_files(&self, a: i64, b: i64) -> Vec<PathBuf> {
        let mut out = vec![];
        let mut m = month_start(a);
        while m < b {
            let p = self.m1_path(m);
            if p.exists() {
                out.push(p);
            }
            m = next_month(m);
        }
        out
    }

    /// 層ごとの読み出し元を組み立てる。
    /// 各行は (series_id, t BIGINT ms, vmin FLOAT, vmax FLOAT, vsum DOUBLE, vcount BIGINT)。
    fn source_sql(&self, tier: Tier, l: &Watermarks, ids: &[SeriesId], a: i64, b: i64) -> Vec<String> {
        let idl = id_list(ids);
        let mut parts = vec![];

        let raw_rows = |inner: String| {
            format!("SELECT series_id, epoch_ms(ts::TIMESTAMP) AS t, value AS vmin, value AS vmax, value::DOUBLE AS vsum, 1::BIGINT AS vcount FROM ({inner})")
        };
        let raw_hot_part = |from: i64, to: i64| raw_rows(raw_hot_dedup(Some(ids), from, to));
        let rollup_rows = |src: String, from: i64, to: i64| {
            format!(
                "SELECT series_id, epoch_ms(bucket) AS t, vmin, vmax, vsum, vcount::BIGINT AS vcount FROM {src} \
                 WHERE series_id IN ({idl}) AND bucket >= {} AND bucket < {}",
                tsus(from),
                tsus(to)
            )
        };

        match tier {
            Tier::Raw => {
                let pb = b.min(l.raw_flushed_until);
                if a < pb {
                    let (plain, q16) = self.raw_files(a, pb);
                    let w = format!("series_id IN ({idl}) AND ts >= {} AND ts < {}", tsms(a), tsms(pb));
                    if !plain.is_empty() {
                        parts.push(raw_rows(format!("SELECT series_id, ts, value FROM read_parquet({}) WHERE {w}", file_list(&plain))));
                    }
                    if !q16.is_empty() {
                        parts.push(raw_rows(format!(
                            "SELECT series_id, ts, (q_off + q * q_scale)::FLOAT AS value FROM read_parquet({}) WHERE {w}",
                            file_list(&q16)
                        )));
                    }
                }
                let ha = a.max(l.raw_flushed_until);
                if ha < b {
                    parts.push(raw_hot_part(ha, b));
                }
            }
            Tier::M1 => {
                let pb = b.min(l.m1_flushed_until);
                if a < pb {
                    let files = self.m1_files(a, pb);
                    if !files.is_empty() {
                        parts.push(rollup_rows(format!("read_parquet({})", file_list(&files)), a, pb));
                    }
                }
                let ha = a.max(l.m1_flushed_until);
                let hb = b.min(l.m1_until);
                if ha < hb {
                    parts.push(rollup_rows("r1m_hot".into(), ha, hb));
                }
                let ra = a.max(l.m1_until);
                if ra < b {
                    parts.push(raw_hot_part(ra, b));
                }
            }
            Tier::H1 => {
                let hb = b.min(l.h1_until);
                if a < hb {
                    parts.push(rollup_rows("r1h".into(), a, hb));
                }
                let ma = a.max(l.h1_until);
                let mb = b.min(l.m1_until);
                if ma < mb {
                    parts.push(rollup_rows("r1m_hot".into(), ma, mb));
                }
                let ra = a.max(l.m1_until);
                if ra < b {
                    parts.push(raw_hot_part(ra, b));
                }
            }
        }
        parts
    }

    pub fn choose(span_ms: i64, width: u32, forced: Option<Tier>) -> (Tier, i64) {
        let width = width.clamp(1, 10_000) as i64;
        let raw_bucket = ((span_ms + width - 1) / width).max(1);
        let tier = forced.unwrap_or(if raw_bucket < MIN {
            Tier::Raw
        } else if raw_bucket < HOUR {
            Tier::M1
        } else {
            Tier::H1
        });
        let res = tier.resolution_ms();
        let bucket = ((raw_bucket.max(res) + res - 1) / res) * res;
        (tier, bucket)
    }
}

/// 欠測区間に null を挟む。間隔の中央値と比べて明らかに空いている所だけを欠測とみなす。
fn insert_gaps(tier: Tier, bucket: i64, s: &mut SeriesAgg) {
    if s.t.len() < 2 {
        return;
    }
    let mut deltas: Vec<i64> = s.t.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_unstable();
    let median = deltas[deltas.len() / 2];
    let thr = if tier == Tier::Raw || median > bucket { (2 * bucket).max(3 * median) } else { 2 * bucket };

    let n = s.t.len();
    let mut t = Vec::with_capacity(n + 8);
    let mut mn = Vec::with_capacity(n + 8);
    let mut mx = Vec::with_capacity(n + 8);
    let mut av = Vec::with_capacity(n + 8);
    for i in 0..n {
        if i > 0 && s.t[i] - s.t[i - 1] >= thr {
            t.push(s.t[i - 1] + bucket);
            mn.push(None);
            mx.push(None);
            av.push(None);
        }
        t.push(s.t[i]);
        mn.push(s.min[i]);
        mx.push(s.max[i]);
        av.push(s.avg[i]);
    }
    s.t = t;
    s.min = mn;
    s.max = mx;
    s.avg = av;
}

impl Store for DuckStore {
    fn resolve_series(&self, keys: &[String]) -> Result<Vec<SeriesId>> {
        let mut out = Vec::with_capacity(keys.len());
        let mut missing = vec![];
        {
            let r = self.registry.read();
            for (i, k) in keys.iter().enumerate() {
                match r.by_key.get(k) {
                    Some(id) => out.push(*id),
                    None => {
                        out.push(0);
                        missing.push(i);
                    }
                }
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }
        let w = self.writer.lock();
        let mut reg = self.registry.write();
        let now = now_ms();
        for i in missing {
            let key = &keys[i];
            if let Some(id) = reg.by_key.get(key) {
                out[i] = *id;
                continue;
            }
            let (measurement, tags, field) =
                lp::parse_series_key(key).ok_or_else(|| anyhow!("不正な系列キー: {key}"))?;
            let id = reg.next_id;
            w.execute(
                &format!("INSERT INTO series VALUES (?, ?, ?, ?, {}, NULL, make_timestamp(?::BIGINT))", map_literal(&tags)),
                params![id, key, measurement, field, now * 1000],
            )?;
            reg.next_id += 1;
            reg.by_key.insert(key.clone(), id);
            reg.by_id.insert(
                id,
                Series { id, key: key.clone(), measurement, field, tags: tags.into_iter().collect(), unit: None, created_at: now },
            );
            out[i] = id;
            info!(id, key = %key, "新しい系列を登録");
        }
        Ok(out)
    }

    fn write_batch(&self, points: &[Point]) -> Result<WriteOutcome> {
        self.write_points_at(points, now_ms())
    }

    fn query_agg(&self, q: &AggQuery) -> Result<AggResult> {
        let t0 = Instant::now();
        if q.to_ms <= q.from_ms {
            bail!("to は from より後にしてください");
        }
        if q.series.len() > 500 {
            bail!("系列が多すぎます（最大500）");
        }
        let (tier, bucket) = Self::choose(q.to_ms - q.from_ms, q.width, q.tier);
        let a = floor_to(q.from_ms, bucket);
        let b = q.to_ms;

        let mut by_id: HashMap<SeriesId, SeriesAgg> = q
            .series
            .iter()
            .map(|&id| (id, SeriesAgg { id, t: vec![], min: vec![], max: vec![], avg: vec![] }))
            .collect();

        // 削除済みの系列は読まない（Parquet には保持期間まで行が残る）
        let known = self.known_ids(&q.series);
        if !known.is_empty() {
            let l = self.layout.read();
            let parts = self.source_sql(tier, &l, &known, a, b);
            if !parts.is_empty() {
                let sql = format!(
                    "SELECT series_id, (t // {bucket}) * {bucket} AS bt, min(vmin), max(vmax), (sum(vsum) / sum(vcount))::FLOAT \
                     FROM ({}) GROUP BY ALL ORDER BY series_id, bt",
                    parts.join(" UNION ALL ")
                );
                let r = self.reader();
                let mut st = r.prepare(&sql)?;
                let mut rows = st.query([])?;
                while let Some(row) = rows.next()? {
                    let id: u32 = row.get(0)?;
                    if let Some(s) = by_id.get_mut(&id) {
                        s.t.push(row.get(1)?);
                        s.min.push(row.get(2)?);
                        s.max.push(row.get(3)?);
                        s.avg.push(row.get(4)?);
                    }
                }
            }
        }

        let mut series = Vec::with_capacity(q.series.len());
        for id in &q.series {
            if let Some(mut s) = by_id.remove(id) {
                insert_gaps(tier, bucket, &mut s);
                series.push(s);
            }
        }
        Ok(AggResult {
            tier,
            bucket_ms: bucket,
            band: bucket > tier.resolution_ms() || tier != Tier::Raw,
            from_ms: q.from_ms,
            to_ms: q.to_ms,
            series,
            elapsed_ms: t0.elapsed().as_secs_f64() * 1000.0,
        })
    }

    fn latest(&self, series: &[SeriesId]) -> Result<Vec<Latest>> {
        let lat = self.latest.read();
        let mut out: Vec<Latest> = if series.is_empty() {
            lat.iter().map(|(&id, &(ts, value))| Latest { id, ts, value }).collect()
        } else {
            series.iter().filter_map(|id| lat.get(id).map(|&(ts, value)| Latest { id: *id, ts, value })).collect()
        };
        if series.is_empty() {
            out.sort_by_key(|l| l.id);
        }
        Ok(out)
    }

    fn list_series(&self, filter: &SeriesFilter) -> Result<Vec<Series>> {
        let reg = self.registry.read();
        let it = reg.by_id.values().filter(|s| filter.matches(s)).cloned();
        Ok(match filter.limit {
            Some(n) => it.take(n).collect(),
            None => it.collect(),
        })
    }

    fn set_series_unit(&self, id: SeriesId, unit: Option<String>) -> Result<()> {
        let w = self.writer.lock();
        let mut reg = self.registry.write();
        let s = reg.by_id.get_mut(&id).ok_or_else(|| anyhow!("系列 {id} がありません"))?;
        let unit = unit.filter(|u| !u.trim().is_empty());
        w.execute("UPDATE series SET unit = ? WHERE id = ?", params![unit, id])?;
        s.unit = unit;
        Ok(())
    }

    fn rename_series(&self, id: SeriesId, new_key: &str) -> Result<Series> {
        let (measurement, tags, field) = lp::parse_series_key(new_key.trim())
            .ok_or_else(|| anyhow!("系列キーの形式が不正です: '{new_key}'（例: plc,device=dev01 temp）"))?;
        let key = lp::series_key(&measurement, &tags, &field);
        let w = self.writer.lock();
        let mut reg = self.registry.write();
        let old = reg.by_id.get(&id).cloned().ok_or_else(|| anyhow!("系列 {id} がありません"))?;
        if old.key == key {
            return Ok(old);
        }
        if let Some(other) = reg.by_key.get(&key) {
            bail!("系列キー '{key}' は既に使われています（ID {other}）。統合する場合は long 形式でエクスポートし、キーを書き換えて取り込んでから削除してください");
        }
        w.execute(
            &format!("UPDATE series SET key = ?, measurement = ?, field = ?, tags = {} WHERE id = ?", map_literal(&tags)),
            params![key, measurement, field, id],
        )?;
        reg.by_key.remove(&old.key);
        reg.by_key.insert(key.clone(), id);
        let s = reg.by_id.get_mut(&id).unwrap();
        s.key = key;
        s.measurement = measurement;
        s.field = field;
        s.tags = tags.into_iter().collect();
        info!(id, from = %old.key, to = %s.key, "系列の名前を変更");
        Ok(s.clone())
    }

    fn delete_series(&self, ids: &[SeriesId]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let w = self.writer.lock();
        let mut reg = self.registry.write();
        let ids: Vec<SeriesId> = ids.iter().copied().filter(|id| reg.by_id.contains_key(id)).collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let list = id_list(&ids);
        let next = reg.next_id;
        tx(&w, |c| {
            c.execute(&format!("DELETE FROM series WHERE id IN ({list})"), [])?;
            c.execute(&format!("DELETE FROM raw_hot WHERE series_id IN ({list})"), [])?;
            c.execute(&format!("DELETE FROM r1m_hot WHERE series_id IN ({list})"), [])?;
            c.execute(&format!("DELETE FROM r1h WHERE series_id IN ({list})"), [])?;
            set_meta(c, "next_series_id", next as i64)
        })?;
        for id in &ids {
            if let Some(s) = reg.by_id.remove(id) {
                reg.by_key.remove(&s.key);
                info!(id, key = %s.key, "系列を削除");
            }
        }
        drop(reg);
        let mut lat = self.latest.write();
        for id in &ids {
            lat.remove(id);
        }
        Ok(ids.len())
    }

    fn run_maintenance(&self, now: i64) -> Result<()> {
        let Some(_g) = self.maint.try_lock() else {
            return Ok(());
        };
        self.job("rollup_1m", now, || self.rollup_1m(now));
        self.job("rollup_1h", now, || self.rollup_1h(now));
        self.job("flush_daily", now, || self.flush_days(now));
        self.job("flush_monthly", now, || self.flush_months());
        self.job("compact_16bit", now, || self.compact(now));
        self.job("retention", now, || self.retention(now));
        Ok(())
    }

    fn status(&self) -> Result<StoreStatus> {
        // 接続プールを持ったまま layout を待たない（クエリとのデッドロック防止）
        let watermarks = self.layout.read().clone();
        let series_count = self.registry.read().by_id.len();
        let jobs = self.jobs.lock().values().cloned().collect();
        let r = self.reader();
        Ok(StoreStatus {
            series_count,
            raw_hot_rows: count(&r, "SELECT count(*) FROM raw_hot")?,
            r1m_hot_rows: count(&r, "SELECT count(*) FROM r1m_hot")?,
            r1h_rows: count(&r, "SELECT count(*) FROM r1h")?,
            db_file_bytes: std::fs::metadata(&self.db_path).map(|m| m.len()).unwrap_or(0)
                + std::fs::metadata(self.db_path.with_extension("duckdb.wal")).map(|m| m.len()).unwrap_or(0),
            watermarks,
            jobs,
            written_points: self.written.load(Ordering::Relaxed),
            rejected_points: self.rejected.load(Ordering::Relaxed),
        })
    }

    fn export(&self, req: &ExportRequest, out: &Path) -> Result<u64> {
        if req.to_ms <= req.from_ms {
            bail!("to は from より後にしてください");
        }
        let known = self.known_ids(&req.series);
        if known.is_empty() {
            bail!("系列を選んでください");
        }
        let l = self.layout.read();
        let parts = self.source_sql(req.tier, &l, &known, req.from_ms, req.to_ms);
        let src = if parts.is_empty() {
            "SELECT 0::UINTEGER AS series_id, 0::BIGINT AS t, 0::FLOAT AS vmin, 0::FLOAT AS vmax, 0::DOUBLE AS vsum, 0::BIGINT AS vcount WHERE false".to_string()
        } else {
            parts.join(" UNION ALL ")
        };
        let select = match req.tier {
            Tier::Raw => format!(
                "SELECT make_timestamp(src.t * 1000) AS time_utc, s.key AS series, src.vmin AS value \
                 FROM ({src}) src JOIN series s ON s.id = src.series_id ORDER BY time_utc, series"
            ),
            t => {
                let res = t.resolution_ms();
                format!(
                    "SELECT make_timestamp(src.bt * 1000) AS time_utc, s.key AS series, src.vmin AS min, src.vmax AS max, src.avg, src.n AS count \
                     FROM (SELECT series_id, (t // {res}) * {res} AS bt, min(vmin) AS vmin, max(vmax) AS vmax, \
                           (sum(vsum) / sum(vcount))::FLOAT AS avg, sum(vcount)::BIGINT AS n FROM ({src}) GROUP BY ALL) src \
                     JOIN series s ON s.id = src.series_id ORDER BY time_utc, series"
                )
            }
        };
        let opts = match req.format {
            ExportFormat::Csv => "FORMAT csv, HEADER",
            ExportFormat::Parquet => "FORMAT parquet, COMPRESSION zstd",
        };
        if let Some(d) = out.parent() {
            std::fs::create_dir_all(d)?;
        }
        let r = self.reader();
        let n = r.execute(&format!("COPY ({select}) TO {} ({opts})", sql_path(out)), [])?;
        Ok(n as u64)
    }

    fn backup(&self, dest: &Path) -> Result<BackupReport> {
        let t0 = Instant::now();
        let mut rep = BackupReport::default();
        std::fs::create_dir_all(dest)?;
        // 比較だけ正規化したパスで行う（Windows の \\?\ 付きパスを DuckDB に渡さない）
        let canon = dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf());
        if canon.starts_with(self.opt.data_dir.canonicalize().unwrap_or_else(|_| self.opt.data_dir.clone())) {
            bail!("バックアップ先をデータフォルダの中にはできません");
        }
        let dest = dest.to_path_buf();

        // DuckDB ファイル: CHECKPOINT してから DuckDB 自身にコピーさせる（開いたままのファイルを直接コピーしない）
        let tmp = dest.join("tsd.duckdb.tmp");
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(dest.join("tsd.duckdb.tmp.wal"));
        {
            let w = self.writer.lock();
            w.execute_batch("CHECKPOINT")?;
            let db: String = w.query_row("SELECT current_database()", [], |r| r.get(0))?;
            w.execute_batch(&format!(
                "ATTACH {} AS tsd_backup; COPY FROM DATABASE \"{}\" TO tsd_backup; DETACH tsd_backup;",
                sql_path(&tmp),
                db.replace('"', "\"\"")
            ))?;
        }
        std::fs::rename(&tmp, dest.join("tsd.duckdb"))?;
        rep.db_bytes = std::fs::metadata(dest.join("tsd.duckdb")).map(|m| m.len()).unwrap_or(0);

        // Parquet などは書き出し後に変更しないので差分コピー
        for sub in ["raw", "1m", "dashboards"] {
            let src_root = self.opt.data_dir.join(sub);
            for f in walk_files(&src_root) {
                let name = f.to_string_lossy();
                if name.ends_with(".tmp") {
                    continue;
                }
                let rel = f.strip_prefix(&self.opt.data_dir).unwrap_or(&f);
                let target = dest.join(rel);
                let src_len = match std::fs::metadata(&f) {
                    Ok(m) => m.len(),
                    Err(_) => continue, // 保持期間管理で消えた
                };
                let same = std::fs::metadata(&target).map(|m| m.len() == src_len).unwrap_or(false);
                if same && sub != "dashboards" {
                    rep.files_skipped += 1;
                    continue;
                }
                if let Some(d) = target.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let tmp = PathBuf::from(format!("{}.tmp", target.to_string_lossy()));
                match std::fs::copy(&f, &tmp) {
                    Ok(n) => {
                        std::fs::rename(&tmp, &target)?;
                        rep.files_copied += 1;
                        rep.bytes_copied += n;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        rep.elapsed_ms = t0.elapsed().as_millis() as u64;
        info!(?rep, dest = %dest.display(), "バックアップ完了");
        Ok(rep)
    }

    fn checkpoint(&self) -> Result<()> {
        self.writer.lock().execute_batch("CHECKPOINT")?;
        Ok(())
    }

    fn import_csv(&self, path: &Path, opt: &CsvImportOptions) -> Result<ImportReport> {
        self.import_csv_file(path, opt)
    }
}

// ---- スキーマ ---------------------------------------------------------------

fn migrate(c: &Connection) -> Result<()> {
    c.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT)")?;
    let v: i64 = c.query_row("SELECT coalesce(max(version), 0) FROM schema_version", [], |r| r.get(0))?;
    if v > SCHEMA_VERSION {
        bail!("DB のスキーマ版 {v} はこのプログラム（{SCHEMA_VERSION}）より新しいため開けません");
    }
    if v < 1 {
        tx(c, |c| {
            c.execute_batch(
                "CREATE TABLE meta (key VARCHAR PRIMARY KEY, value BIGINT);
                 CREATE TABLE series (
                   id UINTEGER PRIMARY KEY, key VARCHAR UNIQUE, measurement VARCHAR, field VARCHAR,
                   tags MAP(VARCHAR, VARCHAR), unit VARCHAR, created_at TIMESTAMP);
                 CREATE TABLE raw_hot (series_id UINTEGER, ts TIMESTAMP_MS, value FLOAT, seq UBIGINT);
                 CREATE TABLE r1m_hot (series_id UINTEGER, bucket TIMESTAMP, vmin FLOAT, vmax FLOAT,
                                       vsum DOUBLE, vcount UINTEGER, vfirst FLOAT, vlast FLOAT);
                 CREATE TABLE r1h (series_id UINTEGER, bucket TIMESTAMP, vmin FLOAT, vmax FLOAT,
                                   vsum DOUBLE, vcount UINTEGER, vfirst FLOAT, vlast FLOAT);
                 CREATE TABLE dirty_1m (bucket TIMESTAMP PRIMARY KEY);
                 CREATE TABLE dirty_1h (bucket TIMESTAMP PRIMARY KEY);
                 INSERT INTO schema_version VALUES (1);",
            )?;
            Ok(())
        })?;
        info!("DB スキーマを作成しました（版 1）");
    }
    // 版 2 以降の移行はここに追加する
    Ok(())
}

// ---- ファイル操作 -----------------------------------------------------------

pub fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(t) if t.is_file() => out.push(p),
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// 生データの日次ファイル（通常形式と 16bit 形式の両方）
pub fn list_raw_files(data_dir: &Path) -> Vec<(i64, PathBuf)> {
    walk_files(&data_dir.join("raw"))
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            let day = name.strip_suffix(".q16.parquet").or_else(|| name.strip_suffix(".parquet"))?;
            Some((parse_day(day)?, p))
        })
        .collect()
}

pub fn list_m1_files(data_dir: &Path) -> Vec<(i64, PathBuf)> {
    walk_files(&data_dir.join("1m"))
        .into_iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?.to_string();
            Some((parse_month(name.strip_suffix(".parquet")?)?, p))
        })
        .collect()
}

fn remove_empty_dirs(root: &Path) {
    fn rec(d: &Path, is_root: bool) -> bool {
        let Ok(rd) = std::fs::read_dir(d) else { return false };
        let mut empty = true;
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if !rec(&p, false) {
                    empty = false;
                }
            } else {
                empty = false;
            }
        }
        if empty && !is_root {
            return std::fs::remove_dir(d).is_ok();
        }
        empty
    }
    rec(root, true);
}

/// 起動時: 書きかけの一時ファイルを消し、再圧縮の途中で止まった日の古い形式を消す。
fn cleanup_partial_files(data_dir: &Path) {
    for sub in ["raw", "1m"] {
        for f in walk_files(&data_dir.join(sub)) {
            let s = f.to_string_lossy();
            if s.ends_with(".tmp") {
                warn!(file = %f.display(), "書きかけの一時ファイルを削除");
                let _ = std::fs::remove_file(&f);
            } else if let Some(base) = s.strip_suffix(".q16.parquet") {
                let plain = PathBuf::from(format!("{base}.parquet"));
                if plain.exists() {
                    let _ = std::fs::remove_file(plain);
                }
            }
        }
    }
}
