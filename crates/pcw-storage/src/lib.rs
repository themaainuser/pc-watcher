//! PC-Watcher storage: SQLite schema v1, the write path and the read path
//! (queries, PTR-worker support and streaming export readers). Retention
//! pruning (`prune.rs`) and export are separate modules (plan Tasks 6–8).

mod prune;
mod read;
mod schema;
mod write;

pub use read::{
    ConnFilter, ConnectionRow, DestinationRow, OverviewSummary, SessionRow, TitleRow, UsageSummary,
};
pub use write::{SharedStore, StorageError, Store, WriteOp};
