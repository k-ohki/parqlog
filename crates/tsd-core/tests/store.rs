//! ストレージの結合テスト。時刻は引数で与えて、数日〜数か月の経過を再現する。

use tsd_core::lp::series_key;
use tsd_core::time::*;
use tsd_core::*;

fn key(dev: &str, field: &str) -> String {
    series_key("plc", &[("device".to_string(), dev.to_string())], field)
}

fn opts(dir: &std::path::Path) -> StoreOptions {
    let mut o = StoreOptions::for_dir(dir);
    o.memory_limit = "512MB".into();
    o.threads = 2;
    o
}

/// 2026-03-30 00:00 UTC
fn t0() -> i64 {
    parse_day("2026-03-30").unwrap()
}

/// 1Hz の点を [from, to) で生成。値は秒数 mod 60。
fn gen(id: u32, from: i64, to: i64) -> Vec<Point> {
    (0..)
        .map(|i| from + i * SEC)
        .take_while(|t| *t < to)
        .map(|t| Point { series_id: id, ts_ms: t, value: ((t / SEC) % 60) as f32 })
        .collect()
}

fn write(s: &DuckStore, pts: &[Point], now: i64) -> WriteOutcome {
    s.write_points_at(pts, now).unwrap()
}

fn q(s: &DuckStore, ids: &[u32], from: i64, to: i64, width: u32, tier: Option<Tier>) -> AggResult {
    s.query_agg(&AggQuery { series: ids.to_vec(), from_ms: from, to_ms: to, width, tier }).unwrap()
}

/// メンテナンスを実行し、どのジョブも失敗していないことを確かめる
fn maint(s: &DuckStore, now: i64) {
    s.run_maintenance(now).unwrap();
    for j in s.status().unwrap().jobs {
        assert!(j.last_error.is_none(), "job {} failed: {:?}", j.name, j.last_error);
    }
}

fn total_count(r: &AggResult) -> usize {
    r.series.iter().map(|s| s.avg.iter().filter(|v| v.is_some()).count()).sum()
}

#[test]
fn ingest_rollup_and_query() {
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + 2 * HOUR;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    let ids = s.resolve_series(&[key("a", "temp"), key("b", "temp")]).unwrap();
    assert_eq!(ids, vec![1, 2]);
    // 同じキーは同じ ID
    assert_eq!(s.resolve_series(&[key("b", "temp")]).unwrap(), vec![2]);

    let from = t0();
    let to = t0() + HOUR;
    let o = write(&s, &gen(1, from, to), now);
    assert_eq!(o.written, 3600);
    write(&s, &gen(2, from, to), now);
    maint(&s, now);

    let wm = s.watermarks();
    assert_eq!(wm.m1_until, floor_to(now - 10 * SEC, MIN));
    assert_eq!(wm.h1_until, floor_to(now - MIN, HOUR));

    // 1時間を 1600px → 生データ層、3秒バケット
    let r = q(&s, &[1, 2], from, to, 1600, None);
    assert_eq!(r.tier, Tier::Raw);
    assert_eq!(r.bucket_ms, 3000);
    assert_eq!(r.series[0].t.len(), 1200);
    // 1分層を強制: 値 0..59 の平均は 29.5、min 0、max 59
    let r = q(&s, &[1], from, to, 60, Some(Tier::M1));
    assert_eq!(r.tier, Tier::M1);
    assert_eq!(r.bucket_ms, MIN);
    assert_eq!(r.series[0].t.len(), 60);
    assert_eq!(r.series[0].avg[0], Some(29.5));
    assert_eq!(r.series[0].min[0], Some(0.0));
    assert_eq!(r.series[0].max[0], Some(59.0));
    // 1時間層
    let r = q(&s, &[1], from, to, 10, Some(Tier::H1));
    assert_eq!(r.series[0].t, vec![from]);
    assert_eq!(r.series[0].avg[0], Some(29.5));

    let lat = s.latest(&[1]).unwrap();
    assert_eq!(lat[0].ts, to - SEC);
    let st = s.status().unwrap();
    assert_eq!(st.raw_hot_rows, 7200);
    assert_eq!(st.series_count, 2);
}

#[test]
fn recent_data_is_stitched_from_raw() {
    // 1分集計がまだ無い直近分も 1分層・1時間層のクエリに出る
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + 3 * HOUR + 30 * MIN;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.resolve_series(&[key("a", "v")]).unwrap();
    write(&s, &gen(1, t0(), now), now);
    maint(&s, t0() + 2 * HOUR); // 集計は 2時間目まで
    let r = q(&s, &[1], t0(), now, 100, Some(Tier::H1));
    assert_eq!(r.series[0].t.len(), 4);
    assert!(r.series[0].avg.iter().all(|v| v.is_some()));
    let r = q(&s, &[1], t0(), now, 1000, Some(Tier::M1));
    assert_eq!(total_count(&r), 210);
}

#[test]
fn duplicates_last_write_wins() {
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + HOUR;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.resolve_series(&[key("a", "v")]).unwrap();
    let ts = t0() + 10 * MIN;
    write(&s, &[Point { series_id: 1, ts_ms: ts, value: 1.0 }, Point { series_id: 1, ts_ms: ts, value: 2.0 }], now);
    write(&s, &[Point { series_id: 1, ts_ms: ts, value: 3.0 }], now);
    let r = q(&s, &[1], ts, ts + SEC, 10, Some(Tier::Raw));
    assert_eq!(r.series[0].avg, vec![Some(3.0)]);
    maint(&s, now);
    let r = q(&s, &[1], ts, ts + MIN, 10, Some(Tier::M1));
    assert_eq!(r.series[0].avg, vec![Some(3.0)]);
}

#[test]
fn late_data_is_reaggregated_and_too_old_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + 5 * HOUR;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.resolve_series(&[key("a", "v")]).unwrap();
    write(&s, &gen(1, t0(), t0() + HOUR), now);
    maint(&s, now);
    let r = q(&s, &[1], t0(), t0() + MIN, 10, Some(Tier::M1));
    assert_eq!(r.series[0].max[0], Some(59.0));

    // 集計済みの分に遅れて大きな値が届く
    write(&s, &[Point { series_id: 1, ts_ms: t0() + 500, value: 1000.0 }], now);
    maint(&s, now + MIN);
    let r = q(&s, &[1], t0(), t0() + MIN, 10, Some(Tier::M1));
    assert_eq!(r.series[0].max[0], Some(1000.0));
    let r = q(&s, &[1], t0(), t0() + HOUR, 10, Some(Tier::H1));
    assert_eq!(r.series[0].max[0], Some(1000.0));

    // 猶予期間（1日）より前は拒否
    let o = write(&s, &[Point { series_id: 1, ts_ms: t0() - 2 * DAY, value: 1.0 }], now);
    assert_eq!(o.rejected_old, 1);
    // 1日以上先の未来も拒否
    let o = write(&s, &[Point { series_id: 1, ts_ms: now + 2 * DAY, value: 1.0 }], now);
    assert_eq!(o.rejected_future, 1);
}

#[test]
fn daily_and_monthly_flush_keep_results_identical() {
    let dir = tempfile::tempdir().unwrap();
    let start = t0(); // 3/30
    let s = DuckStore::open_at(opts(dir.path()), start + HOUR).unwrap();
    s.resolve_series(&[key("a", "v"), key("b", "v")]).unwrap();

    // 3/30, 3/31, 4/1 の各日に 2時間ずつ書き込み、その都度メンテナンス
    let days = [start, start + DAY, start + 2 * DAY];
    for d in days {
        let now = d + 3 * HOUR;
        write(&s, &gen(1, d, d + 2 * HOUR), now);
        write(&s, &gen(2, d, d + 2 * HOUR), now);
        maint(&s, now);
    }
    let before_raw = q(&s, &[1, 2], start, start + 2 * HOUR, 1600, None);
    let before_m1 = q(&s, &[1, 2], start, start + 3 * DAY, 2000, Some(Tier::M1));
    let before_h1 = q(&s, &[1], start, start + 3 * DAY, 100, Some(Tier::H1));

    // 4/3 になると 3/30〜4/1 が確定し、3月分の1分集計も月次ファイルになる
    let now = start + 4 * DAY + 2 * HOUR;
    maint(&s, now);
    let wm = s.watermarks();
    assert_eq!(wm.raw_flushed_until, start + 4 * DAY - DAY); // 4/2 まで確定（4/2 は猶予中）
    assert_eq!(wm.m1_flushed_until, parse_day("2026-04-01").unwrap());
    assert!(s.raw_path(start).exists());
    assert!(s.raw_path(start + DAY).exists());
    assert!(s.m1_path(start).exists());
    assert_eq!(s.status().unwrap().raw_hot_rows, 0);

    let after_raw = q(&s, &[1, 2], start, start + 2 * HOUR, 1600, None);
    let after_m1 = q(&s, &[1, 2], start, start + 3 * DAY, 2000, Some(Tier::M1));
    let after_h1 = q(&s, &[1], start, start + 3 * DAY, 100, Some(Tier::H1));
    assert_eq!(before_raw.series, after_raw.series);
    assert_eq!(before_m1.series, after_m1.series);
    assert_eq!(before_h1.series, after_h1.series);

    // 再オープンしても同じ
    drop(s);
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    let again = q(&s, &[1, 2], start, start + 3 * DAY, 2000, Some(Tier::M1));
    assert_eq!(after_m1.series, again.series);
    // 何度実行しても結果は同じ
    maint(&s, now);
    maint(&s, now);
    let again = q(&s, &[1, 2], start, start + 3 * DAY, 2000, Some(Tier::M1));
    assert_eq!(after_m1.series, again.series);
}

#[test]
fn gaps_become_nulls() {
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + 3 * HOUR;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.resolve_series(&[key("a", "v")]).unwrap();
    write(&s, &gen(1, t0(), t0() + 30 * MIN), now);
    write(&s, &gen(1, t0() + 40 * MIN, t0() + HOUR), now);
    maint(&s, now);
    let r = q(&s, &[1], t0(), t0() + HOUR, 60, Some(Tier::M1));
    let s0 = &r.series[0];
    assert_eq!(s0.avg.iter().filter(|v| v.is_none()).count(), 1);
    let i = s0.avg.iter().position(|v| v.is_none()).unwrap();
    assert_eq!(s0.t[i], t0() + 30 * MIN);
    // 秒単位のゆらぎ（1つ抜け）は欠測にしない
    let mut pts = gen(1, t0() + 2 * HOUR, t0() + 2 * HOUR + MIN);
    pts.remove(30);
    write(&s, &pts, now);
    let r = q(&s, &[1], t0() + 2 * HOUR, t0() + 2 * HOUR + MIN, 1000, Some(Tier::Raw));
    assert!(r.series[0].avg.iter().all(|v| v.is_some()));
    assert!(!r.band);
}

#[test]
fn compaction_to_16bit() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.compact_after_days = 3;
    let s = DuckStore::open_at(o, t0() + HOUR).unwrap();
    s.resolve_series(&[key("a", "v"), key("b", "flat")]).unwrap();
    let d = t0();
    let pts: Vec<Point> = gen(1, d, d + HOUR)
        .into_iter()
        .map(|p| Point { value: 20.0 + (p.ts_ms as f32 / 1000.0).sin() * 5.0 + p.value * 0.001, ..p })
        .collect();
    write(&s, &pts, d + 2 * HOUR);
    write(&s, &gen(2, d, d + HOUR).into_iter().map(|p| Point { value: 7.5, ..p }).collect::<Vec<_>>(), d + 2 * HOUR);
    maint(&s, d + 2 * HOUR);
    let exact = q(&s, &[1, 2], d, d + HOUR, 3600, Some(Tier::Raw));

    maint(&s, d + 2 * DAY + HOUR);
    assert!(s.raw_path(d).exists());
    maint(&s, d + 5 * DAY);
    assert!(!s.raw_path(d).exists());
    assert!(s.raw_q16_path(d).exists());

    let comp = q(&s, &[1, 2], d, d + HOUR, 3600, Some(Tier::Raw));
    let a = &exact.series[0];
    let b = &comp.series[0];
    assert_eq!(a.t, b.t);
    // 誤差はレンジ（約10）÷ 131070 程度
    let max_err = a.avg.iter().zip(&b.avg).map(|(x, y)| (x.unwrap() - y.unwrap()).abs()).fold(0f32, f32::max);
    assert!(max_err < 1e-4, "max_err = {max_err}");
    assert!(comp.series[1].avg.iter().all(|v| *v == Some(7.5)));
}

#[test]
fn retention_deletes_old_files() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.raw_days = 3;
    o.compact_after_days = 0;
    let s = DuckStore::open_at(o, t0() + HOUR).unwrap();
    s.resolve_series(&[key("a", "v")]).unwrap();
    write(&s, &gen(1, t0(), t0() + MIN), t0() + HOUR);
    maint(&s, t0() + 2 * DAY + HOUR);
    assert!(s.raw_path(t0()).exists());
    maint(&s, t0() + 4 * DAY + HOUR);
    assert!(!s.raw_path(t0()).exists());
}

#[test]
fn export_and_backup() {
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + 2 * HOUR;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.resolve_series(&[key("a", "v")]).unwrap();
    write(&s, &gen(1, t0(), t0() + 10 * MIN), now);
    maint(&s, now);

    let out = dir.path().join("out/export.csv");
    let n = s
        .export(
            &ExportRequest { series: vec![1], from_ms: t0(), to_ms: t0() + 10 * MIN, tier: Tier::Raw, format: ExportFormat::Csv },
            &out,
        )
        .unwrap();
    assert_eq!(n, 600);
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with("time_utc,series,value"));
    let n = s
        .export(
            &ExportRequest { series: vec![1], from_ms: t0(), to_ms: t0() + 10 * MIN, tier: Tier::M1, format: ExportFormat::Parquet },
            &dir.path().join("out/export.parquet"),
        )
        .unwrap();
    assert_eq!(n, 10);

    let bk = tempfile::tempdir().unwrap();
    let rep = s.backup(bk.path()).unwrap();
    assert!(rep.db_bytes > 0);
    drop(s);
    let s2 = DuckStore::open_at(opts(bk.path()), now).unwrap();
    assert_eq!(s2.list_series(&SeriesFilter::default()).unwrap().len(), 1);
    assert_eq!(s2.status().unwrap().raw_hot_rows, 600);
}

#[test]
fn tier_choice_matches_spec_table() {
    // 仕様 5章の表（幅 1,600px）
    let w = 1600;
    assert_eq!(DuckStore::choose(HOUR, w, None).0, Tier::Raw);
    assert_eq!(DuckStore::choose(DAY, w, None).0, Tier::Raw);
    assert_eq!(DuckStore::choose(7 * DAY, w, None).0, Tier::M1);
    assert_eq!(DuckStore::choose(30 * DAY, w, None).0, Tier::M1);
    assert_eq!(DuckStore::choose(365 * DAY, w, None).0, Tier::H1);
    assert_eq!(DuckStore::choose(5 * 365 * DAY, w, None).0, Tier::H1);
    assert_eq!(DuckStore::choose(DAY, w, None).1, 54_000);
    assert_eq!(DuckStore::choose(365 * DAY, w, None).1, 6 * HOUR);
}

#[test]
fn ingestor_parses_and_flushes() {
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let s: Arc<DuckStore> = Arc::new(DuckStore::open(opts(dir.path())).unwrap());
    let ing = Ingestor::start(s.clone());
    let now_s = now_ms() / 1000;
    let body = format!(
        "plc,device=a temp=21.5,run=t,name=\"x\" {now_s}\nplc,device=b temp=22i {now_s}\nbroken line\n"
    );
    let r = ing.ingest_lp(&body, lp::Precision::S).unwrap();
    assert_eq!(r.points, 3);
    assert_eq!(r.skipped_fields, 1);
    assert_eq!(r.errors.len(), 1);
    assert_eq!(r.errors[0].line, 3);
    ing.shutdown();
    assert_eq!(s.status().unwrap().raw_hot_rows, 3);
    let list = s.list_series(&SeriesFilter { tags: vec![("device".into(), "a".into())], ..Default::default() }).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].field, "temp");
}

#[test]
fn concurrent_write_query_maintenance_no_deadlock() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(DuckStore::open(opts(dir.path())).unwrap());
    s.resolve_series(&[key("a", "v"), key("b", "v")]).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = vec![];
    {
        let (s, stop) = (s.clone(), stop.clone());
        hs.push(std::thread::spawn(move || {
            let mut t = now_ms() - 30 * MIN;
            while !stop.load(Ordering::Relaxed) {
                let pts: Vec<Point> = (0..500).flat_map(|i| [1u32, 2].map(|id| Point { series_id: id, ts_ms: t + i * 100, value: i as f32 })).collect();
                s.write_batch(&pts).unwrap();
                t += 50_000;
            }
        }));
    }
    for _ in 0..3 {
        let (s, stop) = (s.clone(), stop.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let now = now_ms();
                s.query_agg(&AggQuery { series: vec![1, 2], from_ms: now - HOUR, to_ms: now, width: 800, tier: None }).unwrap();
                s.status().unwrap();
            }
        }));
    }
    {
        let (s, stop) = (s.clone(), stop.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                s.run_maintenance(now_ms()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_secs(4));
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }
    for j in s.status().unwrap().jobs {
        assert!(j.last_error.is_none(), "{:?}", j);
    }
}

#[test]
fn rename_and_delete_series() {
    let dir = tempfile::tempdir().unwrap();
    let now = t0() + 2 * HOUR;
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.resolve_series(&[key("dev1", "temp"), key("dev02", "temp"), key("dev03", "temp")]).unwrap();
    write(&s, &gen(1, t0(), t0() + 10 * MIN), now);
    write(&s, &gen(2, t0(), t0() + 10 * MIN), now);
    // 古いデータを Parquet に確定させる
    maint(&s, t0() + 3 * DAY);

    // 名前変更: ID とデータはそのまま、タグは並べ替えて正規化
    let r = s.rename_series(1, "plc,zone=A,device=dev01 temp").unwrap();
    assert_eq!(r.key, "plc,device=dev01,zone=A temp");
    assert_eq!(r.tags.get("zone").map(String::as_str), Some("A"));
    assert_eq!(s.resolve_series(&[r.key.clone()]).unwrap(), vec![1]);
    let res = q(&s, &[1], t0(), t0() + 10 * MIN, 600, Some(Tier::Raw));
    assert_eq!(res.series[0].t.len(), 600);
    // 既にあるキーへの変更は不可、不正なキーも不可
    assert!(s.rename_series(1, &key("dev02", "temp")).is_err());
    assert!(s.rename_series(1, "no-field").is_err());
    assert!(s.rename_series(99, "plc v").is_err());

    // 削除: 一覧から消え、確定済みのデータも読めない
    assert_eq!(s.delete_series(&[2, 99]).unwrap(), 1);
    assert!(s.list_series(&SeriesFilter::default()).unwrap().iter().all(|x| x.id != 2));
    let res = q(&s, &[2], t0(), t0() + 10 * MIN, 600, Some(Tier::Raw));
    assert!(res.series[0].t.is_empty());
    assert!(s.latest(&[2]).unwrap().is_empty());
    // 削除前にバッファに入っていた点は捨てる
    let o = s.write_points_at(&[Point { series_id: 2, ts_ms: now, value: 1.0 }], now).unwrap();
    assert_eq!(o.written, 0);

    // 再起動しても ID は使い回さない。同じキーで送り直すと新しい系列になる
    drop(s);
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    s.delete_series(&[3]).unwrap();
    drop(s);
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    let ids = s.resolve_series(&[key("dev02", "temp")]).unwrap();
    assert_eq!(ids, vec![4]);
    let res = q(&s, &[4], t0(), t0() + 10 * MIN, 600, Some(Tier::Raw));
    assert!(res.series[0].t.is_empty());
    assert_eq!(s.list_series(&SeriesFilter::default()).unwrap()[0].key, "plc,device=dev01,zone=A temp");
}
