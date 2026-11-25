//! parqlog の HTTP API（axum）。
//!
//! - 取り込み: `POST /api/v3/write_lp`（InfluxDB 3 互換）、`/api/v2/write`（v2 互換）、`/write`（v1 互換）
//! - クエリ:   `GET /api/query`、`/api/series`、`/api/latest`、`/api/devices`、`/api/status`
//! - 管理:     `/api/export`、`/api/import/csv`、`/api/backup`、`/api/config`（Tauri アプリ用）
//! - Grafana:  Prometheus 互換 `/api/v1/*`（`prom` モジュール）

mod auth;
mod handlers;
pub mod prom;

use anyhow::{Context, Result};
use axum::extract::DefaultBodyLimit;
use axum::routing::{get, post, put};
use axum::{middleware, Extension, Router};
use parking_lot::Mutex;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::decompression::RequestDecompressionLayer;
use tracing::{error, info, warn};
use parqlog_core::{Config, Ingestor, Store};

pub use auth::ListenerKind;
pub use handlers::{local_offset_minutes, parse_offset};

pub struct AppState {
    pub cfg: Config,
    pub config_path: PathBuf,
    pub store: Arc<dyn Store>,
    pub ingest: Arc<Ingestor>,
    pub data_dir: PathBuf,
    pub started: Instant,
    pub disk_cache: Mutex<Option<(Instant, handlers::DiskInfo)>>,
}

pub type SharedState = Arc<AppState>;

/// Tauri アプリ（WebView）と開発用 Vite から呼べるオリジン
pub const ALLOWED_ORIGINS: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "http://localhost:1420",
    "http://127.0.0.1:1420",
];

pub fn router(state: SharedState, kind: ListenerKind) -> Router {
    let max_body = state.cfg.http.max_body_mb.max(1) * 1024 * 1024;

    let write_routes = Router::new()
        .route("/api/v3/write_lp", post(handlers::write_v3))
        .route("/api/v2/write", post(handlers::write_v2))
        .route("/write", post(handlers::write_v1))
        .route("/ping", get(handlers::ping).head(handlers::ping))
        .route("/health", get(handlers::health));

    let mut app = write_routes;
    if kind == ListenerKind::Api {
        app = app
            .route("/api/query", get(handlers::query))
            .route("/api/series", get(handlers::series))
            .route("/api/series/{id}", put(handlers::series_update).delete(handlers::series_delete))
            .route("/api/latest", get(handlers::latest))
            .route("/api/devices", get(handlers::devices))
            .route("/api/status", get(handlers::status))
            .route("/api/config", get(handlers::config_get).put(handlers::config_put))
            .route("/api/export", post(handlers::export))
            .route("/api/import/csv", post(handlers::import_csv).layer(DefaultBodyLimit::disable()))
            .route("/api/backup", post(handlers::backup))
            .merge(prom::routes());
    }

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|o, _| o.to_str().map(|s| ALLOWED_ORIGINS.contains(&s)).unwrap_or(false)))
        .allow_methods([axum::http::Method::GET, axum::http::Method::POST, axum::http::Method::PUT, axum::http::Method::DELETE])
        .allow_headers([axum::http::header::CONTENT_TYPE, axum::http::header::AUTHORIZATION])
        .expose_headers([axum::http::header::CONTENT_DISPOSITION, axum::http::HeaderName::from_static("x-parqlog-rows")]);

    app.layer(middleware::from_fn_with_state(state.clone(), auth::guard))
        .layer(Extension(kind))
        .layer(RequestDecompressionLayer::new().gzip(true))
        .layer(DefaultBodyLimit::max(max_body))
        .layer(cors)
        .with_state(state)
}

/// 共有状態を作る（取り込みスレッドも開始する）
pub fn build_state(cfg: Config, config_path: PathBuf, store: Arc<dyn Store>, data_dir: PathBuf) -> Result<SharedState> {
    let ingest = Ingestor::start(store.clone());
    handlers::cleanup_tmp(&data_dir);
    Ok(Arc::new(AppState {
        cfg,
        config_path,
        store,
        ingest,
        data_dir,
        started: Instant::now(),
        disk_cache: Mutex::new(None),
    }))
}

/// サーバーを起動し、`shutdown` が完了するまで動かす。終了時はバッファを書き切ってからチェックポイントする。
pub async fn serve(
    cfg: Config,
    config_path: PathBuf,
    store: Arc<dyn Store>,
    data_dir: PathBuf,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = build_state(cfg.clone(), config_path, store.clone(), data_dir)?;
    let ingest = state.ingest.clone();

    // 定期ジョブ（5秒ごと。各ジョブは自分で実行時刻を判断する）
    let (stop_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    let maint = {
        let store = store.clone();
        let mut stop = stop_tx.subscribe();
        tokio::spawn(async move {
            let mut iv = tokio::time::interval(Duration::from_secs(5));
            iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = iv.tick() => {}
                    _ = stop.recv() => break,
                }
                let s = store.clone();
                let r = tokio::task::spawn_blocking(move || s.run_maintenance(parqlog_core::time::now_ms())).await;
                if let Ok(Err(e)) = r {
                    error!(error = %format!("{e:#}"), "定期ジョブでエラー");
                }
            }
        })
    };

    let api_addr: SocketAddr = cfg.http.listen.parse()?;
    let api_listener = tokio::net::TcpListener::bind(api_addr)
        .await
        .with_context(|| format!("{api_addr} で待ち受けできません（ポートが使用中の可能性）"))?;
    info!(addr = %api_addr, "HTTP API を開始");
    if !api_addr.ip().is_loopback() {
        warn!("http.listen がローカル以外に公開されています。ローカル以外からのアクセスにはトークンが必要です");
    }

    let mut servers = vec![];
    let mut stop = stop_tx.subscribe();
    let api = router(state.clone(), ListenerKind::Api)
        .into_make_service_with_connect_info::<SocketAddr>();
    servers.push(tokio::spawn(async move {
        axum::serve(api_listener, api)
            .with_graceful_shutdown(async move {
                let _ = stop.recv().await;
            })
            .await
    }));

    if !cfg.http.ingest_listen.is_empty() {
        let addr: SocketAddr = cfg.http.ingest_listen.parse()?;
        let l = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("{addr} で待ち受けできません（ポートが使用中の可能性）"))?;
        info!(addr = %addr, "取り込み専用ポートを開始（トークン必須）");
        let app = router(state.clone(), ListenerKind::Ingest).into_make_service_with_connect_info::<SocketAddr>();
        let mut stop = stop_tx.subscribe();
        servers.push(tokio::spawn(async move {
            axum::serve(l, app)
                .with_graceful_shutdown(async move {
                    let _ = stop.recv().await;
                })
                .await
        }));
    }

    shutdown.await;
    info!("停止要求を受けました。書き込み中のデータを保存しています");
    let _ = stop_tx.send(());
    for s in servers {
        let _ = tokio::time::timeout(Duration::from_secs(10), s).await;
    }
    let _ = maint.await;
    let st = store.clone();
    tokio::task::spawn_blocking(move || {
        ingest.shutdown();
        if let Err(e) = st.checkpoint() {
            error!(error = %e, "チェックポイントに失敗");
        }
    })
    .await?;
    info!("停止しました");
    Ok(())
}
