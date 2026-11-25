//! ログは標準出力へ（systemd では journald が受け取る）。詳しさは環境変数 PARQLOG_LOG（例: debug）。

use tracing_subscriber::EnvFilter;

pub fn init() {
    let filter = EnvFilter::try_from_env("PARQLOG_LOG").unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn,hyper=warn"));
    // journald は時刻を付けるので、端末でないときは時刻を省く
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let b = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).with_ansi(tty);
    if tty {
        b.init();
    } else {
        b.without_time().init();
    }
}
