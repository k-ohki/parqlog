//! tsd のコア: Line Protocol、ストレージ（DuckDB + Parquet）、集計、保持期間管理。

pub mod config;
pub mod duck;
pub mod ingest;
pub mod lp;
pub mod store;
pub mod time;

pub use config::Config;
pub use duck::{DuckStore, StoreOptions};
pub use ingest::Ingestor;
pub use store::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
