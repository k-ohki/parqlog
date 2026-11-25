# WSL2 で試すときの注意点

parqlog 本体は普通の Ubuntu と同じく動く見込みです（WSL2 上での動作確認はまだしていません）。以下は WSL2 特有の注意点です。

## 事前の確認・設定

### systemd を有効にする

```sh
systemctl is-system-running    # "running" か "degraded" なら有効
```

無効なら `/etc/wsl.conf` に次を書き、PowerShell で `wsl --shutdown` してから入り直します。

```ini
[boot]
systemd=true
```

最近の `wsl --install` で入れた Ubuntu は、最初から有効のことが多いです。

### ソースとデータは Linux 側に置く

- ソースは `~/parqlog`、データは `/var/lib/parqlog` など Linux 側に置きます。
- `/mnt/c/...`（Windows 側のディスク）は避けます。ファイル操作がかなり遅く、ビルドが極端に遅くなり、DuckDB やファイルの置き換えの動きも怪しくなります。
- CSV を `/mnt/c` から読むだけなら問題ありません。

### ビルドに使うメモリ

DuckDB の C++ のビルドに数 GB 使います。WSL2 に割り当てたメモリが少ないと失敗するので、Windows 側の `%UserProfile%\.wslconfig` を確認します。

```ini
[wsl2]
memory=8GB
```

## 長時間の収集テストで起きうること

### WSL が勝手に止まる

端末を全部閉じると、しばらくして WSL ごと止まることがあります。その間のデータは受け取れません。テスト中は端末を1つ開いたままにしておくのが確実です。

### 時計のずれ

Windows がスリープから復帰すると、WSL2 の時計がずれることがあります。

- 時刻を付けずに送られたデータには受信時刻が付くため、記録がずれます。
- parqlog は1日以上先の未来の時刻や、猶予期間（既定1日）より古い時刻のデータを捨てるので、ずれが大きいと取り込まれないこともあります。

おかしいと思ったら確認して合わせます。

```sh
date
sudo hwclock -s
```

## ネットワーク

### Windows 側から見る

Grafana を Windows で動かす場合、`http://localhost:8086` で WSL2 の parqlog に届くはずです（WSL2 が localhost を中継するため）。

### 他の PC の装置から送る

そのままでは WSL2 に届きません。

- Windows 11 なら `.wslconfig` に次を書くのが簡単です。

  ```ini
  [wsl2]
  networkingMode=mirrored
  ```

- それ以外は `netsh interface portproxy` で中継し、Windows のファイアウォールも開けます。

### トークンが要ると言われた場合

parqlog は 127.0.0.1 以外からの接続にトークンを求めます。中継の仕方によっては接続元が 127.0.0.1 にならないかもしれず、その場合は `/etc/parqlog/parqlog.toml` でトークンを設定して使います。

## Tauri の管理アプリ

Windows 11 などで WSL から GUI アプリを出せる環境（WSLg）なら画面は出ます。ただし次の制約があります。

| 機能 | WSL2 での状態 | 代わりの方法 |
| --- | --- | --- |
| タスクトレイのアイコン | 出ない | 画面を開いたまま使う |
| サービスの開始・停止ボタン | 管理者の認証ダイアログが出ないことが多い | 端末で `sudo systemctl restart parqlog` |
| ログ表示 | 読み取り権限のエラーが出ることがある | `sudo usermod -aG systemd-journal $USER` のあと入り直す |

## 本番の Ubuntu へ移すとき

サービスを止めて `/var/lib/parqlog`（データ）と `/etc/parqlog/parqlog.toml`（設定）をコピーすれば移せます。
