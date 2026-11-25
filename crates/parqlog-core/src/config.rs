//! 設定ファイル（TOML）。

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub http: HttpConfig,
    pub auth: AuthConfig,
    pub storage: StorageConfig,
    pub retention: RetentionConfig,
    pub monitor: MonitorConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    /// クエリ API と取り込み API。既定はローカルのみ。
    pub listen: String,
    /// LAN 向けの取り込み専用ポート（例 "0.0.0.0:8087"）。空なら無効。トークン必須。
    pub ingest_listen: String,
    /// 1リクエストの最大サイズ（MB、展開後）
    pub max_body_mb: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Token {
    pub name: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub tokens: Vec<Token>,
    /// ローカル（127.0.0.1）からの書き込みにもトークンを求める
    pub require_token_on_loopback: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct StorageConfig {
    pub data_dir: String,
    /// DuckDB の memory_limit（例 "1GB"）
    pub memory_limit: String,
    /// DuckDB のスレッド数。0 で自動。
    pub threads: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub raw_days: u32,
    pub m1_days: u32,
    pub h1_days: u32,
    /// 遅れデータを受け付ける猶予（日）
    pub grace_days: u32,
    /// この日数より古い生データを 16bit に再圧縮する。0 で無効。
    pub compact_after_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MonitorConfig {
    /// 装置を表すタグ名（無い系列は host タグ、さらに無ければ measurement で代用）
    pub device_tag: String,
    /// この秒数受信が無ければ欠測とみなす
    pub stale_seconds: u32,
    /// ディスク残量がこれを下回ったら警告（GB）
    pub disk_warn_gb: u32,
}

/// 設定ファイルの既定の場所
pub fn default_config_path() -> PathBuf {
    PathBuf::from("/etc/parqlog/parqlog.toml")
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8086".into(),
            ingest_listen: String::new(),
            max_body_mb: 64,
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self { tokens: vec![], require_token_on_loopback: false }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            data_dir: "/var/lib/parqlog".into(),
            memory_limit: "1GB".into(),
            threads: 0,
        }
    }
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self { raw_days: 1826, m1_days: 1826, h1_days: 1826, grace_days: 1, compact_after_days: 365 }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self { device_tag: "device".into(), stale_seconds: 30, disk_warn_gb: 10 }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            http: HttpConfig::default(),
            auth: AuthConfig::default(),
            storage: StorageConfig::default(),
            retention: RetentionConfig::default(),
            monitor: MonitorConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("設定ファイルを読めません: {}", path.display()))?;
        Self::from_toml(&text).with_context(|| format!("設定ファイルの形式が不正です: {}", path.display()))
    }

    /// ファイルが無ければ既定値。
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// 一時ファイルに書いてからリネームする。
    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, self.to_toml()?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.http
            .listen
            .parse::<SocketAddr>()
            .with_context(|| format!("http.listen が不正です: {}", self.http.listen))?;
        if !self.http.ingest_listen.is_empty() {
            self.http
                .ingest_listen
                .parse::<SocketAddr>()
                .with_context(|| format!("http.ingest_listen が不正です: {}", self.http.ingest_listen))?;
            if self.auth.tokens.is_empty() {
                bail!("http.ingest_listen を使うには auth.tokens にトークンが1つ以上必要です");
            }
        }
        if self.storage.data_dir.trim().is_empty() {
            bail!("storage.data_dir が空です");
        }
        let r = &self.retention;
        if r.raw_days == 0 || r.m1_days == 0 || r.h1_days == 0 {
            bail!("retention の日数は1以上にしてください");
        }
        if r.grace_days > 30 {
            bail!("retention.grace_days は 30 以下にしてください");
        }
        if r.compact_after_days != 0 && r.compact_after_days <= r.grace_days + 1 {
            bail!("retention.compact_after_days は grace_days + 1 より大きくしてください");
        }
        for t in &self.auth.tokens {
            if t.token.len() < 16 {
                bail!("トークン '{}' が短すぎます（16文字以上）", t.name);
            }
        }
        Ok(())
    }

    pub fn data_dir(&self) -> PathBuf {
        PathBuf::from(&self.storage.data_dir)
    }

    pub fn store_options(&self) -> crate::duck::StoreOptions {
        crate::duck::StoreOptions {
            data_dir: self.data_dir(),
            memory_limit: self.storage.memory_limit.clone(),
            threads: self.storage.threads,
            grace_days: self.retention.grace_days as i64,
            raw_days: self.retention.raw_days as i64,
            m1_days: self.retention.m1_days as i64,
            h1_days: self.retention.h1_days as i64,
            compact_after_days: self.retention.compact_after_days as i64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_default() {
        let c = Config::default();
        let t = c.to_toml().unwrap();
        assert_eq!(Config::from_toml(&t).unwrap(), c);
    }

    #[test]
    fn partial_file_uses_defaults() {
        let c = Config::from_toml("[http]\nlisten = \"127.0.0.1:9000\"\n").unwrap();
        assert_eq!(c.http.listen, "127.0.0.1:9000");
        assert_eq!(c.retention.raw_days, 1826);
    }

    #[test]
    fn lan_ingest_requires_token() {
        let mut c = Config::default();
        c.http.ingest_listen = "0.0.0.0:8087".into();
        assert!(c.validate().is_err());
        c.auth.tokens.push(Token { name: "t".into(), token: "0123456789abcdef".into() });
        assert!(c.validate().is_ok());
    }
}
