#!/bin/sh
# parqlog を Ubuntu にインストールして systemd に登録する（リポジトリのルートで sudo 実行）
#   sudo sh deploy/install.sh
set -eu
cd "$(dirname "$0")/.."

if [ ! -x target/release/parqlog ]; then
  echo "先に cargo build --release -p parqlog を実行してください（初回は DuckDB のビルドに 15〜30 分）" >&2
  exit 1
fi

install -m 755 target/release/parqlog /usr/local/bin/parqlog
id parqlog >/dev/null 2>&1 || useradd --system --home /var/lib/parqlog --shell /usr/sbin/nologin parqlog
install -d -o parqlog -g parqlog -m 750 /var/lib/parqlog
install -d -o parqlog -g parqlog -m 750 /etc/parqlog
[ -f /etc/parqlog/parqlog.toml ] || sudo -u parqlog /usr/local/bin/parqlog init-config --config /etc/parqlog/parqlog.toml
install -m 644 deploy/parqlog.service /etc/systemd/system/parqlog.service
systemctl daemon-reload
systemctl enable --now parqlog
systemctl --no-pager status parqlog | head -5
