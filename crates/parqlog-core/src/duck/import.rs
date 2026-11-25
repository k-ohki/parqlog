//! CSV の取り込み。
//!
//! - wide: `時刻,列1,列2,...`。列名を field、指定した measurement とタグで系列にする。
//! - long: `時刻,系列キー,値`（エクスポートした CSV と同じ形）。
//!
//! 時刻はエポック秒・エポックミリ秒・日時文字列を受け付ける。タイムゾーンの無い日時は
//! `utc_offset_minutes` の地方時として扱う。列名が `_utc` で終わる場合は UTC とみなす。
//!
//! 確定済み（Parquet 化済み）の日に当たるデータは、その日のファイルと1分・1時間集計を作り直す。
//! 同じ (系列, 時刻) は取り込んだ値で上書きするので、同じファイルを何度取り込んでも結果は同じ。

use super::*;

static IMPORT_SEQ: AtomicU64 = AtomicU64::new(0);

fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// 時刻文字列 → エポックミリ秒（読めなければ NULL）
fn time_expr(col: &str, offset_ms: i64, assume_utc: bool) -> String {
    let c = format!("trim({})", ident(col));
    let off = if assume_utc { 0 } else { offset_ms };
    let naive = format!(
        "coalesce(TRY_CAST(replace({c}, '/', '-') AS TIMESTAMP), \
         try_strptime({c}, ['%Y/%m/%d %H:%M:%S', '%Y/%m/%d %H:%M', '%Y/%m/%d', '%Y-%m-%d %H:%M:%S', '%Y-%m-%d %H:%M', '%Y-%m-%d']))"
    );
    format!(
        "CASE \
           WHEN TRY_CAST({c} AS DOUBLE) IS NOT NULL THEN \
             CASE WHEN abs(TRY_CAST({c} AS DOUBLE)) >= 1e11 THEN TRY_CAST({c} AS DOUBLE)::BIGINT \
                  ELSE (TRY_CAST({c} AS DOUBLE) * 1000)::BIGINT END \
           WHEN regexp_matches({c}, '(Z|[+-][0-9][0-9]:?[0-9][0-9])$') THEN \
             epoch_ms(TRY_CAST(replace({c}, '/', '-') AS TIMESTAMPTZ)::TIMESTAMP) \
           ELSE epoch_ms({naive}) - {off} \
         END"
    )
}

impl DuckStore {
    /// CSV ファイルを取り込む。取り込み中は定期ジョブを止める。
    pub fn import_csv_file(&self, path: &Path, opt: &CsvImportOptions) -> Result<ImportReport> {
        if !path.exists() {
            bail!("ファイルがありません: {}", path.display());
        }
        let _m = self.maint.lock();
        let t0 = Instant::now();
        let tbl = format!("_imp{}", IMPORT_SEQ.fetch_add(1, Ordering::Relaxed));
        // 取り込み専用の接続（一時テーブルはこの接続だけに見える）
        let c = self.writer.lock().try_clone()?;
        let res = self.import_inner(&c, path, opt, &tbl, now_ms());
        for suffix in ["_csv", "_t", "_map", "", "_day", "_1m", "_days"] {
            let _ = c.execute_batch(&format!("DROP TABLE IF EXISTS {tbl}{suffix}"));
        }
        let mut r = res?;
        r.elapsed_ms = t0.elapsed().as_millis() as u64;
        if r.backfilled_days > 0 {
            self.checkpoint()?;
        }
        info!(?r, file = %path.display(), "CSV を取り込みました");
        Ok(r)
    }

    fn import_inner(&self, c: &Connection, path: &Path, opt: &CsvImportOptions, tbl: &str, now: i64) -> Result<ImportReport> {
        let mut rep = ImportReport::default();
        c.execute_batch(&format!(
            "CREATE TEMP TABLE {tbl}_csv AS SELECT * FROM read_csv({}, header = true, all_varchar = true)",
            sql_path(path)
        ))
        .context("CSV を読めません（1行目は列名にしてください）")?;
        let cols: Vec<String> = {
            let mut st = c.prepare(&format!("SELECT column_name FROM (DESCRIBE {tbl}_csv)"))?;
            let rows = st.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        if cols.len() < 2 {
            bail!("列が2つ以上必要です（時刻と値）");
        }
        let tcol = match &opt.time_column {
            Some(t) => cols.iter().find(|c| *c == t).cloned().ok_or_else(|| anyhow!("時刻の列 '{t}' がありません"))?,
            None => cols[0].clone(),
        };
        let assume_utc = tcol.to_lowercase().ends_with("utc");
        let texpr = time_expr(&tcol, opt.utc_offset_minutes as i64 * MIN, assume_utc);

        rep.rows = count(c, &format!("SELECT count(*) FROM {tbl}_csv"))?;
        c.execute_batch(&format!(
            "CREATE TEMP TABLE {tbl}_t AS SELECT ({texpr})::BIGINT AS __t, * EXCLUDE ({}) FROM {tbl}_csv",
            ident(&tcol)
        ))?;
        rep.skipped_rows = count(c, &format!("SELECT count(*) FROM {tbl}_t WHERE __t IS NULL"))?;

        // 列 → 系列の対応表
        c.execute_batch(&format!("CREATE TEMP TABLE {tbl}_map (name VARCHAR, sid UINTEGER)"))?;
        let (cells_sql, name_of) = match opt.layout {
            CsvLayout::Wide => {
                // 数値が1つも無い列（状態の文字列など）は系列にしない
                let cand: Vec<&String> = cols.iter().filter(|c| **c != tcol).collect();
                let counts_sql = cand
                    .iter()
                    .map(|c| format!("count(TRY_CAST(replace(trim({}), ',', '') AS FLOAT))", ident(c)))
                    .collect::<Vec<_>>()
                    .join(", ");
                let counts: Vec<i64> = c.query_row(&format!("SELECT {counts_sql} FROM {tbl}_csv"), [], |r| {
                    (0..cand.len()).map(|i| r.get::<_, i64>(i)).collect()
                })?;
                let fields: Vec<&String> = cand.iter().zip(&counts).filter(|(_, n)| **n > 0).map(|(c, _)| *c).collect();
                if fields.is_empty() {
                    bail!("数値の列がありません");
                }
                let mut tags = opt.tags.clone();
                tags.sort();
                let m = if opt.measurement.trim().is_empty() { "csv" } else { opt.measurement.trim() };
                let keys: Vec<String> = fields.iter().map(|f| lp::series_key(m, &tags, f.trim())).collect();
                let ids = self.resolve_series(&keys)?;
                let mut st = c.prepare(&format!("INSERT INTO {tbl}_map VALUES (?, ?)"))?;
                for (f, id) in fields.iter().zip(&ids) {
                    st.execute(params![f.as_str(), id])?;
                }
                (
                    format!("SELECT __t, name, v FROM (UNPIVOT {tbl}_t ON COLUMNS(* EXCLUDE (__t)) INTO NAME name VALUE v)"),
                    "name",
                )
            }
            CsvLayout::Long => {
                let rest: Vec<&String> = cols.iter().filter(|c| **c != tcol).collect();
                let pick = |names: &[&str], idx: usize| -> Result<String> {
                    names
                        .iter()
                        .find_map(|n| rest.iter().find(|c| c.eq_ignore_ascii_case(n)))
                        .or_else(|| rest.get(idx))
                        .map(|s| s.to_string())
                        .ok_or_else(|| anyhow!("long 形式には 時刻・系列・値 の3列が必要です"))
                };
                let scol = pick(&["series", "key"], 0)?;
                let vcol = pick(&["value", "avg"], 1)?;
                let keys: Vec<String> = {
                    let mut st = c.prepare(&format!("SELECT DISTINCT trim({}) FROM {tbl}_t WHERE {} IS NOT NULL", ident(&scol), ident(&scol)))?;
                    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
                    rows.collect::<std::result::Result<_, _>>()?
                };
                for k in &keys {
                    if lp::parse_series_key(k).is_none() {
                        bail!("系列キーの形式が不正です: '{k}'（例: plc,device=dev01 temp）");
                    }
                }
                let ids = self.resolve_series(&keys)?;
                let mut st = c.prepare(&format!("INSERT INTO {tbl}_map VALUES (?, ?)"))?;
                for (k, id) in keys.iter().zip(&ids) {
                    st.execute(params![k.as_str(), id])?;
                }
                (format!("SELECT __t, trim({}) AS name, {} AS v FROM {tbl}_t", ident(&scol), ident(&vcol)), "name")
            }
        };
        rep.series = count(c, &format!("SELECT count(*) FROM {tbl}_map"))? as usize;

        c.execute_batch(&format!(
            "CREATE TEMP TABLE {tbl} AS \
             SELECT m.sid AS series_id, u.__t AS ts_ms, last(u.x) AS value FROM ( \
               SELECT __t, {name_of}, TRY_CAST(replace(trim(v), ',', '') AS FLOAT) AS x FROM ({cells_sql}) \
             ) u JOIN {tbl}_map m ON m.name = u.{name_of} \
             WHERE u.__t IS NOT NULL AND u.x IS NOT NULL AND isfinite(u.x) GROUP BY ALL"
        ))?;
        rep.skipped_cells = count(
            c,
            &format!(
                "SELECT count(*) FROM ({cells_sql}) WHERE {name_of} IN (SELECT name FROM {tbl}_map) AND __t IS NOT NULL AND trim(coalesce(v, '')) <> '' \
                 AND (TRY_CAST(replace(trim(v), ',', '') AS FLOAT) IS NULL OR NOT isfinite(TRY_CAST(replace(trim(v), ',', '') AS FLOAT)))"
            ),
        )?;
        rep.points = count(c, &format!("SELECT count(*) FROM {tbl}"))?;
        if rep.points == 0 {
            return Ok(rep);
        }
        let (mn, mx): (i64, i64) = c.query_row(&format!("SELECT min(ts_ms), max(ts_ms) FROM {tbl}"), [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rep.from_ms = Some(mn);
        rep.to_ms = Some(mx);

        let wm = self.watermarks();
        let cut = floor_to(now, DAY) - self.opt.raw_days * DAY;
        let future = now + DAY;
        rep.rejected_old = count(c, &format!("SELECT count(*) FROM {tbl} WHERE ts_ms < {cut}"))?;
        rep.rejected_future = count(c, &format!("SELECT count(*) FROM {tbl} WHERE ts_ms > {future}"))?;

        // 1. 未確定の範囲は通常の書き込みと同じく raw_hot へ
        let hot = format!("ts_ms >= {} AND ts_ms <= {future}", wm.raw_flushed_until.max(cut));
        rep.recent_points = count(c, &format!("SELECT count(*) FROM {tbl} WHERE {hot}"))?;
        if rep.recent_points > 0 {
            let _w = self.writer.lock();
            let seq = self.seq.fetch_add(1, Ordering::SeqCst) + 1;
            tx(c, |c| {
                c.execute(
                    &format!(
                        "INSERT INTO raw_hot SELECT series_id, make_timestamp(ts_ms * 1000)::TIMESTAMP_MS, value, {seq} FROM {tbl} WHERE {hot}"
                    ),
                    [],
                )?;
                c.execute(
                    &format!(
                        "INSERT OR IGNORE INTO dirty_1m SELECT DISTINCT make_timestamp((ts_ms // 60000) * 60000000) FROM {tbl} \
                         WHERE {hot} AND ts_ms < {}",
                        wm.m1_until
                    ),
                    [],
                )?;
                Ok(())
            })?;
            let mut st = c.prepare(&format!("SELECT series_id, max(ts_ms), arg_max(value, ts_ms) FROM {tbl} WHERE {hot} GROUP BY 1"))?;
            let rows: Vec<(u32, i64, f32)> =
                st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<std::result::Result<_, _>>()?;
            let mut lat = self.latest.write();
            for (id, ts, v) in rows {
                let e = lat.entry(id).or_insert((i64::MIN, 0.0));
                if ts >= e.0 {
                    *e = (ts, v);
                }
            }
            self.written.fetch_add(rep.recent_points, Ordering::Relaxed);
        }

        // 2. 確定済みの日: 日次ファイルを作り直し、その日の1分集計を作り直す
        let days: Vec<i64> = {
            let mut st = c.prepare(&format!(
                "SELECT DISTINCT (ts_ms // {DAY}) * {DAY} FROM {tbl} WHERE ts_ms >= {cut} AND ts_ms < {} ORDER BY 1",
                wm.raw_flushed_until
            ))?;
            let rows = st.query_map([], |r| r.get::<_, i64>(0))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        if days.is_empty() {
            return Ok(rep);
        }
        let compact_before =
            if self.opt.compact_after_days > 0 { floor_to(now, DAY) - self.opt.compact_after_days * DAY } else { i64::MIN };
        c.execute_batch(&format!(
            "CREATE TEMP TABLE {tbl}_1m AS SELECT * FROM r1m_hot WHERE false; CREATE TEMP TABLE {tbl}_days (d TIMESTAMP);"
        ))?;
        for &d in &days {
            let plain = self.raw_path(d);
            let q = self.raw_q16_path(d);
            let existing = if q.exists() {
                format!("SELECT series_id, ts, (q_off + q * q_scale)::FLOAT AS value, 0 AS pri FROM read_parquet({}) UNION ALL ", sql_path(&q))
            } else if plain.exists() {
                format!("SELECT series_id, ts, value, 0 AS pri FROM read_parquet({}) UNION ALL ", sql_path(&plain))
            } else {
                String::new()
            };
            c.execute_batch(&format!(
                "CREATE OR REPLACE TEMP TABLE {tbl}_day AS \
                 SELECT series_id, ts, arg_max(value, pri) AS value FROM ( \
                   {existing} SELECT series_id, make_timestamp(ts_ms * 1000)::TIMESTAMP_MS AS ts, value, 1 AS pri FROM {tbl} \
                   WHERE ts_ms >= {d} AND ts_ms < {} \
                 ) GROUP BY series_id, ts",
                d + DAY
            ))?;
            {
                let _l = self.layout.write();
                self.write_raw_day(c, &format!("SELECT series_id, ts, value FROM {tbl}_day"), d, q.exists() || d < compact_before)?;
            }
            c.execute_batch(&format!(
                "INSERT INTO {tbl}_1m SELECT series_id, date_trunc('minute', ts)::TIMESTAMP AS b, {M1_AGG} FROM {tbl}_day GROUP BY series_id, b; \
                 INSERT INTO {tbl}_days VALUES ({});",
                tsus(d)
            ))?;
            rep.backfilled_points += count(c, &format!("SELECT count(*) FROM {tbl} WHERE ts_ms >= {d} AND ts_ms < {}", d + DAY))?;
            rep.backfilled_days += 1;
        }

        // 3. 1分集計: 当月以降は r1m_hot、確定した月は月次ファイルを作り直す
        let in_days = format!("date_trunc('day', bucket) IN (SELECT d FROM {tbl}_days)");
        let flushed_m = wm.m1_flushed_until;
        let months: BTreeSet<i64> = days.iter().map(|&d| month_start(d)).collect();
        tx(c, |c| {
            c.execute(&format!("DELETE FROM r1m_hot WHERE {in_days} AND bucket >= {}", tsus(flushed_m)), [])?;
            c.execute(&format!("INSERT INTO r1m_hot SELECT * FROM {tbl}_1m WHERE bucket >= {}", tsus(flushed_m)), [])?;
            Ok(())
        })?;
        for &m in months.iter().filter(|&&m| m < flushed_m) {
            let path = self.m1_path(m);
            let existing = if path.exists() {
                format!("SELECT * FROM read_parquet({}) WHERE NOT ({in_days}) UNION ALL ", sql_path(&path))
            } else {
                String::new()
            };
            let rows = format!(
                "SELECT * FROM ({existing} SELECT * FROM {tbl}_1m WHERE bucket >= {} AND bucket < {}) ORDER BY series_id, bucket",
                tsus(m),
                tsus(next_month(m))
            );
            c.execute_batch(&format!("CREATE OR REPLACE TEMP TABLE {tbl}_day AS {rows}"))?;
            let _l = self.layout.write();
            copy_to_parquet(c, &format!("SELECT * FROM {tbl}_day"), &path)?;
        }

        // 4. 1時間集計
        tx(c, |c| {
            c.execute(&format!("DELETE FROM r1h WHERE {in_days}"), [])?;
            c.execute(
                &format!("INSERT INTO r1h SELECT series_id, date_trunc('hour', bucket)::TIMESTAMP AS b, {UP_AGG} FROM {tbl}_1m GROUP BY series_id, b"),
                [],
            )?;
            Ok(())
        })?;
        Ok(rep)
    }
}
