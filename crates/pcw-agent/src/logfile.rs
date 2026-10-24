//! File logger (plan Task 12): `%LOCALAPPDATA%\PC-Watcher\agent.log`, a
//! `log::Log` writing timestamped lines, rotated at `max_bytes` (5 MB in the
//! agent) into `<path>.1`, replacing the old copy there (spec §5: exactly
//! one old file is kept).
//!
//! Every line opens, writes and closes the file and takes its size from
//! `metadata` first, so rotation never races an open handle: the rename runs
//! against a closed file. The agent logs only a few lines per minute, so the
//! per-line syscall cost is irrelevant.

use std::fs::{OpenOptions, remove_file, rename};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use log::{Level, LevelFilter, Log, Metadata, Record, set_boxed_logger, set_max_level};
use time::OffsetDateTime;
use time::macros::format_description;

const TIMESTAMP_FORMAT: &[time::format_description::FormatItem<'static>] =
    format_description!("[year]-[month]-[day] [hour]:[minute]:[second]");

/// Installs the global logger, keeping at most one rotated copy (`<path>.1`).
/// A logger already installed (e.g. a test) is left untouched.
pub fn init(path: &Path, max_bytes: u64) {
    let logger = FileLogger {
        target: Mutex::new(path.to_path_buf()),
        max_bytes: Mutex::new(max_bytes),
    };
    if set_boxed_logger(Box::new(logger)).is_ok() {
        set_max_level(LevelFilter::Info);
    }
    log::info!("logging to {}", path.display());
}

/// Timestamped line format: `[2026-10-22 14:03:57] [INFO] message`.
struct FileLogger {
    target: Mutex<PathBuf>,
    max_bytes: Mutex<u64>,
}

impl FileLogger {
    /// Locks the configuration the line is written with; `log` never calls
    /// nested, so this cannot self-deadlock.
    fn line_context(&self) -> (MutexGuard<'_, PathBuf>, u64) {
        let path = self.target.lock().unwrap_or_else(|p| p.into_inner());
        let max_bytes = *self.max_bytes.lock().unwrap_or_else(|p| p.into_inner());
        (path, max_bytes)
    }

    /// Rotates when size exceeded `max_bytes`: closes nothing (no open
    /// handle), removes the stale `.1` (`fs::rename` never overwrites on
    /// Windows), renames the live file over it. A failed rotation still
    /// writes the line to the same file.
    fn rotate_if_needed(path: &Path, max_bytes: u64) {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if size <= max_bytes {
            return;
        }
        let mut rotated = path.as_os_str().to_os_string();
        rotated.push(".1");
        let rotated = PathBuf::from(rotated);
        let _ = remove_file(&rotated);
        let _ = rename(path, &rotated);
    }

    fn write_line(path: &Path, line: &str) {
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= Level::Info
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let timestamp = OffsetDateTime::now_local()
            .unwrap_or_else(|_| OffsetDateTime::now_utc())
            .format(&TIMESTAMP_FORMAT)
            .unwrap_or_else(|_| "?".to_string());
        // Multi-line messages collapse onto one physical line.
        let message = record.args().to_string().replace(['\n', '\r'], " ");
        let line = format!("[{timestamp}] [{}] {message}\n", record.level());
        let (path, max_bytes) = self.line_context();
        Self::rotate_if_needed(&path, max_bytes);
        Self::write_line(&path, &line);
    }

    fn flush(&self) {}
}
