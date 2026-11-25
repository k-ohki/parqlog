//! アクセス制御。
//!
//! - ローカル以外（LAN）からのリクエストは、どのエンドポイントでもトークン必須。
//! - 取り込み専用ポートは常にトークン必須。
//! - ローカルからの書き込みは `auth.require_token_on_loopback` のときだけトークン必須。
//! - ブラウザからの要求は Tauri の画面以外を拒否する（CSRF 対策）。
//! - 設定・バックアップ・CSV 取り込みはローカルからのみ。

use crate::{SharedState, ALLOWED_ORIGINS};
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerKind {
    /// 通常の API ポート
    Api,
    /// LAN 向けの取り込み専用ポート
    Ingest,
}

/// ハンドラから参照するアクセス情報
#[derive(Debug, Clone, Copy)]
pub struct Access {
    pub local: bool,
    pub token_ok: bool,
}

pub fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "code": code, "message": message.into() }))).into_response()
}

fn token_from(headers: &HeaderMap, query: Option<&str>) -> Option<String> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some(t) = v.strip_prefix("Token ").or_else(|| v.strip_prefix("Bearer ")) {
            return Some(t.trim().to_string());
        }
        if let Some(b) = v.strip_prefix("Basic ") {
            // v1 互換: ユーザー名は任意、パスワードをトークンとして扱う
            if let Ok(d) = base64::engine::general_purpose::STANDARD.decode(b.trim()) {
                if let Ok(s) = String::from_utf8(d) {
                    if let Some((_, p)) = s.split_once(':') {
                        return Some(p.to_string());
                    }
                }
            }
        }
    }
    // v1 互換: ?u=..&p=<token>
    query.and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == "p").then(|| v.to_string())
        })
    })
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn guard(
    State(st): State<SharedState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::Extension(kind): axum::Extension<ListenerKind>,
    mut req: Request,
    next: Next,
) -> Response {
    // ブラウザからの要求は Tauri の画面だけ受け付ける（CSRF 対策。Grafana などのサーバー間通信には Origin が付かない）
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        if origin != format!("http://{host}") && !ALLOWED_ORIGINS.contains(&origin) {
            return error(StatusCode::FORBIDDEN, "forbidden", format!("このオリジンからのアクセスは許可されていません: {origin}"));
        }
    }
    if req.method() == axum::http::Method::OPTIONS {
        return next.run(req).await;
    }

    let local = peer.ip().is_loopback() || peer.ip().to_canonical().is_loopback();
    let token_ok = match token_from(req.headers(), req.uri().query()) {
        Some(t) => st.cfg.auth.tokens.iter().any(|k| constant_eq(k.token.as_bytes(), t.as_bytes())),
        None => false,
    };

    let path = req.uri().path().to_string();
    let is_probe = path == "/ping" || path == "/health";
    let need_token = !is_probe && (kind == ListenerKind::Ingest || !local);
    if need_token && !token_ok {
        return error(StatusCode::UNAUTHORIZED, "unauthorized", "トークンが無いか正しくありません");
    }
    req.extensions_mut().insert(Access { local: local && kind == ListenerKind::Api, token_ok });
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Token abc".parse().unwrap());
        assert_eq!(token_from(&h, None).as_deref(), Some("abc"));
        h.insert(header::AUTHORIZATION, "Basic dXNlcjpzZWNyZXQ=".parse().unwrap());
        assert_eq!(token_from(&h, None).as_deref(), Some("secret"));
        assert_eq!(token_from(&HeaderMap::new(), Some("db=x&u=a&p=zz")).as_deref(), Some("zz"));
        assert!(constant_eq(b"abc", b"abc"));
        assert!(!constant_eq(b"abc", b"abd"));
    }
}
