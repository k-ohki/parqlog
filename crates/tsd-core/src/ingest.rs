//! 取り込み: Line Protocol を解析してバッファに積み、1秒ごと（または1万点ごと）にまとめて書き込む。

use crate::lp::{self, LineError, Precision};
use crate::store::{Point, Store};
use crate::time::now_ms;
use anyhow::Result;
use parking_lot::{Condvar, Mutex};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tracing::{error, warn};

pub const FLUSH_INTERVAL: Duration = Duration::from_secs(1);
pub const FLUSH_POINTS: usize = 10_000;
/// これ以上溜まったら 503 を返して送信側に再送させる
pub const MAX_BUFFER: usize = 2_000_000;

#[derive(Debug, Default, Serialize)]
pub struct IngestResult {
    pub lines: usize,
    pub points: usize,
    /// 文字列フィールドなど保存しない値
    pub skipped_fields: usize,
    pub errors: Vec<LineError>,
}

#[derive(Debug)]
pub enum IngestError {
    Busy,
    Other(anyhow::Error),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Busy => write!(f, "書き込みバッファが満杯です。しばらくして再送してください"),
            IngestError::Other(e) => write!(f, "{e:#}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IngestStats {
    pub points_per_sec: f64,
    pub received_points: u64,
    pub written_points: u64,
    pub rejected_old: u64,
    pub rejected_future: u64,
    pub parse_errors: u64,
    pub skipped_fields: u64,
    pub buffer_len: usize,
    pub write_errors: u64,
    pub last_write_error: Option<String>,
}

struct Shared {
    buf: Mutex<Vec<Point>>,
    cv: Condvar,
    stop: AtomicBool,
    received: AtomicU64,
    written: AtomicU64,
    rejected_old: AtomicU64,
    rejected_future: AtomicU64,
    parse_errors: AtomicU64,
    skipped: AtomicU64,
    write_errors: AtomicU64,
    last_error: Mutex<Option<String>>,
    rate: Mutex<VecDeque<(Instant, u64)>>,
    warned_strings: Mutex<Instant>,
}

pub struct Ingestor {
    store: Arc<dyn Store>,
    sh: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Ingestor {
    pub fn start(store: Arc<dyn Store>) -> Arc<Self> {
        let sh = Arc::new(Shared {
            buf: Mutex::new(Vec::with_capacity(FLUSH_POINTS)),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            received: AtomicU64::new(0),
            written: AtomicU64::new(0),
            rejected_old: AtomicU64::new(0),
            rejected_future: AtomicU64::new(0),
            parse_errors: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
            last_error: Mutex::new(None),
            rate: Mutex::new(VecDeque::new()),
            warned_strings: Mutex::new(Instant::now() - Duration::from_secs(3600)),
        });
        let st = store.clone();
        let sh2 = sh.clone();
        let th = std::thread::Builder::new()
            .name("tsd-ingest".into())
            .spawn(move || flusher(st, sh2))
            .expect("取り込みスレッドを起動できません");
        Arc::new(Self { store, sh, thread: Mutex::new(Some(th)) })
    }

    /// Line Protocol の本文を取り込む。構文エラーの行は飛ばし、正しい行だけを書き込む。
    pub fn ingest_lp(&self, body: &str, precision: Precision) -> Result<IngestResult, IngestError> {
        self.ingest_lp_with(body, precision, true)
    }

    /// `accept_partial = false` なら、1行でも誤りがあれば何も書き込まない（InfluxDB 3 の accept_partial）
    pub fn ingest_lp_with(&self, body: &str, precision: Precision, accept_partial: bool) -> Result<IngestResult, IngestError> {
        if self.sh.buf.lock().len() >= MAX_BUFFER {
            return Err(IngestError::Busy);
        }
        let now = now_ms();
        let (lines, errors) = lp::parse_lines(body);
        let mut res = IngestResult { lines: lines.len(), errors, ..Default::default() };
        if !res.errors.is_empty() {
            self.sh.parse_errors.fetch_add(res.errors.len() as u64, Ordering::Relaxed);
            if !accept_partial {
                return Ok(res);
            }
        }

        // 系列キーをまとめて ID に変換
        let mut keys: Vec<String> = Vec::new();
        let mut key_idx: HashMap<String, usize> = HashMap::new();
        let mut pending: Vec<(usize, i64, f32)> = Vec::new();
        for l in &lines {
            let ts = l.timestamp.map(|t| precision.to_ms(t)).unwrap_or(now);
            for (field, v) in &l.fields {
                let Some(v) = v.as_f64() else {
                    res.skipped_fields += 1;
                    continue;
                };
                let v32 = v as f32;
                if !v32.is_finite() {
                    res.skipped_fields += 1;
                    continue;
                }
                let key = lp::series_key(&l.measurement, &l.tags, field);
                let i = match key_idx.get(&key) {
                    Some(&i) => i,
                    None => {
                        key_idx.insert(key.clone(), keys.len());
                        keys.push(key);
                        keys.len() - 1
                    }
                };
                pending.push((i, ts, v32));
            }
        }
        if res.skipped_fields > 0 {
            self.sh.skipped.fetch_add(res.skipped_fields as u64, Ordering::Relaxed);
            let mut last = self.sh.warned_strings.lock();
            if last.elapsed() > Duration::from_secs(60) {
                *last = Instant::now();
                warn!(count = res.skipped_fields, "数値でないフィールド（文字列や範囲外の値）を破棄しました");
            }
        }
        if pending.is_empty() {
            return Ok(res);
        }
        let ids = self.store.resolve_series(&keys).map_err(IngestError::Other)?;
        let points: Vec<Point> =
            pending.into_iter().map(|(i, ts, value)| Point { series_id: ids[i], ts_ms: ts, value }).collect();
        res.points = points.len();
        self.sh.received.fetch_add(points.len() as u64, Ordering::Relaxed);

        let mut buf = self.sh.buf.lock();
        buf.extend_from_slice(&points);
        if buf.len() >= FLUSH_POINTS {
            self.sh.cv.notify_one();
        }
        Ok(res)
    }

    /// 解析済みの点を直接積む（テストや一括投入用）
    pub fn push(&self, points: &[Point]) {
        self.sh.received.fetch_add(points.len() as u64, Ordering::Relaxed);
        let mut buf = self.sh.buf.lock();
        buf.extend_from_slice(points);
        if buf.len() >= FLUSH_POINTS {
            self.sh.cv.notify_one();
        }
    }

    /// バッファを今すぐ書き出す
    pub fn flush_now(&self) {
        flush_once(&*self.store, &self.sh);
    }

    pub fn stats(&self) -> IngestStats {
        let sh = &self.sh;
        let rate = {
            let r = sh.rate.lock();
            match (r.front(), r.back()) {
                (Some(a), Some(b)) if b.0 > a.0 => (b.1 - a.1) as f64 / (b.0 - a.0).as_secs_f64(),
                _ => 0.0,
            }
        };
        IngestStats {
            points_per_sec: rate,
            received_points: sh.received.load(Ordering::Relaxed),
            written_points: sh.written.load(Ordering::Relaxed),
            rejected_old: sh.rejected_old.load(Ordering::Relaxed),
            rejected_future: sh.rejected_future.load(Ordering::Relaxed),
            parse_errors: sh.parse_errors.load(Ordering::Relaxed),
            skipped_fields: sh.skipped.load(Ordering::Relaxed),
            buffer_len: sh.buf.lock().len(),
            write_errors: sh.write_errors.load(Ordering::Relaxed),
            last_write_error: sh.last_error.lock().clone(),
        }
    }

    /// 停止: バッファを書き切ってからスレッドを終える
    pub fn shutdown(&self) {
        self.sh.stop.store(true, Ordering::SeqCst);
        self.sh.cv.notify_all();
        if let Some(h) = self.thread.lock().take() {
            let _ = h.join();
        }
        flush_once(&*self.store, &self.sh);
    }
}

fn flush_once(store: &dyn Store, sh: &Shared) {
    let batch = std::mem::take(&mut *sh.buf.lock());
    if batch.is_empty() {
        return;
    }
    match store.write_batch(&batch) {
        Ok(o) => {
            sh.written.fetch_add(o.written as u64, Ordering::Relaxed);
            if o.rejected_old > 0 || o.rejected_future > 0 {
                sh.rejected_old.fetch_add(o.rejected_old as u64, Ordering::Relaxed);
                sh.rejected_future.fetch_add(o.rejected_future as u64, Ordering::Relaxed);
                warn!(
                    old = o.rejected_old,
                    future = o.rejected_future,
                    "受け付け範囲外の時刻のデータを拒否しました（猶予期間を過ぎた過去、または1日以上先の未来）"
                );
            }
        }
        Err(e) => {
            sh.write_errors.fetch_add(1, Ordering::Relaxed);
            *sh.last_error.lock() = Some(format!("{e:#}"));
            error!(error = %format!("{e:#}"), points = batch.len(), "書き込みに失敗しました。再試行します");
            // 失敗した分を先頭に戻す（上限を超えたら古い方を捨てる）
            let mut buf = sh.buf.lock();
            let mut merged = batch;
            merged.extend_from_slice(&buf);
            if merged.len() > MAX_BUFFER {
                let drop_n = merged.len() - MAX_BUFFER;
                merged.drain(0..drop_n);
                error!(dropped = drop_n, "バッファ上限を超えたためデータを破棄しました");
            }
            *buf = merged;
        }
    }
}

fn flusher(store: Arc<dyn Store>, sh: Arc<Shared>) {
    let mut next = Instant::now() + FLUSH_INTERVAL;
    loop {
        {
            let mut buf = sh.buf.lock();
            while !sh.stop.load(Ordering::SeqCst) && buf.len() < FLUSH_POINTS && Instant::now() < next {
                sh.cv.wait_until(&mut buf, next);
            }
        }
        if sh.stop.load(Ordering::SeqCst) {
            return;
        }
        let failed_before = sh.write_errors.load(Ordering::Relaxed);
        flush_once(&*store, &sh);
        if sh.write_errors.load(Ordering::Relaxed) != failed_before {
            std::thread::sleep(Duration::from_secs(1));
        }
        let now = Instant::now();
        if now >= next {
            next = now + FLUSH_INTERVAL;
        }
        let mut r = sh.rate.lock();
        r.push_back((now, sh.written.load(Ordering::Relaxed)));
        while r.len() > 2 && now.duration_since(r.front().unwrap().0) > Duration::from_secs(10) {
            r.pop_front();
        }
    }
}
