//! systemd のサービス（tsd.service）の状態取得と操作。
//! 開始・停止は pkexec（polkit の認証ダイアログ）で systemctl を実行する。

use serde::Serialize;
use std::process::Command;

pub const UNIT: &str = "tsd.service";

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ServiceInfo {
    pub installed: bool,
    /// running / stopped / start_pending / stop_pending / failed / other / unsupported
    pub state: String,
    pub pid: Option<u32>,
}

fn systemctl_show() -> Option<std::collections::HashMap<String, String>> {
    let out = Command::new("systemctl")
        .args(["show", UNIT, "--property=LoadState,ActiveState,SubState,MainPID", "--no-pager"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect(),
    )
}

pub fn status() -> ServiceInfo {
    let Some(m) = systemctl_show() else {
        return ServiceInfo { installed: false, state: "unsupported".into(), pid: None };
    };
    let installed = m.get("LoadState").map(|s| s == "loaded").unwrap_or(false);
    let state = match m.get("ActiveState").map(String::as_str).unwrap_or("") {
        "active" => "running",
        "inactive" => "stopped",
        "activating" => "start_pending",
        "deactivating" => "stop_pending",
        "failed" => "failed",
        _ => "other",
    };
    let pid = m.get("MainPID").and_then(|p| p.parse().ok()).filter(|p: &u32| *p > 0);
    ServiceInfo { installed, state: state.into(), pid }
}

/// 管理者権限で実行する（polkit の認証ダイアログが出る）
pub fn run_privileged(program: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new("pkexec")
        .arg(program)
        .args(args)
        .output()
        .map_err(|e| format!("pkexec を実行できません: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        Some(126) => Err("認証がキャンセルされました".into()),
        Some(127) => Err("認証できませんでした".into()),
        _ => Err(format!("{program} {} が失敗しました: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim())),
    }
}

fn systemctl(action: &str) -> Result<(), String> {
    run_privileged("systemctl", &[action, UNIT])
}

pub fn start() -> Result<(), String> {
    systemctl("start")
}

pub fn stop() -> Result<(), String> {
    systemctl("stop")
}

pub fn restart() -> Result<(), String> {
    systemctl("restart")
}

/// サービスのログ（journald）の末尾
pub fn log_tail(lines: usize) -> Result<String, String> {
    let out = Command::new("journalctl")
        .args(["-u", UNIT, "-n", &lines.to_string(), "--no-pager", "-o", "short-iso"])
        .output()
        .map_err(|e| format!("journalctl を実行できません: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.trim().is_empty() {
        return Ok("（ログがありません。読めない場合はユーザーを systemd-journal グループに追加してください）".into());
    }
    Ok(text)
}
