//! parqlog — 時系列データサービス
//!
//! ```text
//! parqlog run                         サービス本体（systemd から起動。Ctrl+C / SIGTERM で停止）
//! parqlog import FILE...              CSV を取り込む（稼働中は API 経由、停止中は直接）
//! parqlog backup --dest DIR           バックアップ（同上）
//! parqlog series list|rename|delete   系列の一覧・名前変更・削除（同上）
//! parqlog check-config | init-config  設定ファイルの検証・既定値での作成
//! parqlog apply-config --from FILE    設定ファイルを検証して反映（管理アプリから pkexec で呼ぶ）
//! ```

mod logging;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use parqlog_core::{Config, CsvImportOptions, CsvLayout, DuckStore, Store};

#[derive(Parser)]
#[command(name = "parqlog", version, about = "時系列データの収集・保存・クエリを行うサービス")]
struct Cli {
    /// 設定ファイル（既定: /etc/parqlog/parqlog.toml。Windows はカレントフォルダの parqlog.toml）
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Layout {
    /// 時刻,列1,列2,...（列名が項目名になる）
    Wide,
    /// 時刻,系列キー,値（エクスポートした CSV と同じ形）
    Long,
}

#[derive(Subcommand)]
enum Cmd {
    /// サービスを実行する
    Run,
    /// CSV を取り込む
    Import {
        files: Vec<PathBuf>,
        #[arg(long, value_enum, default_value = "wide")]
        layout: Layout,
        /// wide のときの measurement（省略時はファイル名）
        #[arg(long)]
        measurement: Option<String>,
        /// wide のときに付けるタグ（例: --tag device=dev01、複数可）
        #[arg(long = "tag", value_name = "KEY=VALUE")]
        tags: Vec<String>,
        /// 時刻の列名（省略時は先頭列）
        #[arg(long)]
        time_column: Option<String>,
        /// タイムゾーンの無い日時の UTC からのずれ（例: +09:00）。省略時はこの PC の地方時
        #[arg(long, allow_hyphen_values = true)]
        utc_offset: Option<String>,
    },
    /// 系列の一覧・名前変更・削除
    Series {
        #[command(subcommand)]
        cmd: SeriesCmd,
    },
    /// DB と Parquet ファイルをバックアップする
    Backup {
        #[arg(long)]
        dest: PathBuf,
    },
    /// 設定ファイルを検証して所定の場所に反映する
    ApplyConfig {
        #[arg(long)]
        from: PathBuf,
    },
    /// 設定ファイルを検証する
    CheckConfig,
    /// 既定値の設定ファイルを書き出す（既にあれば何もしない）
    InitConfig,
}

#[derive(Subcommand)]
enum SeriesCmd {
    /// 一覧（ID と系列キー）。QUERY でキーを部分一致検索
    List { query: Option<String> },
    /// 系列キーを変える（データはそのまま）。例: parqlog series rename 12 'plc,device=dev01 temp'
    Rename { id: u32, key: String },
    /// 系列を削除する
    Delete {
        ids: Vec<u32>,
        /// 確認せずに削除する
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = real_main(cli) {
        eprintln!("エラー: {e:#}");
        std::process::exit(1);
    }
}

fn config_path(cli: &Cli) -> PathBuf {
    let p = cli.config.clone().unwrap_or_else(parqlog_core::config::default_config_path);
    if p.is_relative() {
        std::env::current_dir().map(|d| d.join(&p)).unwrap_or(p)
    } else {
        p
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let path = config_path(&cli);
    match cli.cmd {
        Cmd::Run => {
            logging::init();
            let cfg = Config::load_or_default(&path)?;
            tracing::info!(config = %path.display(), exists = path.exists(), "parqlog {} を起動", parqlog_core::VERSION);
            run_server(cfg, path)
        }
        Cmd::Import { files, layout, measurement, tags, time_column, utc_offset } => {
            if files.is_empty() {
                bail!("取り込む CSV ファイルを指定してください");
            }
            let cfg = Config::load_or_default(&path)?;
            let mut tag_pairs = vec![];
            for t in &tags {
                let (k, v) = t.split_once('=').with_context(|| format!("--tag は KEY=VALUE の形にしてください: {t}"))?;
                tag_pairs.push((k.to_string(), v.to_string()));
            }
            let offset = match &utc_offset {
                Some(s) => parqlog_server::parse_offset(s).with_context(|| format!("--utc-offset が不正です: {s}"))?,
                None => parqlog_server::local_offset_minutes(),
            };
            for f in &files {
                let opt = CsvImportOptions {
                    layout: match layout {
                        Layout::Wide => CsvLayout::Wide,
                        Layout::Long => CsvLayout::Long,
                    },
                    measurement: measurement.clone().unwrap_or_else(|| {
                        f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "csv".into())
                    }),
                    tags: tag_pairs.clone(),
                    time_column: time_column.clone(),
                    utc_offset_minutes: offset,
                };
                let rep = import(&cfg, f, &opt)?;
                let n = |k: &str| rep[k].as_u64().unwrap_or(0);
                println!(
                    "{}: {} 点（{} 系列）を取り込みました。うち過去 {} 日分を作り直し。読めなかった行 {}・セル {}。{:.1} 秒",
                    f.display(),
                    n("points"),
                    n("series"),
                    n("backfilled_days"),
                    n("skipped_rows"),
                    n("skipped_cells"),
                    n("elapsed_ms") as f64 / 1000.0
                );
                if n("rejected_old") + n("rejected_future") > 0 {
                    println!("  保持期間より古い、または未来の時刻で捨てた点: {}", n("rejected_old") + n("rejected_future"));
                }
            }
            Ok(())
        }
        Cmd::Backup { dest } => backup(&path, dest),
        Cmd::Series { cmd } => series(&Config::load_or_default(&path)?, cmd),
        Cmd::ApplyConfig { from } => {
            let cfg = Config::load(&from)?;
            cfg.save(&path)?;
            println!("設定を反映しました: {}", path.display());
            Ok(())
        }
        Cmd::CheckConfig => {
            let cfg = Config::load(&path)?;
            println!("設定は正しい形式です: {}\n{}", path.display(), cfg.to_toml()?);
            Ok(())
        }
        Cmd::InitConfig => {
            if path.exists() {
                Config::load(&path)?;
            } else {
                Config::default().save(&path).with_context(|| format!("設定ファイルを作れません: {}", path.display()))?;
            }
            println!("{}", path.display());
            Ok(())
        }
    }
}

fn run_server(cfg: Config, path: PathBuf) -> Result<()> {
    let store = Arc::new(DuckStore::open(cfg.store_options())?);
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("parqlog-http").build()?;
    let data_dir = cfg.data_dir();
    let store: Arc<dyn Store> = store;
    rt.block_on(parqlog_server::serve(cfg, path, store, data_dir, shutdown_signal()))
}

/// Ctrl+C か SIGTERM（systemd の停止）を待つ
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn api_url(cfg: &Config, path: &str) -> String {
    format!("http://{}{path}", cfg.http.listen.replace("0.0.0.0", "127.0.0.1"))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(24 * 3600)))
        .http_status_as_error(false)
        .build()
        .into()
}

fn is_conn_error(e: &ureq::Error) -> bool {
    matches!(e, ureq::Error::Io(_) | ureq::Error::ConnectionFailed | ureq::Error::HostNotFound)
}

/// サービスが動いていれば API へ送り、止まっていればデータフォルダを直接開いて取り込む
fn import(cfg: &Config, file: &Path, opt: &CsvImportOptions) -> Result<serde_json::Value> {
    let mut f = std::fs::File::open(file).with_context(|| format!("開けません: {}", file.display()))?;
    let mut req = agent()
        .post(api_url(cfg, "/api/import/csv"))
        .query("layout", if opt.layout == CsvLayout::Long { "long" } else { "wide" })
        .query("measurement", &opt.measurement)
        .query("utc_offset", opt.utc_offset_minutes.to_string());
    if let Some(t) = &opt.time_column {
        req = req.query("time_column", t);
    }
    for (k, v) in &opt.tags {
        req = req.query(format!("tag.{k}"), v);
    }
    match req.content_type("text/csv").send(ureq::SendBody::from_reader(&mut f)) {
        Ok(mut res) => {
            let body = res.body_mut().read_to_string().unwrap_or_default();
            if !res.status().is_success() {
                bail!("取り込みに失敗しました（{}）: {body}", res.status());
            }
            Ok(serde_json::from_str(&body)?)
        }
        Err(e) if is_conn_error(&e) => {
            let store = DuckStore::open(cfg.store_options())
                .context("サービスに接続できず、データフォルダも開けません")?;
            let rep = store.import_csv_file(file, opt)?;
            Ok(serde_json::to_value(&rep)?)
        }
        Err(e) => bail!("サービスへの接続でエラー: {e}"),
    }
}

fn backup(path: &Path, dest: PathBuf) -> Result<()> {
    let cfg = Config::load_or_default(path)?;
    let dest = if dest.is_absolute() { dest } else { std::env::current_dir()?.join(dest) };
    match agent().post(api_url(&cfg, "/api/backup")).send_json(serde_json::json!({ "dest": dest.to_string_lossy() })) {
        Ok(mut res) => {
            let body = res.body_mut().read_to_string().unwrap_or_default();
            if !res.status().is_success() {
                bail!("サービスがバックアップに失敗しました（{}）: {body}", res.status());
            }
            println!("バックアップ完了（サービス経由）: {}\n{body}", dest.display());
            Ok(())
        }
        Err(e) if is_conn_error(&e) => {
            println!("サービスに接続できないため、データフォルダを直接開いてバックアップします");
            let rep = DuckStore::open(cfg.store_options())?.backup(&dest)?;
            println!("バックアップ完了: {}\n{}", dest.display(), serde_json::to_string_pretty(&rep)?);
            Ok(())
        }
        Err(e) => bail!("サービスへの接続でエラー: {e}"),
    }
}

/// サービスが動いていれば API、止まっていればデータフォルダを直接開く
enum Target {
    Api,
    Direct(DuckStore),
}

fn target(cfg: &Config) -> Result<Target> {
    match agent().get(api_url(cfg, "/health")).call() {
        Ok(_) => Ok(Target::Api),
        Err(e) if is_conn_error(&e) => Ok(Target::Direct(
            DuckStore::open(cfg.store_options()).context("サービスに接続できず、データフォルダも開けません")?,
        )),
        Err(e) => bail!("サービスへの接続でエラー: {e}"),
    }
}

fn api_call(req: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<String> {
    let mut res = req?;
    let body = res.body_mut().read_to_string().unwrap_or_default();
    if !res.status().is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(String::from))
            .unwrap_or(body);
        bail!("{msg}");
    }
    Ok(body)
}

fn series(cfg: &Config, cmd: SeriesCmd) -> Result<()> {
    let t = target(cfg)?;
    match cmd {
        SeriesCmd::List { query } => {
            let list: Vec<parqlog_core::Series> = match &t {
                Target::Api => {
                    let mut r = agent().get(api_url(cfg, "/api/series"));
                    if let Some(q) = &query {
                        r = r.query("q", q);
                    }
                    serde_json::from_str(&api_call(r.call())?)?
                }
                Target::Direct(s) => s.list_series(&parqlog_core::SeriesFilter { q: query, ..Default::default() })?,
            };
            for s in &list {
                println!("{:>6}  {}{}", s.id, s.key, s.unit.as_deref().map(|u| format!("  [{u}]")).unwrap_or_default());
            }
            println!("{} 系列", list.len());
        }
        SeriesCmd::Rename { id, key } => {
            let new_key = match &t {
                Target::Api => {
                    let body = api_call(agent().put(api_url(cfg, &format!("/api/series/{id}"))).send_json(serde_json::json!({ "key": key })))?;
                    serde_json::from_str::<parqlog_core::Series>(&body)?.key
                }
                Target::Direct(s) => s.rename_series(id, &key)?.key,
            };
            println!("系列 {id} を '{new_key}' に変更しました");
        }
        SeriesCmd::Delete { ids, yes } => {
            if ids.is_empty() {
                bail!("削除する系列の ID を指定してください");
            }
            if !yes {
                eprint!("系列 {ids:?} を削除します。よろしいですか？ [y/N] ");
                let mut a = String::new();
                std::io::stdin().read_line(&mut a)?;
                if !matches!(a.trim(), "y" | "Y" | "yes") {
                    println!("中止しました");
                    return Ok(());
                }
            }
            for id in ids {
                match &t {
                    Target::Api => {
                        api_call(agent().delete(api_url(cfg, &format!("/api/series/{id}"))).call())?;
                    }
                    Target::Direct(s) => {
                        if s.delete_series(&[id])? == 0 {
                            bail!("系列 {id} がありません");
                        }
                    }
                }
                println!("系列 {id} を削除しました");
            }
        }
    }
    Ok(())
}
