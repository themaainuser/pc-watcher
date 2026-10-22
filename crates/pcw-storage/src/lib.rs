//! PC-Watcher storage: SQLite schema v1 and the write path.
//!
//! Read path (`read.rs`), retention pruning (`prune.rs`) and export are
//! separate modules (plan Tasks 5–8).

mod schema;
mod write;

pub use write::{SharedStore, StorageError, Store, WriteOp};
