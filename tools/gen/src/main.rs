//! 合成データ生成ツール
//!
//! ```text
//! # サービス停止中のデータフォルダへ、5年分を直接書き込む（HTTP を通さない）
//! parqlog-gen backfill --data-dir ./data --raw-days 14 --m1-days 365 --h1-days 1826
//!
//! # 稼働中のサービスへ、装置10台×10項目を1秒ごとに送る
//! parqlog-gen live --url http://127.0.0.1:8086
//!
//! # 取り込み性能の計測（直近1時間の時刻で大量に送る）
//! parqlog-gen bench --url http://127.0.0.1:8086 --points 2000000
//! ```
//!
//! 値は系列ごとに4種類のパターン（温度・圧力・運転状態・電流）を割り当てる。
//! 実データより規則的なので、容量の見積もりは実データで再確認すること。

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use parqlog_core::lp::series_key;
use parqlog_core::time::*;
use parqlog_core::*;

#[derive(Parser)]
#[command(name = "parqlog-gen", about = "parqlog の負荷試験用データ生成")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// データフォルダに直接、過去分を一括生成する（サービスは止めておく）
    Backfill {
        #[arg(long)]
        data_dir: PathBuf,
        #[arg(long, default_value_t = 10)]
        devices: u32,
        #[arg(long, default_value_t = 10)]
        fields: u32,
        /// 生データ（1秒）を作る日数（今日から遡る）
        #[arg(long, default_value_t = 14)]
        raw_days: i64,
        /// 1分集計を作る日数
        #[arg(long, default_value_t = 365)]
        m1_days: i64,
        /// 1時間集計を作る日数
        #[arg(long, default_value_t = 1826)]
        h1_days: i64,
        /// この日数より古い生データは 16bit 形式で書く
        #[arg(long, default_value_t = 365)]
        compact_after_days: i64,
        /// 既存のデータを消してから作る
        #[arg(long)]
        force: bool,
    },
    /// 稼働中のサービスへ一定間隔で送る
    Live {
        #[arg(long, default_value = "http://127.0.0.1:8086")]
        url: String,
        #[arg(long)]
        token: Option<String>,
        #[arg(long, default_value_t = 10)]
        devices: u32,
        #[arg(long, default_value_t = 10)]
        fields: u32,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
    /// 取り込み性能を計測する
    Bench {
        #[arg(long, default_value = "http://127.0.0.1:8086")]
        url: String,
        #[arg(long)]
        token: Option<String>,
        #[arg(long, default_value_t = 1_000_000)]
        points: u64,
        #[arg(long, default_value_t = 5000)]
        batch: usize,
        #[arg(long, default_value_t = 4)]
        threads: usize,
        #[arg(long, default_value_t = 1000)]
        series: u64,
        #[arg(long)]
        gzip: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Backfill { data_dir, devices, fields, raw_days, m1_days, h1_days, compact_after_days, force } => {
            backfill(data_dir, devices, fields, raw_days, m1_days.max(raw_days), h1_days.max(m1_days), compact_after_days, force)
        }
        Cmd::Live { url, token, devices, fields, interval_ms } => live(&url, token, devices, fields, interval_ms),
        Cmd::Bench { url, token, points, batch, threads, series, gzip } => {
            bench(&url, token, points, batch, threads, series, gzip)
        }
    }
}

/// 系列の値（t は UNIX 秒）。kind = id % 4
const VALUE: &str = "CASE s.kind \
    WHEN 0 THEN round(20 + 5*sin(2*pi()*{T}/86400 + s.id) + 2*sin(2*pi()*{T}/604800) + 0.1*({R} - 0.5), 1) \
    WHEN 1 THEN round(101.3 + 0.5*sin(2*pi()*{T}/3600 + s.id) + 0.02*({R} - 0.5), 2) \
    WHEN 2 THEN CASE WHEN sin(2*pi()*{T}/1800 + s.id) > 0.2 THEN 1 ELSE 0 END \
    ELSE 10 + 3*sin(2*pi()*{T}/600 + s.id) + 0.4*({R} - 0.5) END";

fn value_at(t: &str) -> String {
    VALUE.replace("{T}", &format!("({t})")).replace("{R}", &format!("((hash(s.id, {t}) % 1000003) / 1000003.0)"))
}

/// 集計値を直接作るときの min/max の幅
fn spread(per_hour: bool) -> &'static str {
    if per_hour {
        "CASE s.kind WHEN 0 THEN 0.3 WHEN 1 THEN 0.5 WHEN 2 THEN 0.5 ELSE 3.0 END"
    } else {
        "CASE s.kind WHEN 0 THEN 0.1 WHEN 1 THEN 0.02 WHEN 2 THEN 0.0 ELSE 1.2 END"
    }
}

#[allow(clippy::too_many_arguments)]
fn backfill(
    dir: PathBuf,
    devices: u32,
    fields: u32,
    raw_days: i64,
    m1_days: i64,
    h1_days: i64,
    compact_after: i64,
    force: bool,
) -> Result<()> {
    if dir.join("parqlog.duckdb").exists() {
        if !force {
            bail!("{} には既にデータがあります（消してよければ --force）", dir.display());
        }
        for sub in ["raw", "1m"] {
            let _ = std::fs::remove_dir_all(dir.join(sub));
        }
        for f in ["parqlog.duckdb", "parqlog.duckdb.wal"] {
            let _ = std::fs::remove_file(dir.join(f));
        }
    }
    let now = now_ms();
    let end = floor_to(now, DAY);
    let start = end - h1_days * DAY;
    let raw_from = end - raw_days * DAY;
    let m1_from = end - m1_days * DAY;
    let compact_before = if compact_after > 0 { end - compact_after * DAY } else { i64::MIN };

    let mut opt = StoreOptions::for_dir(&dir);
    opt.compact_after_days = compact_after;
    let store = DuckStore::open_at(opt, now)?;

    let mut keys = vec![];
    for d in 1..=devices {
        for f in 1..=fields {
            keys.push(series_key("plc", &[("device".into(), format!("dev{d:02}"))], &format!("f{f:02}")));
        }
    }
    let n_series = store.resolve_series(&keys)?.len();
    println!(
        "系列 {n_series}、期間 {} 〜 {}（生データ {raw_days} 日、1分 {m1_days} 日、1時間 {h1_days} 日）",
        day_str(start),
        day_str(end - DAY)
    );

    let w = store.writer();
    w.execute_batch(
        "CREATE OR REPLACE TEMP TABLE gen_series (id UINTEGER, kind UTINYINT);
         CREATE OR REPLACE TEMP TABLE gen_1m AS SELECT * FROM r1m_hot WHERE false;
         DELETE FROM raw_hot; DELETE FROM r1m_hot; DELETE FROM r1h;",
    )?;
    w.execute_batch("INSERT INTO gen_series SELECT id, (id % 4)::UTINYINT FROM series")?;

    let t_all = Instant::now();
    let mut staging_month: Option<i64> = None;
    let mut raw_plain = (0u64, 0u64); // (日数, バイト)
    let mut raw_q16 = (0u64, 0u64);

    let finish_month = |m: i64| -> Result<()> {
        w.execute_batch(
            "INSERT INTO r1h SELECT series_id, date_trunc('hour', bucket)::TIMESTAMP AS b, \
             min(vmin), max(vmax), sum(vsum), sum(vcount)::UINTEGER, arg_min(vfirst, bucket), arg_max(vlast, bucket) \
             FROM gen_1m GROUP BY series_id, b",
        )?;
        if m < month_start(end) {
            parqlog_core::duck::copy_to_parquet(&w, "SELECT * FROM gen_1m ORDER BY series_id, bucket", &store.m1_path(m))?;
        } else {
            w.execute_batch("INSERT INTO r1m_hot SELECT * FROM gen_1m")?;
        }
        w.execute_batch("DELETE FROM gen_1m")?;
        Ok(())
    };

    let mut d = start;
    let mut last_print = Instant::now();
    while d < end {
        let ds = d / 1000;
        if d >= m1_from {
            let m = month_start(d);
            if let Some(cur) = staging_month {
                if cur != m {
                    finish_month(cur)?;
                }
            }
            staging_month = Some(m);
        }

        if d >= raw_from {
            // 生データ 1秒 × 86,400
            let t = format!("({ds} + r.i)");
            w.execute_batch(&format!(
                "CREATE OR REPLACE TEMP TABLE day_raw AS \
                 SELECT s.id AS series_id, make_timestamp((({ds} + r.i) * 1000000)::BIGINT)::TIMESTAMP_MS AS ts, \
                        ({})::FLOAT AS value \
                 FROM gen_series s, range(0, 86400) r(i)",
                value_at(&t)
            ))?;
            let compact = d < compact_before;
            store.write_raw_day(&w, "SELECT series_id, ts, value FROM day_raw", d, compact)?;
            let path = if compact { store.raw_q16_path(d) } else { store.raw_path(d) };
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if compact {
                raw_q16.0 += 1;
                raw_q16.1 += len;
            } else {
                raw_plain.0 += 1;
                raw_plain.1 += len;
            }
            w.execute_batch(
                "INSERT INTO gen_1m SELECT series_id, date_trunc('minute', ts)::TIMESTAMP AS b, \
                 min(value), max(value), sum(value::DOUBLE), count(*)::UINTEGER, arg_min(value, ts), arg_max(value, ts) \
                 FROM day_raw GROUP BY series_id, b; DROP TABLE day_raw;",
            )?;
        } else if d >= m1_from {
            let t = format!("({ds} + r.i * 60 + 30)");
            w.execute_batch(&format!(
                "INSERT INTO gen_1m SELECT id, b, (v - sp)::FLOAT, (v + sp)::FLOAT, v * 60, 60, v::FLOAT, v::FLOAT FROM ( \
                   SELECT s.id, make_timestamp((({ds} + r.i * 60) * 1000000)::BIGINT) AS b, ({})::DOUBLE AS v, {} AS sp \
                   FROM gen_series s, range(0, 1440) r(i))",
                value_at(&t),
                spread(false)
            ))?;
        } else {
            let t = format!("({ds} + r.i * 3600 + 1800)");
            w.execute_batch(&format!(
                "INSERT INTO r1h SELECT id, b, (v - sp)::FLOAT, (v + sp)::FLOAT, v * 3600, 3600, v::FLOAT, v::FLOAT FROM ( \
                   SELECT s.id, make_timestamp((({ds} + r.i * 3600) * 1000000)::BIGINT) AS b, ({})::DOUBLE AS v, {} AS sp \
                   FROM gen_series s, range(0, 24) r(i))",
                value_at(&t),
                spread(true)
            ))?;
        }

        d += DAY;
        if last_print.elapsed() > Duration::from_secs(5) || d == end {
            last_print = Instant::now();
            print!("\r{} まで完了（{:.0} 秒経過）   ", day_str(d - DAY), t_all.elapsed().as_secs_f64());
            std::io::stdout().flush().ok();
        }
    }
    if let Some(m) = staging_month {
        finish_month(m)?;
    }
    println!();
    w.execute_batch("DROP TABLE gen_series; DROP TABLE gen_1m; CHECKPOINT;")?;
    drop(w);

    store.set_watermarks(&Watermarks {
        raw_accept_from: end,
        raw_flushed_until: end,
        raw_compacted_until: if compact_after > 0 { compact_before.max(start) } else { start },
        m1_until: end,
        h1_until: end,
        m1_flushed_until: month_start(end),
    })?;
    store.checkpoint()?;

    let gb = |b: u64| b as f64 / 1024f64.powi(3);
    let mb = |b: u64| b as f64 / 1024f64.powi(2);
    println!("完了: {:.0} 秒", t_all.elapsed().as_secs_f64());
    let pts_day = n_series as f64 * 86400.0;
    if raw_plain.0 > 0 {
        let per = raw_plain.1 as f64 / raw_plain.0 as f64;
        println!(
            "生データ（32bit）: {} 日、平均 {:.1} MB/日（{:.2} バイト/点）→ 1年 {:.1} GB",
            raw_plain.0, mb(per as u64), per / pts_day, gb((per * 365.0) as u64)
        );
    }
    if raw_q16.0 > 0 {
        let per = raw_q16.1 as f64 / raw_q16.0 as f64;
        println!(
            "生データ（16bit）: {} 日、平均 {:.1} MB/日（{:.2} バイト/点）→ 4年 {:.1} GB",
            raw_q16.0, mb(per as u64), per / pts_day, gb((per * 365.0 * 4.0) as u64)
        );
    }
    let m1: u64 = parqlog_core::duck::list_m1_files(&dir).iter().map(|(_, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    println!("1分集計ファイル: {:.1} MB", mb(m1));
    println!("DuckDB ファイル: {:.1} MB", mb(std::fs::metadata(dir.join("parqlog.duckdb")).map(|m| m.len()).unwrap_or(0)));
    Ok(())
}

// ---- HTTP 送信 --------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn post(agent: &ureq::Agent, url: &str, token: &Option<String>, body: Vec<u8>, gzip: bool) -> Result<()> {
    let mut req = agent.post(url).header("Content-Type", "text/plain; charset=utf-8");
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Token {t}"));
    }
    let body = if gzip {
        req = req.header("Content-Encoding", "gzip");
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(&body)?;
        e.finish()?
    } else {
        body
    };
    let mut res = req.send(&body[..])?;
    if !res.status().is_success() {
        bail!("{}: {}", res.status(), res.body_mut().read_to_string().unwrap_or_default());
    }
    Ok(())
}

fn live(url: &str, token: Option<String>, devices: u32, fields: u32, interval_ms: u64) -> Result<()> {
    let url = format!("{}/api/v2/write?precision=ms", url.trim_end_matches('/'));
    let a = agent();
    println!("{url} へ {devices} 台 × {fields} 項目を {interval_ms}ms ごとに送信（Ctrl+C で終了）");
    let mut n = 0u64;
    loop {
        let t0 = Instant::now();
        let ts = now_ms();
        let s = ts as f64 / 1000.0;
        let mut body = String::new();
        for d in 1..=devices {
            body.push_str(&format!("plc,device=dev{d:02} "));
            for f in 1..=fields {
                // backfill と同じく系列 ID（登録順）で値の種類を決める
                let idn = (d - 1) * fields + f;
                let id = idn as f64;
                let v = match idn % 4 {
                    0 => 20.0 + 5.0 * (2.0 * std::f64::consts::PI * s / 86400.0 + id).sin(),
                    1 => 101.3 + 0.5 * (2.0 * std::f64::consts::PI * s / 3600.0 + id).sin(),
                    2 => ((2.0 * std::f64::consts::PI * s / 1800.0 + id).sin() > 0.2) as i32 as f64,
                    _ => 10.0 + 3.0 * (2.0 * std::f64::consts::PI * s / 600.0 + id).sin() + (s * 7.3 + id).sin() * 0.2,
                };
                if f > 1 {
                    body.push(',');
                }
                body.push_str(&format!("f{f:02}={v:.3}"));
            }
            body.push_str(&format!(" {ts}\n"));
        }
        match post(&a, &url, &token, body.into_bytes(), false) {
            Ok(()) => n += 1,
            Err(e) => eprintln!("送信エラー: {e:#}"),
        }
        if n % 60 == 0 {
            println!("{n} 回送信");
        }
        let el = t0.elapsed();
        if el < Duration::from_millis(interval_ms) {
            std::thread::sleep(Duration::from_millis(interval_ms) - el);
        }
    }
}

fn bench(url: &str, token: Option<String>, points: u64, batch: usize, threads: usize, series: u64, gzip: bool) -> Result<()> {
    let url = format!("{}/api/v2/write?precision=ms", url.trim_end_matches('/'));
    let sent = Arc::new(AtomicU64::new(0));
    let next = Arc::new(AtomicU64::new(0));
    let base = now_ms() - HOUR;
    let t0 = Instant::now();
    let mut hs = vec![];
    for _ in 0..threads {
        let (sent, next, url, token) = (sent.clone(), next.clone(), url.clone(), token.clone());
        hs.push(std::thread::spawn(move || -> Result<()> {
            let a = agent();
            loop {
                let start = next.fetch_add(batch as u64, Ordering::SeqCst);
                if start >= points {
                    return Ok(());
                }
                let endp = (start + batch as u64).min(points);
                let mut body = String::with_capacity(batch * 48);
                for i in start..endp {
                    let s = i % series;
                    let ts = base + (i / series) as i64 % HOUR;
                    body.push_str(&format!("bench,device=b{:03} v{}={} {ts}\n", s / 10, s % 10, (i % 1000) as f64 * 0.1));
                }
                post(&a, &url, &token, body.into_bytes(), gzip)?;
                sent.fetch_add(endp - start, Ordering::Relaxed);
            }
        }));
    }
    for h in hs {
        h.join().unwrap()?;
    }
    let el = t0.elapsed().as_secs_f64();
    println!("{} 点を {:.2} 秒で送信: {:.0} 点/秒", sent.load(Ordering::Relaxed), el, sent.load(Ordering::Relaxed) as f64 / el);
    Ok(())
}
