//! CSV 取り込みのテスト

use parqlog_core::lp::series_key;
use parqlog_core::time::*;
use parqlog_core::*;

fn opts(dir: &std::path::Path) -> StoreOptions {
    let mut o = StoreOptions::for_dir(dir);
    o.memory_limit = "512MB".into();
    o.threads = 2;
    o
}

fn q(s: &DuckStore, ids: &[u32], from: i64, to: i64, width: u32, tier: Option<Tier>) -> AggResult {
    s.query_agg(&AggQuery { series: ids.to_vec(), from_ms: from, to_ms: to, width, tier }).unwrap()
}

fn wide_opts(offset_min: i32) -> CsvImportOptions {
    CsvImportOptions {
        layout: CsvLayout::Wide,
        measurement: "line".into(),
        tags: vec![("device".into(), "A".into())],
        time_column: None,
        utc_offset_minutes: offset_min,
    }
}

fn write_csv(dir: &std::path::Path, name: &str, text: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, text).unwrap();
    p
}

/// 日本時間の "YYYY/MM/DD HH:MM:SS"
fn jst(ms: i64) -> String {
    to_dt(ms + 9 * HOUR).format("%Y/%m/%d %H:%M:%S").to_string()
}

#[test]
fn wide_recent_with_local_time_and_bad_cells() {
    let dir = tempfile::tempdir().unwrap();
    let s = DuckStore::open(opts(dir.path())).unwrap();
    let t = floor_to(now_ms(), MIN) - 30 * MIN;
    let mut csv = String::from("日時,温度,圧力,メモ\n");
    for i in 0..60 {
        csv.push_str(&format!("{},{},{},{}\n", jst(t + i * SEC), 20.0 + i as f64 * 0.1, if i == 5 { "ERR" } else { "101.3" }, ""));
    }
    csv.push_str("壊れた時刻,1,2,\n");
    let p = write_csv(dir.path(), "a.csv", &csv);
    let r = s.import_csv(&p, &wide_opts(540)).unwrap();
    assert_eq!(r.rows, 61);
    assert_eq!(r.skipped_rows, 1);
    assert_eq!(r.skipped_cells, 1);
    assert_eq!(r.series, 2); // 空のメモ列は系列にしない
    assert_eq!(r.points, 60 + 59);
    assert_eq!(r.recent_points, 119);
    assert_eq!(r.from_ms, Some(t));

    let ids = s.resolve_series(&[series_key("line", &[("device".into(), "A".into())], "温度")]).unwrap();
    let res = q(&s, &ids, t, t + MIN, 100, Some(Tier::Raw));
    assert_eq!(res.series[0].t[0], t);
    assert!((res.series[0].avg[0].unwrap() - 20.0).abs() < 1e-5);
    assert_eq!(res.series[0].t.len(), 60);
}

#[test]
fn time_formats() {
    let dir = tempfile::tempdir().unwrap();
    let s = DuckStore::open(opts(dir.path())).unwrap();
    let t = floor_to(now_ms(), HOUR) - HOUR; // 分・秒が 0
    let jst_short = to_dt(t + 9 * HOUR).format("%Y/%-m/%-d %-H:%M").to_string();
    let iso_z = to_dt(t + SEC).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let iso_jst = to_dt(t + 2 * SEC + 9 * HOUR).format("%Y-%m-%dT%H:%M:%S+09:00").to_string();
    let csv = format!(
        "time,v\n{}\n{}\n{}\n{}\n{}\n",
        format!("{jst_short},1"),
        format!("{iso_z},2"),
        format!("{iso_jst},3"),
        format!("{},4", (t + 3 * SEC) / 1000),
        format!("{},5", t + 4 * SEC),
    );
    let p = write_csv(dir.path(), "t.csv", &csv);
    let r = s.import_csv(&p, &wide_opts(540)).unwrap();
    assert_eq!(r.skipped_rows, 0, "{r:?}");
    let res = q(&s, &[1], t, t + 10 * SEC, 100, Some(Tier::Raw));
    assert_eq!(res.series[0].t, vec![t, t + SEC, t + 2 * SEC, t + 3 * SEC, t + 4 * SEC]);
    assert_eq!(res.series[0].avg, vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0), Some(5.0)]);
}

#[test]
fn backfill_into_finalized_days_and_months() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    let key = series_key("line", &[("device".into(), "A".into())], "v");
    s.resolve_series(&[key.clone()]).unwrap();
    // 既存データ: 昨日の 00:00〜01:00、値 1
    let d = floor_to(now, DAY) - DAY;
    let pts: Vec<Point> = (0..3600).map(|i| Point { series_id: 1, ts_ms: d + i * SEC, value: 1.0 }).collect();
    s.write_points_at(&pts, now).unwrap();
    // 40日後まで進めて、日も月も確定させる
    s.run_maintenance(now + 40 * DAY).unwrap();
    let wm = s.watermarks();
    assert!(wm.raw_flushed_until > d && wm.m1_flushed_until > d, "{wm:?}");
    assert!(s.raw_path(d).exists());
    assert!(s.m1_path(d).exists());

    // 取り込み: 00:30〜01:30 を値 5 で（前半は上書き、後半は追加）、さらに 400 日前の日（新しいファイル、16bit）
    let old = floor_to(now, DAY) - 400 * DAY;
    let mut csv = String::from("time_utc,v\n");
    for i in 0..3600 {
        csv.push_str(&format!("{},5\n", to_dt(d + 30 * MIN + i * SEC).format("%Y-%m-%d %H:%M:%S")));
    }
    for i in 0..60 {
        csv.push_str(&format!("{},7\n", to_dt(old + i * MIN).format("%Y-%m-%d %H:%M:%S")));
    }
    let p = write_csv(dir.path(), "b.csv", &csv);
    let o = CsvImportOptions { tags: vec![("device".into(), "A".into())], ..wide_opts(540) };
    let r = s.import_csv(&p, &o).unwrap();
    assert_eq!(r.backfilled_days, 2, "{r:?}");
    assert_eq!(r.backfilled_points, 3660);
    assert_eq!(r.recent_points, 0);
    assert!(s.raw_q16_path(old).exists());

    let check = |s: &DuckStore| {
        // 生データ: 00:00〜00:30 は 1、00:30〜01:30 は 5
        let raw = q(s, &[1], d, d + 2 * HOUR, 7200, Some(Tier::Raw));
        assert_eq!(raw.series[0].t.len(), 5400);
        assert_eq!(raw.series[0].avg[0], Some(1.0));
        assert_eq!(raw.series[0].avg[1800], Some(5.0));
        // 1分集計（月次ファイル）と 1時間集計にも反映
        let m1 = q(s, &[1], d, d + 2 * HOUR, 120, Some(Tier::M1));
        assert_eq!(m1.series[0].t.len(), 90);
        assert_eq!(m1.series[0].avg[29], Some(1.0));
        assert_eq!(m1.series[0].avg[30], Some(5.0));
        let h1 = q(s, &[1], d, d + 2 * HOUR, 2, Some(Tier::H1));
        assert_eq!(h1.series[0].avg, vec![Some(3.0), Some(5.0)]);
        assert_eq!(h1.series[0].max[0], Some(5.0));
        let oldq = q(s, &[1], old, old + HOUR, 60, Some(Tier::Raw));
        assert_eq!(oldq.series[0].t.len(), 60);
        assert_eq!(oldq.series[0].avg[0], Some(7.0));
    };
    check(&s);
    // 同じファイルをもう一度取り込んでも同じ結果
    s.import_csv(&p, &o).unwrap();
    check(&s);
    // 再起動後も同じ
    drop(s);
    let s = DuckStore::open_at(opts(dir.path()), now).unwrap();
    check(&s);
}

#[test]
fn long_format_roundtrip_with_export() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let s = DuckStore::open(opts(dir.path())).unwrap();
    let keys = [
        series_key("plc", &[("device".into(), "dev01".into())], "temp"),
        series_key("plc", &[("device".into(), "dev 2".into())], "温度"),
    ];
    s.resolve_series(&keys).unwrap();
    let t = floor_to(now, MIN) - 20 * MIN;
    let pts: Vec<Point> = (0..600).flat_map(|i| [1u32, 2].map(|id| Point { series_id: id, ts_ms: t + i * SEC, value: (i % 7) as f32 + id as f32 })).collect();
    s.write_points_at(&pts, now).unwrap();
    let out = dir.path().join("e.csv");
    s.export(&ExportRequest { series: vec![1, 2], from_ms: t, to_ms: t + 10 * MIN, tier: Tier::Raw, format: ExportFormat::Csv }, &out).unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let s2 = DuckStore::open(opts(dir2.path())).unwrap();
    let r = s2.import_csv(&out, &CsvImportOptions { layout: CsvLayout::Long, ..Default::default() }).unwrap();
    assert_eq!(r.points, 1200, "{r:?}");
    assert_eq!(r.series, 2);
    let ids2 = s2.resolve_series(&keys).unwrap();
    let a = q(&s, &[1, 2], t, t + 10 * MIN, 1000, Some(Tier::Raw));
    let b = q(&s2, &ids2, t, t + 10 * MIN, 1000, Some(Tier::Raw));
    assert_eq!(a.series[0].avg, b.series[0].avg);
    assert_eq!(a.series[1].t, b.series[1].t);
}
