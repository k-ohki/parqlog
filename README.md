# tsd — 小規模な時系列データサービス（Ubuntu）

装置からのデータを受け取って保存し、Grafana 9 から見られるようにするサービスです。InfluxDB2 の代わりに使う想定で、DuckDB を組み込んだ単一バイナリで動きます。

- 取り込み: InfluxDB 3 / v2 / v1 互換の書き込み API（Telegraf の `outputs.influxdb_v3` / `outputs.influxdb_v2` をそのまま向けられる）と CSV
- 保存: 直近は DuckDB、確定した日は日付単位の Parquet。値は 32bit、1年より古い日は 16bit に再圧縮
- 集計: 1分・1時間のロールアップ。表示幅に合わせて層（生 / 1分 / 1時間）を自動で選ぶ
- 可視化: Grafana 9 の標準 Prometheus データソースで読める（Prometheus 互換 API）
- 管理: Tauri の管理・簡易確認アプリ（任意）

## 構成

```
crates/tsd-core/    ストレージ、Line Protocol、集計・保持期間、CSV 取り込み
crates/tsd-server/  HTTP API（取り込み、クエリ、Prometheus 互換、管理）
crates/tsd/         tsd コマンド（run / import / series / backup / check-config）
tools/gen/          合成データ生成・性能計測
app/                Tauri 2 の管理・簡易確認アプリ（Svelte + uPlot）
deploy/             systemd ユニット、インストールスクリプト、Grafana の設定例
```

## インストール（Ubuntu）

```sh
sudo apt install build-essential pkg-config     # DuckDB の C++ ビルド用
cargo build --release -p tsd                     # 初回は 15〜30 分
sudo sh deploy/install.sh                        # /usr/local/bin/tsd、/etc/tsd/tsd.toml、/var/lib/tsd、systemd 登録
```

ログは `journalctl -u tsd -f`。詳しく出すときは `systemctl edit tsd` で `Environment=TSD_LOG=debug`。

## データを入れる

### Telegraf

```toml
[[outputs.influxdb_v2]]
  urls = ["http://127.0.0.1:8086"]
  token = ""          # ローカルからは不要
  organization = "tsd"
  bucket = "tsd"
```

`outputs.influxdb_v3`（`/api/v3/write_lp`、時刻の精度は自動判定）も使えます。`database` は無視して1つの DB に保存します。

他の PC から送るときは `/etc/tsd/tsd.toml` で `http.ingest_listen = "0.0.0.0:8087"` とトークンを設定し、ufw でポートを開けます。

### CSV

```sh
# wide 形式: 1列目が時刻、残りの列がそれぞれ系列になる（列名 = 項目名）
tsd import 炉1_2026-11.csv --measurement furnace --tag device=炉1

# long 形式: 時刻,系列キー,値（tsd のエクスポートと同じ形）
tsd import export.csv --layout long
```

- 時刻は `2026/11/25 14:00:00`、`2026-11-25T14:00:00+09:00`、エポック秒・ミリ秒のどれでも読めます。タイムゾーンの無い日時はこの PC の地方時（`--utc-offset +09:00` で指定可）。列名が `_utc` で終わる列は UTC として扱います。
- 過去の日にも取り込めます。確定済みの日次ファイルと 1分・1時間集計を作り直します。同じ系列・時刻は上書きなので、同じファイルを何度取り込んでも結果は同じです。
- 数値でないセル（空欄、`ERR` など）は飛ばし、数値が1つも無い列は系列にしません。
- サービス稼働中は API（`POST /api/import/csv`、ローカルのみ）経由、停止中はデータフォルダを直接開いて取り込みます。管理アプリの「CSV」画面からも取り込めます。

## 系列の名前変更・削除

```sh
tsd series list 炉温                                   # ID と系列キーの一覧（部分一致）
tsd series rename 12 'furnace,device=炉1 炉温'          # キーを変える（データはそのまま）
tsd series delete 13 14                                # 削除（確認あり。--yes で省略）
```

- 名前変更は ID を変えずにキーだけを書き換えるので、過去データもそのまま引き継ぎます。送信側が古い名前で送り続けると古い名前の系列が新しく作られるので、送信側も直してください。
- 既にあるキーへは変えられません。2つの系列を1つにまとめたいときは、統合元を long 形式でエクスポートし、CSV の系列キーを書き換えて取り込んでから、統合元を削除します。
- 削除すると一覧・クエリ・Grafana から消えます。直近の DB 上のデータはすぐ消え、確定済みの Parquet に残る行は保持期間が来たときに消えます。削除した ID は使い回しません。
- 管理アプリの系列ブラウザからも行えます。API は `PUT /api/series/{id}`（`{"key": ...}`）と `DELETE /api/series/{id}`（どちらもローカルのみ）。

## Grafana 9 で見る

`deploy/grafana/datasource.yaml` を `/etc/grafana/provisioning/datasources/` に置くか、手動で Prometheus データソースを追加して URL を `http://127.0.0.1:8086` にします（HTTP method は POST、Scrape interval は 1s）。

系列は次のラベルで表されます。

| ラベル | 内容 |
| --- | --- |
| `__name__` | field（項目名）。例 `炉温` |
| `_measurement` | measurement |
| その他 | タグ（`device` など） |

クエリ例:

```promql
炉温{device="炉1"}                                     # ステップ幅ごとの平均
{__name__=~"f0[1-3]", device=~"$device"}               # 変数を使った複数系列
max_over_time(炉温{device="炉1"}[$__interval])          # ステップ幅ごとの最大
min_over_time(炉温{device="炉1"}[$__interval])          # 最小
```

変数は `label_values(device)`、`label_values({_measurement="furnace"}, __name__)` のように作れます。凡例は `{{device}} {{__name__}}`。

対応しているのはセレクタと `avg_over_time` / `min_over_time` / `max_over_time` だけです（`rate`、`sum by` などは使えません）。`[...]` の幅は無視し、Grafana のステップ幅でバケットに集計します。関数を通しても `__name__` は残します。Prometheus と違い、セレクタだけのときは直前の値ではなくステップ幅の平均を返します。Stat パネルなど瞬時クエリでは、5分以内の最新値を返します。

## HTTP API

| メソッド | パス | 内容 |
| --- | --- | --- |
| POST | `/api/v3/write_lp`、`/api/v2/write`、`/write` | InfluxDB 3 / v2 / v1 互換の書き込み（gzip 可）。v3 は `precision=auto` と `accept_partial` に対応 |
| GET/POST | `/api/v1/query_range`、`/api/v1/query`、`/api/v1/labels`、`/api/v1/label/{name}/values`、`/api/v1/series` | Prometheus 互換（Grafana 用） |
| GET | `/api/query?series=1,2&from=now-1d&to=now&width=1600` | 系列ごとの `{t, min, max, avg}`（自作ビューアー向け） |
| GET | `/api/series`、`/api/latest`、`/api/devices`、`/api/status` | 系列一覧、最新値、装置ごとの受信状況、稼働状況 |
| PUT / DELETE | `/api/series/{id}` | 単位・系列キーの変更（`{"unit": ..}` / `{"key": ..}`）、削除。キー変更と削除はローカルのみ |
| POST | `/api/import/csv?layout=wide&measurement=..&tag.device=..` | CSV 取り込み（ローカルのみ） |
| POST | `/api/export` | CSV / Parquet で書き出し |
| POST | `/api/backup` | `{dest}` にバックアップ（ローカルのみ） |
| GET/PUT | `/api/config` | 設定の取得・保存（ローカルのみ、再起動で反映） |

ローカル（127.0.0.1）以外からはトークン必須です。ブラウザからの要求は管理アプリ以外のオリジンを拒否します。

## 管理・簡易確認アプリ（任意）

`app/` は Tauri 2 のデスクトップアプリです。サービスの状態とログ、開始・停止（polkit の認証）、設定の編集、装置ごとの受信状況、簡易グラフ、CSV の取り込み・書き出しができます。ダッシュボードはアプリ側に保存します。

```sh
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev
cd app/ui && npm ci && cd ../src-tauri && npx --prefix ../ui tauri build   # .deb ができる
```

## InfluxDB 3 との関係

本体は InfluxDB 3 と同じく Rust で、考え方も近い構成です（Line Protocol で受けて直近をメモリと DB に持ち、確定分を日単位の Parquet にする）。違いは次のとおりです。

| | InfluxDB 3 Core | tsd |
| --- | --- | --- |
| クエリエンジン | DataFusion（SQL / InfluxQL） | DuckDB（組み込み） |
| 長期間の集計 | 圧縮・集計は Enterprise の機能 | 1分・1時間のロールアップを持ち、5年分でも1秒以内に表示 |
| 保存先 | オブジェクトストレージ / ローカル | ローカルのみ |
| Grafana | InfluxDB データソース（SQL / InfluxQL） | Prometheus データソース |

## 保存の仕組み

| 層 | 置き場所 | 確定のタイミング |
| --- | --- | --- |
| 生データ（直近） | DuckDB `raw_hot` | 猶予期間（既定1日）が過ぎた日を Parquet へ |
| 生データ（確定） | `raw/YYYY/MM/YYYY-MM-DD.parquet` | |
| 生データ（1年超） | `raw/YYYY/MM/YYYY-MM-DD.q16.parquet` | 16bit に再圧縮 |
| 1分集計 | DuckDB `r1m_hot` → `1m/YYYY-MM.parquet` | 月が確定したら Parquet へ |
| 1時間集計 | DuckDB `r1h` | |

16bit は系列ごとにその日の最小〜最大を 65,536 段階で表します（`value = q_off + q × q_scale`）。誤差はその日の値幅 ÷ 131,070 以下です。

## 性能（合成データ、2 vCPU）

| 項目 | 結果 |
| --- | --- |
| 1年・10系列の表示 | 19 ms |
| 1日・生データ・10系列 | 32 ms |
| 取り込み（HTTP） | 54 万点/秒 |
| 容量（100系列・5年） | 約 13 GB |

詳細は `docs/benchmark.md`。

## テスト

```sh
cargo test --workspace
```
