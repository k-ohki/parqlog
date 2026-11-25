//! HTTP API の結合テスト（ソケットを開かずにルーターを直接呼ぶ）

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{header, Request, StatusCode};
use http_body_util::BodyExt;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;
use tsd_core::{Config, DuckStore, Store, StoreOptions};
use tsd_server::{build_state, router, ListenerKind, SharedState};

struct Env {
    _dir: tempfile::TempDir,
    st: SharedState,
}

fn env(tokens: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::default();
    cfg.storage.data_dir = dir.path().join("data").to_string_lossy().into();
    if tokens {
        cfg.auth.tokens.push(tsd_core::config::Token { name: "t".into(), token: "0123456789abcdef-token".into() });
    }
    let mut o = StoreOptions::for_dir(cfg.data_dir());
    o.memory_limit = "256MB".into();
    let store: Arc<dyn Store> = Arc::new(DuckStore::open(o).unwrap());
    let st = build_state(cfg.clone(), dir.path().join("tsd.toml"), store, cfg.data_dir()).unwrap();
    Env { _dir: dir, st }
}

async fn call(st: &SharedState, kind: ListenerKind, peer: &str, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let peer: SocketAddr = peer.parse().unwrap();
    let app = router(st.clone(), kind).layer(MockConnectInfo(peer));
    let res = app.oneshot(req).await.unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, headers, body)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn json(method: &str, uri: &str, v: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn write_req(body: impl Into<Body>, gzip: bool) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri("/api/v2/write?org=x&bucket=y&precision=s");
    if gzip {
        b = b.header(header::CONTENT_ENCODING, "gzip");
    }
    b.body(body.into()).unwrap()
}

const LOCAL: &str = "127.0.0.1:50000";
const LAN: &str = "192.168.1.20:50000";

#[tokio::test]
async fn write_query_and_metadata() {
    let e = env(false);
    let now = tsd_core::time::now_ms() / 1000;
    let mut body = String::new();
    for i in 0..60 {
        body.push_str(&format!("plc,device=a temp={},hum={}i {}\n", 20 + i % 3, 40 + i % 5, now - 60 + i));
        body.push_str(&format!("plc,device=b temp={} {}\n", 30.5, now - 60 + i));
    }
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, write_req(body, false)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // gzip でも受け付ける
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(format!("plc,device=c temp=1 {now}\n").as_bytes()).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, write_req(gz.finish().unwrap(), true)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    e.st.ingest.flush_now();

    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/series?q=temp")).await;
    assert_eq!(s, StatusCode::OK);
    let series: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(series.as_array().unwrap().len(), 3);
    assert_eq!(series[0]["tags"]["device"], "a");
    assert!(series[0]["last_value"].is_number());

    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/series?tag.device=a")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&b).unwrap().as_array().unwrap().len(), 2);

    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/query?series=1,3&from=now-5m&to=now&width=300")).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let q: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(q["tier"], "raw");
    let n: usize = q["series"][0]["avg"].as_array().unwrap().iter().filter(|v| v.is_number()).count();
    assert_eq!(n, 60);
    assert_eq!(q["series"][1]["avg"][0], 30.5);

    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/latest?series=3")).await;
    assert_eq!(s, StatusCode::OK);
    let l: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(l[0]["value"], 30.5);

    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/devices")).await;
    let d: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(d.as_array().unwrap().len(), 3);
    assert_eq!(d[0]["device"], "a");
    assert_eq!(d[0]["series_count"], 2);

    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/status")).await;
    assert_eq!(s, StatusCode::OK);
    let st: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(st["store"]["series_count"], 4);
    assert_eq!(st["ingest"]["written_points"], 181);

    // 単位の設定
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, json("PUT", "/api/series/1", serde_json::json!({"unit": "°C"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/series?ids=1")).await;
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&b).unwrap()[0]["unit"], "°C");

    // エクスポート（CSV）
    let (s, h, b) = call(
        &e.st,
        ListenerKind::Api,
        LOCAL,
        json("POST", "/api/export", serde_json::json!({"series": [1], "from": "now-10m", "to": "now", "tier": "raw", "format": "csv"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(h.get("x-tsd-rows").unwrap(), "60");
    let text = String::from_utf8(b).unwrap();
    assert!(text.starts_with("time_utc,series,value"));
    assert_eq!(text.lines().count(), 61);
}

#[tokio::test]
async fn bad_lines_give_400_with_line_number_but_keep_good_lines() {
    let e = env(false);
    let now = tsd_core::time::now_ms() / 1000;
    let body = format!("m v=1 {now}\nm v= {now}\nm v=3 {}\n", now + 1);
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, write_req(body, false)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["line"], 2);
    assert_eq!(v["code"], "invalid");
    e.st.ingest.flush_now();
    assert_eq!(e.st.store.status().unwrap().raw_hot_rows, 2);
}

#[tokio::test]
async fn v3_write_lp_auto_precision_and_accept_partial() {
    let e = env(false);
    let now_s = tsd_core::time::now_ms() / 1000;
    // 秒・ミリ秒・ナノ秒が混ざっていても precision=auto で読める
    let body = format!("m,device=x v=1 {now_s}\nm,device=x v=2 {}\nm,device=x v=3 {}\n", (now_s + 1) * 1000, (now_s + 2) * 1_000_000_000);
    let req = Request::builder().method("POST").uri("/api/v3/write_lp?db=sensors").body(Body::from(body)).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, req).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // accept_partial=false なら誤りが1行でもあれば何も書かない
    let req = Request::builder()
        .method("POST")
        .uri("/api/v3/write_lp?db=sensors&accept_partial=false")
        .body(Body::from(format!("m,device=x v=9 {}\nbad\n", now_s + 3)))
        .unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, req).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    e.st.ingest.flush_now();
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get(&format!("/api/query?series=1&from={}&to={}&width=100&tier=raw", now_s * 1000, (now_s + 10) * 1000))).await;
    let q: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(q["series"][0]["avg"], serde_json::json!([1.0, 2.0, 3.0]));
}

#[tokio::test]
async fn series_rename_and_delete() {
    let e = env(true);
    let now_s = tsd_core::time::now_ms() / 1000;
    let body = format!("plc,device=dev1 temp=1 {now_s}\nplc,device=dev02 temp=2 {now_s}\n");
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, write_req(body, false)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    e.st.ingest.flush_now();
    let j = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();

    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, json("PUT", "/api/series/1", serde_json::json!({"key": "plc,device=dev01 temp"}))).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(j(&b)["tags"]["device"], "dev01");
    // 単位だけの変更では名前は変わらない
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, json("PUT", "/api/series/1", serde_json::json!({"unit": "°C"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // 既にあるキーへは変えられない
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, json("PUT", "/api/series/1", serde_json::json!({"key": "plc,device=dev02 temp"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(j(&b)["message"].as_str().unwrap().contains("既に使われています"));
    // LAN からはトークンがあっても不可
    let req = Request::builder()
        .method("DELETE")
        .uri("/api/series/2")
        .header(header::AUTHORIZATION, "Token 0123456789abcdef-token")
        .body(Body::empty())
        .unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LAN, req).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let del = || Request::builder().method("DELETE").uri("/api/series/2").body(Body::empty()).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, del()).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, del()).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/series")).await;
    let list = j(&b);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["key"], "plc,device=dev01 temp");
    assert_eq!(list[0]["unit"], "°C");
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/v1/label/device/values")).await;
    assert_eq!(j(&b)["data"], serde_json::json!(["dev01"]));
}

#[tokio::test]
async fn csv_import_via_api() {
    let e = env(true);
    let t = (tsd_core::time::now_ms() / 60_000 - 10) * 60_000;
    let mut csv = String::from("time_utc,温度,圧力\n");
    for i in 0..30 {
        csv.push_str(&format!("{},{},101.3\n", t + i * 1000, 20 + i));
    }
    let uri = "/api/import/csv?layout=wide&measurement=line&tag.device=A&utc_offset=%2B09:00";
    let req = Request::builder().method("POST").uri(uri).body(Body::from(csv.clone())).unwrap();
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, req).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let r: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(r["points"], 60);
    assert_eq!(r["series"], 2);
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/series?tag.device=A")).await;
    let list: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(list[0]["measurement"], "line");
    assert_eq!(list[0]["field"], "温度");
    // LAN からはトークンがあっても不可
    let req = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::AUTHORIZATION, "Token 0123456789abcdef-token")
        .body(Body::from(csv))
        .unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LAN, req).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

fn form(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn prometheus_api_for_grafana() {
    let e = env(false);
    let now_s = tsd_core::time::now_ms() / 1000;
    let mut body = String::new();
    for i in 0..120 {
        let ts = now_s - 120 + i;
        body.push_str(&format!("plc,device=dev01 temp={},pres=101 {ts}\n", i % 10));
        body.push_str(&format!("plc,device=dev02 temp=50 {ts}\n"));
    }
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, write_req(body, false)).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    e.st.ingest.flush_now();
    let j = |b: &[u8]| serde_json::from_slice::<serde_json::Value>(b).unwrap();

    // 接続テスト（Grafana は 1+1 を投げる）
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, form("/api/v1/query", &format!("query=1%2B1&time={now_s}"))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(j(&b)["data"]["resultType"], "scalar");
    assert_eq!(j(&b)["data"]["result"][1], "2");

    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/v1/labels")).await;
    assert_eq!(j(&b)["data"], serde_json::json!(["__name__", "_measurement", "device"]));
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/v1/label/__name__/values")).await;
    assert_eq!(j(&b)["data"], serde_json::json!(["pres", "temp"]));
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/v1/label/device/values?match%5B%5D=pres")).await;
    assert_eq!(j(&b)["data"], serde_json::json!(["dev01"]));
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/v1/series?match%5B%5D=temp")).await;
    assert_eq!(j(&b)["data"].as_array().unwrap().len(), 2);

    // 範囲クエリ（Grafana 9 は POST のフォーム）。ステップ 10 秒の平均
    let q = format!(
        "query={}&start={}&end={}&step=10",
        urlencode(r#"temp{device=~"dev0(1|2)"}"#),
        now_s - 120,
        now_s - 1
    );
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, form("/api/v1/query_range", &q)).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    let d = j(&b);
    assert_eq!(d["data"]["resultType"], "matrix");
    let res = d["data"]["result"].as_array().unwrap();
    assert_eq!(res.len(), 2);
    assert_eq!(res[0]["metric"]["device"], "dev01");
    assert_eq!(res[0]["metric"]["__name__"], "temp");
    let vals = res[0]["values"].as_array().unwrap();
    assert!(vals.len() >= 11 && vals.len() <= 13, "{}", vals.len());
    assert!(vals[0][0].is_f64() || vals[0][0].is_i64());
    assert!(vals[0][1].is_string());
    assert_eq!(res[1]["values"][0][1], "50");

    // 最大値
    let q = format!("query={}&start={}&end={}&step=60", urlencode("max_over_time(temp{device=\"dev01\"}[1m])"), now_s - 120, now_s);
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, form("/api/v1/query_range", &q)).await;
    let d = j(&b);
    // 途中の1分は完全に埋まっているので最大は 9（両端のバケットは一部だけ）
    let vals = d["data"]["result"][0]["values"].as_array().unwrap();
    assert!(vals.iter().any(|v| v[1] == "9"), "{vals:?}");
    assert!(vals.iter().all(|v| v[0].as_f64().unwrap() as i64 % 60 == 0));

    // 瞬時クエリ: 最新値
    let (_, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get(&format!("/api/v1/query?query=pres&time={now_s}"))).await;
    let d = j(&b);
    assert_eq!(d["data"]["resultType"], "vector");
    assert_eq!(d["data"]["result"][0]["value"][1], "101");

    // 対応していない式はエラー
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get(&format!("/api/v1/query?query={}", urlencode("rate(temp[5m])")))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(j(&b)["status"], "error");
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[tokio::test]
async fn access_control() {
    let e = env(true);
    // 許可していないオリジン（ブラウザ経由の CSRF）は拒否
    let req = Request::builder().uri("/api/status").header(header::ORIGIN, "https://evil.example").body(Body::empty()).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, req).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // Tauri の画面は許可し、CORS ヘッダーを返す
    let req = Request::builder().uri("/api/status").header(header::ORIGIN, "http://tauri.localhost").body(Body::empty()).unwrap();
    let (s, h, _) = call(&e.st, ListenerKind::Api, LOCAL, req).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN).unwrap(), "http://tauri.localhost");

    // LAN からはトークン必須
    let (s, _, _) = call(&e.st, ListenerKind::Api, LAN, get("/api/status")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let req = Request::builder().uri("/api/status").header(header::AUTHORIZATION, "Token 0123456789abcdef-token").body(Body::empty()).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LAN, req).await;
    assert_eq!(s, StatusCode::OK);
    // 設定は LAN からはトークンがあっても不可
    let req = Request::builder().uri("/api/config").header(header::AUTHORIZATION, "Token 0123456789abcdef-token").body(Body::empty()).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LAN, req).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // 取り込み専用ポート: ローカルからでもトークン必須、クエリは存在しない
    let now = tsd_core::time::now_ms() / 1000;
    let (s, _, _) = call(&e.st, ListenerKind::Ingest, LOCAL, write_req(format!("m v=1 {now}"), false)).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let req = Request::builder()
        .method("POST")
        .uri("/api/v2/write?precision=s")
        .header(header::AUTHORIZATION, "Token 0123456789abcdef-token")
        .body(Body::from(format!("m v=1 {now}")))
        .unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Ingest, LAN, req).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // v1 互換（?p=トークン）
    let req = Request::builder()
        .method("POST")
        .uri("/write?db=x&u=a&p=0123456789abcdef-token&precision=s")
        .body(Body::from(format!("m v=2 {now}")))
        .unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Ingest, LAN, req).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let req = Request::builder().uri("/api/status").header(header::AUTHORIZATION, "Token 0123456789abcdef-token").body(Body::empty()).unwrap();
    let (s, _, _) = call(&e.st, ListenerKind::Ingest, LAN, req).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // ping はトークン不要
    let (s, _, _) = call(&e.st, ListenerKind::Ingest, LAN, get("/ping")).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn config_roundtrip() {
    let e = env(false);
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, get("/api/config")).await;
    assert_eq!(s, StatusCode::OK);
    let mut v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let mut cfg = v["config"].take();
    cfg["monitor"]["stale_seconds"] = 45.into();
    let (s, _, b) = call(&e.st, ListenerKind::Api, LOCAL, json("PUT", "/api/config", cfg.clone())).await;
    assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&b));
    assert_eq!(serde_json::from_slice::<serde_json::Value>(&b).unwrap()["restart_required"], true);
    let saved = Config::load(&e.st.config_path).unwrap();
    assert_eq!(saved.monitor.stale_seconds, 45);
    // 不正な設定は保存しない
    cfg["http"]["listen"] = "not-an-address".into();
    let (s, _, _) = call(&e.st, ListenerKind::Api, LOCAL, json("PUT", "/api/config", cfg)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
