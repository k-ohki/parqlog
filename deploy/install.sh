#!/bin/sh
# tsd を Ubuntu にインストールして systemd に登録する（リポジトリのルートで sudo 実行）
#   sudo sh deploy/install.sh
set -eu
cd "$(dirname "$0")/.."

if [ ! -x target/release/tsd ]; then
  echo "先に cargo build --release -p tsd を実行してください（初回は DuckDB のビルドに 15〜30 分）" >&2
  exit 1
fi

install -m 755 target/release/tsd /usr/local/bin/tsd
id tsd >/dev/null 2>&1 || useradd --system --home /var/lib/tsd --shell /usr/sbin/nologin tsd
install -d -o tsd -g tsd -m 750 /var/lib/tsd
install -d -o tsd -g tsd -m 750 /etc/tsd
[ -f /etc/tsd/tsd.toml ] || sudo -u tsd /usr/local/bin/tsd init-config --config /etc/tsd/tsd.toml
install -m 644 deploy/tsd.service /etc/systemd/system/tsd.service
systemctl daemon-reload
systemctl enable --now tsd
systemctl --no-pager status tsd | head -5
