//! ストレージの抽象化。ClickHouse 版を後から追加できるよう trait で定義する。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub type SeriesId = u32;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    pub series_id: SeriesId,
    pub ts_ms: i64,
    pub value: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Series {
    pub id: SeriesId,
    pub key: String,
    pub measurement: String,
    pub field: String,
    pub tags: BTreeMap<String, String>,
    pub unit: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Tier {
    #[serde(rename = "raw")]
    Raw,
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "1h")]
    H1,
}

impl Tier {
    pub fn resolution_ms(self) -> i64 {
        match self {
            Tier::Raw => crate::time::SEC,
            Tier::M1 => crate::time::MIN,
            Tier::H1 => crate::time::HOUR,
        }
    }
    pub fn parse(s: &str) -> Option<Option<Tier>> {
        Some(match s {
            "" | "auto" => None,
            "raw" => Some(Tier::Raw),
            "1m" => Some(Tier::M1),
            "1h" => Some(Tier::H1),
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Tier::Raw => "raw",
            Tier::M1 => "1m",
            Tier::H1 => "1h",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AggQuery {
    pub series: Vec<SeriesId>,
    pub from_ms: i64,
    pub to_ms: i64,
    /// 画面の横幅（px）。1系列あたり最大でこの数のバケットを返す。
    pub width: u32,
    /// 層を強制する場合に指定。None なら自動選択。
    pub tier: Option<Tier>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SeriesAgg {
    pub id: SeriesId,
    /// バケット開始時刻（エポックミリ秒）。欠測区間には null 値の点が入る。
    pub t: Vec<i64>,
    pub min: Vec<Option<f32>>,
    pub max: Vec<Option<f32>>,
    pub avg: Vec<Option<f32>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AggResult {
    pub tier: Tier,
    pub bucket_ms: i64,
    /// false のときは min/max の帯を描く必要がない（バケット＝生データ解像度）
    pub band: bool,
    pub from_ms: i64,
    pub to_ms: i64,
    pub series: Vec<SeriesAgg>,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Latest {
    pub id: SeriesId,
    pub ts: i64,
    pub value: f32,
}

#[derive(Debug, Clone, Default)]
pub struct SeriesFilter {
    /// キーの部分一致（大文字小文字を無視）
    pub q: Option<String>,
    pub measurement: Option<String>,
    pub tags: Vec<(String, String)>,
    pub ids: Option<Vec<SeriesId>>,
    pub limit: Option<usize>,
}

impl SeriesFilter {
    pub fn matches(&self, s: &Series) -> bool {
        if let Some(ids) = &self.ids {
            if !ids.contains(&s.id) {
                return false;
            }
        }
        if let Some(m) = &self.measurement {
            if &s.measurement != m {
                return false;
            }
        }
        for (k, v) in &self.tags {
            if s.tags.get(k) != Some(v) {
                return false;
            }
        }
        if let Some(q) = &self.q {
            let q = q.to_lowercase();
            if !q.split_whitespace().all(|w| s.key.to_lowercase().contains(w)) {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct WriteOutcome {
    pub written: usize,
    /// 猶予期間を過ぎていて拒否した点
    pub rejected_old: usize,
    /// 未来すぎて拒否した点
    pub rejected_future: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExportFormat {
    Csv,
    Parquet,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRequest {
    pub series: Vec<SeriesId>,
    pub from_ms: i64,
    pub to_ms: i64,
    pub tier: Tier,
    pub format: ExportFormat,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct JobState {
    pub name: String,
    pub last_run: Option<i64>,
    pub last_ok: Option<i64>,
    /// 実際に処理対象があった最後の時刻
    pub last_work: Option<i64>,
    pub last_duration_ms: Option<u64>,
    pub last_error: Option<String>,
    pub runs: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Watermarks {
    /// これより前の生データは受け付けない
    pub raw_accept_from: i64,
    /// これより前の生データは Parquet に確定済み
    pub raw_flushed_until: i64,
    /// これより前の日は 16bit に再圧縮済み
    pub raw_compacted_until: i64,
    /// 1分集計が済んだ時刻
    pub m1_until: i64,
    /// 1時間集計が済んだ時刻
    pub h1_until: i64,
    /// これより前の1分集計は Parquet に確定済み
    pub m1_flushed_until: i64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StoreStatus {
    pub series_count: usize,
    pub raw_hot_rows: u64,
    pub r1m_hot_rows: u64,
    pub r1h_rows: u64,
    pub db_file_bytes: u64,
    pub watermarks: Watermarks,
    pub jobs: Vec<JobState>,
    pub written_points: u64,
    pub rejected_points: u64,
}

// ---- CSV 取り込み ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CsvLayout {
    Wide,
    Long,
}

#[derive(Debug, Clone)]
pub struct CsvImportOptions {
    pub layout: CsvLayout,
    /// wide のときの measurement
    pub measurement: String,
    /// wide のときに全列へ付けるタグ
    pub tags: Vec<(String, String)>,
    /// 時刻の列名（省略時は先頭列）
    pub time_column: Option<String>,
    /// タイムゾーンの無い日時の UTC からのずれ（分）。日本時間なら 540。
    pub utc_offset_minutes: i32,
}

impl Default for CsvImportOptions {
    fn default() -> Self {
        Self { layout: CsvLayout::Wide, measurement: "csv".into(), tags: vec![], time_column: None, utc_offset_minutes: 0 }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ImportReport {
    /// CSV の行数
    pub rows: u64,
    /// 時刻を読めなかった行
    pub skipped_rows: u64,
    /// 数値でなかったセル（空欄は数えない）
    pub skipped_cells: u64,
    /// 取り込んだ点（重複を除く）
    pub points: u64,
    pub series: usize,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// 直近（未確定）の層へ書いた点
    pub recent_points: u64,
    /// 確定済みの日へ書き足した点
    pub backfilled_points: u64,
    pub backfilled_days: usize,
    /// 生データの保持期間より古くて捨てた点
    pub rejected_old: u64,
    /// 1日以上先の未来で捨てた点
    pub rejected_future: u64,
    pub elapsed_ms: u64,
}

pub trait Store: Send + Sync {
    /// 系列キー（`lp::series_key` の形式）を ID に変換する。未知のキーは登録する。
    fn resolve_series(&self, keys: &[String]) -> Result<Vec<SeriesId>>;
    fn write_batch(&self, points: &[Point]) -> Result<WriteOutcome>;
    /// 層の選択も含む
    fn query_agg(&self, q: &AggQuery) -> Result<AggResult>;
    fn latest(&self, series: &[SeriesId]) -> Result<Vec<Latest>>;
    fn list_series(&self, filter: &SeriesFilter) -> Result<Vec<Series>>;
    fn set_series_unit(&self, id: SeriesId, unit: Option<String>) -> Result<()>;
    /// 系列キーを変える（ID とデータはそのまま）。新しいキーが既にあればエラー
    fn rename_series(&self, id: SeriesId, new_key: &str) -> Result<Series>;
    /// 系列を削除する。直近の DB 上のデータは消し、確定済みの Parquet の行は保持期間で消える
    fn delete_series(&self, ids: &[SeriesId]) -> Result<usize>;
    /// 集計、フラッシュ、再圧縮、保持期間
    fn run_maintenance(&self, now_ms: i64) -> Result<()>;
    fn status(&self) -> Result<StoreStatus>;
    /// 指定範囲を `out` に書き出し、行数を返す
    fn export(&self, req: &ExportRequest, out: &Path) -> Result<u64>;
    /// DB をチェックポイントしてから `dest` にバックアップする
    fn backup(&self, dest: &Path) -> Result<BackupReport>;
    fn checkpoint(&self) -> Result<()>;
    /// CSV を取り込む（確定済みの日も作り直す）
    fn import_csv(&self, path: &Path, opt: &CsvImportOptions) -> Result<ImportReport>;
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct BackupReport {
    pub db_bytes: u64,
    pub files_copied: u64,
    pub files_skipped: u64,
    pub bytes_copied: u64,
    pub elapsed_ms: u64,
}
