//! TSD デスクトップアプリ（Tauri 2）
//!
//! データはすべて tsd.exe の HTTP API から画面（WebView）が直接取得する。
//! Rust 側は OS に関わる操作（systemd の操作、設定ファイル、ログ、ファイルの読み書き、タスクトレイ）だけを担う。

mod service;

use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, WindowEvent};

// ---- 設定ファイルの場所と API の URL -----------------------------------------

fn config_path() -> PathBuf {
    std::env::var("TSD_CONFIG").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/etc/tsd/tsd.toml"))
}

fn read_config_file() -> Option<toml::Value> {
    std::fs::read_to_string(config_path()).ok().and_then(|s| toml::from_str(&s).ok())
}

fn cfg_str(cfg: &Option<toml::Value>, section: &str, key: &str) -> Option<String> {
    cfg.as_ref()?.get(section)?.get(key)?.as_str().map(String::from)
}

fn api_base_url() -> String {
    if let Ok(u) = std::env::var("TSD_API") {
        return u;
    }
    let listen = cfg_str(&read_config_file(), "http", "listen").unwrap_or_else(|| "127.0.0.1:8086".into());
    let listen = listen.replace("0.0.0.0", "127.0.0.1").replace("[::]", "[::1]");
    format!("http://{listen}")
}

/// PATH から tsd を探す（pkexec には絶対パスが必要）
fn tsd_exe() -> Option<PathBuf> {
    std::env::var_os("PATH")
        .and_then(|p| std::env::split_paths(&p).map(|d| d.join("tsd")).find(|f| f.is_file()))
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(timeout)).http_status_as_error(false).build().into()
}

fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> impl std::future::Future<Output = Result<T, String>> {
    async move { tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())? }
}

// ---- コマンド ---------------------------------------------------------------

#[derive(Serialize)]
struct AppInfo {
    version: String,
    api_base: String,
    config_path: String,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: env!("CARGO_PKG_VERSION").into(),
        api_base: api_base_url(),
        config_path: config_path().to_string_lossy().into(),
    }
}

#[tauri::command]
fn api_base() -> String {
    api_base_url()
}

#[tauri::command]
async fn service_status() -> Result<service::ServiceInfo, String> {
    blocking(|| Ok(service::status())).await
}

#[tauri::command]
async fn service_start() -> Result<(), String> {
    blocking(service::start).await
}

#[tauri::command]
async fn service_stop() -> Result<(), String> {
    blocking(service::stop).await
}

#[tauri::command]
async fn service_restart() -> Result<(), String> {
    blocking(service::restart).await
}

#[derive(Serialize)]
struct ConfigOut {
    path: String,
    /// サービス経由で読めたか（false ならファイルを直接読んだ）
    via_api: bool,
    config: Option<Value>,
}

/// 設定を読む。サービスが動いていれば API から、止まっていればファイルから。
#[tauri::command]
async fn read_config() -> Result<ConfigOut, String> {
    blocking(|| {
        let url = format!("{}/api/config", api_base_url());
        if let Ok(mut res) = agent(Duration::from_secs(3)).get(&url).call() {
            if res.status().is_success() {
                let v: Value = res.body_mut().read_json().map_err(|e| e.to_string())?;
                return Ok(ConfigOut {
                    path: v["path"].as_str().unwrap_or_default().into(),
                    via_api: true,
                    config: Some(v["config"].clone()),
                });
            }
        }
        let path = config_path();
        let config = match std::fs::read_to_string(&path) {
            Ok(s) => {
                let t: toml::Value = toml::from_str(&s).map_err(|e| format!("設定ファイルの形式が不正です: {e}"))?;
                Some(serde_json::to_value(t).map_err(|e| e.to_string())?)
            }
            Err(_) => None,
        };
        Ok(ConfigOut { path: path.to_string_lossy().into(), via_api: false, config })
    })
    .await
}

#[derive(Serialize)]
struct WriteConfigOut {
    restart_required: bool,
}

/// 設定を保存する。サービスが動いていれば API 経由、止まっていれば管理者権限で tsd.exe apply-config。
#[tauri::command]
async fn write_config(config: Value) -> Result<WriteConfigOut, String> {
    blocking(move || {
        let url = format!("{}/api/config", api_base_url());
        match agent(Duration::from_secs(10)).put(&url).send_json(&config) {
            Ok(mut res) => {
                let body: Value = res.body_mut().read_json().unwrap_or(Value::Null);
                if res.status().is_success() {
                    return Ok(WriteConfigOut { restart_required: body["restart_required"].as_bool().unwrap_or(true) });
                }
                Err(body["message"].as_str().unwrap_or("設定を保存できませんでした").to_string())
            }
            Err(_) => {
                let text = toml::to_string_pretty(&config).map_err(|e| format!("設定を TOML にできません: {e}"))?;
                let tmp = std::env::temp_dir().join(format!("tsd-config-{}.toml", std::process::id()));
                std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
                let exe = tsd_exe().ok_or("tsd コマンドが見つかりません")?;
                let tmp_s = tmp.to_string_lossy().into_owned();
                let cfg_s = config_path().to_string_lossy().into_owned();
                let r = service::run_privileged(
                    &exe.to_string_lossy(),
                    &["apply-config", "--from", &tmp_s, "--config", &cfg_s],
                );
                let _ = std::fs::remove_file(&tmp);
                r.map(|_| WriteConfigOut { restart_required: false })
            }
        }
    })
    .await
}

/// サービスのログ（journald）の末尾
#[tauri::command]
async fn read_log_tail(lines: Option<usize>) -> Result<String, String> {
    let lines = lines.unwrap_or(300).min(5000);
    blocking(move || service::log_tail(lines)).await
}

/// CSV ファイルをサービスへ送って取り込む（大きなファイルもメモリに載せない）
#[tauri::command]
async fn import_csv(path: String, params: std::collections::BTreeMap<String, String>) -> Result<Value, String> {
    blocking(move || {
        let mut f = std::fs::File::open(&path).map_err(|e| format!("開けません: {e}"))?;
        let mut req = agent(Duration::from_secs(6 * 3600)).post(format!("{}/api/import/csv", api_base_url()));
        for (k, v) in &params {
            req = req.query(k, v);
        }
        let mut res = req
            .content_type("text/csv")
            .send(ureq::SendBody::from_reader(&mut f))
            .map_err(|e| format!("サービスに接続できません: {e}"))?;
        let body: Value = res.body_mut().read_json().unwrap_or(Value::Null);
        if !res.status().is_success() {
            return Err(body["message"].as_str().unwrap_or("取り込みに失敗しました").to_string());
        }
        Ok(body)
    })
    .await
}

#[derive(Serialize)]
struct ExportOut {
    bytes: u64,
    rows: Option<u64>,
}

/// エクスポート結果をサービスから受け取り、指定ファイルへ書く（大きなデータもメモリに載せない）
#[tauri::command]
async fn export_to_file(request: Value, path: String) -> Result<ExportOut, String> {
    blocking(move || {
        let url = format!("{}/api/export", api_base_url());
        let mut res = agent(Duration::from_secs(6 * 3600)).post(&url).send_json(&request).map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            let body: Value = res.body_mut().read_json().unwrap_or(Value::Null);
            return Err(body["message"].as_str().unwrap_or("エクスポートに失敗しました").to_string());
        }
        let rows = res.headers().get("x-tsd-rows").and_then(|v| v.to_str().ok()).and_then(|s| s.parse().ok());
        let tmp = PathBuf::from(format!("{path}.part"));
        let mut out = std::fs::File::create(&tmp).map_err(|e| format!("保存先に書けません: {e}"))?;
        let mut reader = res.body_mut().with_config().limit(u64::MAX).reader();
        let bytes = std::io::copy(&mut reader, &mut out).map_err(|e| e.to_string())?;
        drop(out);
        std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
        Ok(ExportOut { bytes, rows })
    })
    .await
}

// ---- タスクトレイ -----------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Health {
    Ok,
    Warn,
    Down,
}

/// 丸いアイコンを描く（画像ファイルを持たずに3色を作る）
fn tray_icon(h: Health) -> Image<'static> {
    let (r, g, b) = match h {
        Health::Ok => (0x22, 0xA0, 0x5A),
        Health::Warn => (0xE8, 0x9B, 0x1A),
        Health::Down => (0xD0, 0x3B, 0x3B),
    };
    let n = 32u32;
    let mut px = vec![0u8; (n * n * 4) as usize];
    let c = (n as f32 - 1.0) / 2.0;
    for y in 0..n {
        for x in 0..n {
            let d = ((x as f32 - c).powi(2) + (y as f32 - c).powi(2)).sqrt();
            let i = ((y * n + x) * 4) as usize;
            // 外周はなめらかに、内側に白い折れ線（グラフ）を描く
            let alpha = (15.5 - d).clamp(0.0, 1.0);
            if alpha <= 0.0 {
                continue;
            }
            let fx = x as f32 / n as f32;
            let line_y = 0.62 - 0.22 * ((fx * 9.0).sin() * 0.5 + (fx - 0.2) * 0.8);
            let on_line = (y as f32 / n as f32 - line_y).abs() < 0.05 && (0.2..0.8).contains(&fx);
            let (pr, pg, pb) = if on_line { (255, 255, 255) } else { (r, g, b) };
            px[i] = pr;
            px[i + 1] = pg;
            px[i + 2] = pb;
            px[i + 3] = (alpha * 255.0) as u8;
        }
    }
    Image::new_owned(px, n, n)
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

fn check_health() -> (Health, String) {
    let svc = service::status();
    if svc.state != "unsupported" && svc.state != "running" {
        let s = if svc.installed { "サービス停止中" } else { "サービス未登録" };
        return (Health::Down, format!("TSD: {s}"));
    }
    let url = format!("{}/api/status", api_base_url());
    match agent(Duration::from_secs(3)).get(&url).call() {
        Ok(mut res) if res.status().is_success() => {
            let v: Value = res.body_mut().read_json().unwrap_or(Value::Null);
            let rate = v["ingest"]["points_per_sec"].as_f64().unwrap_or(0.0);
            let warnings = v["warnings"].as_array().map(|a| a.len()).unwrap_or(0);
            let stale = v["devices"]["stale"].as_u64().unwrap_or(0);
            if warnings > 0 {
                let first = v["warnings"][0].as_str().unwrap_or("");
                (Health::Warn, format!("TSD: 注意 {warnings} 件\n{first}"))
            } else {
                let _ = stale;
                (Health::Ok, format!("TSD: 正常（{rate:.0} 点/秒）"))
            }
        }
        _ => (Health::Down, "TSD: サービスに接続できません".into()),
    }
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "画面を開く", true, None::<&str>)?;
    let start = MenuItem::with_id(app, "start", "サービスを開始", true, None::<&str>)?;
    let stop = MenuItem::with_id(app, "stop", "サービスを停止", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "終了", true, None::<&str>)?;
    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&show, &sep1, &start, &stop, &sep2, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(tray_icon(Health::Down))
        .tooltip("TSD")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "show" => show_main(app),
            "start" => {
                std::thread::spawn(|| {
                    let _ = service::start();
                });
            }
            "stop" => {
                std::thread::spawn(|| {
                    let _ = service::stop();
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    // 5秒ごとに状態を確認してアイコンの色を変える
    let handle = app.handle().clone();
    let last: Mutex<Option<(Health, String)>> = Mutex::new(None);
    std::thread::Builder::new().name("tray-monitor".into()).spawn(move || loop {
        let now = check_health();
        let changed = last.lock().map(|l| l.as_ref() != Some(&now)).unwrap_or(true);
        if changed {
            if let Some(tray) = handle.tray_by_id("main") {
                if last.lock().map(|l| l.as_ref().map(|x| x.0) != Some(now.0)).unwrap_or(true) {
                    let _ = tray.set_icon(Some(tray_icon(now.0)));
                }
                let _ = tray.set_tooltip(Some(&now.1));
            }
            if let Ok(mut l) = last.lock() {
                *l = Some(now);
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    })?;
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .invoke_handler(tauri::generate_handler![
            app_info,
            api_base,
            service_status,
            service_start,
            service_stop,
            service_restart,
            read_config,
            write_config,
            read_log_tail,
            import_csv,
            export_to_file,
        ])
        .setup(|app| {
            setup_tray(app)?;
            // ログオン時の自動起動（--minimized）ではトレイに常駐するだけにする
            if !std::env::args().any(|a| a == "--minimized") {
                show_main(app.handle());
            }
            Ok(())
        })
        .on_window_event(|w, ev| {
            // 閉じてもトレイに残す（終了はトレイメニューから）
            if let WindowEvent::CloseRequested { api, .. } = ev {
                api.prevent_close();
                let _ = w.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("TSD アプリを起動できません");
}
