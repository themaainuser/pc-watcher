//! Schema v1, exactly as spec §4.6 (tables, column names/types and the seven
//! indexes). Application is idempotent: every statement is `IF NOT EXISTS`, so
//! `execute_batch` on an existing database changes nothing.

pub(crate) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS connections(
  id INTEGER PRIMARY KEY,
  ts_first INT, ts_last INT,
  pid INT, process_name TEXT, process_path TEXT,
  remote_ip TEXT, remote_port INT, local_port INT,
  protocol TEXT,              -- 'tcp' | 'udp'
  direction TEXT              -- 'out' | 'in'
);

CREATE TABLE IF NOT EXISTS app_sessions(
  id INTEGER PRIMARY KEY, ts_start INT, ts_end INT,
  process_name TEXT, window_title TEXT      -- title when the session opened
);

CREATE TABLE IF NOT EXISTS window_titles(
  id INTEGER PRIMARY KEY, ts INT, process_name TEXT, title TEXT
);

CREATE TABLE IF NOT EXISTS hostnames(
  ip TEXT PRIMARY KEY, hostname TEXT,
  source TEXT,                -- 'dns' | 'ptr' | 'none' | 'pending'
  resolved_at INT
);

CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT);

CREATE INDEX IF NOT EXISTS idx_connections_process_ts_first ON connections(process_name, ts_first);
CREATE INDEX IF NOT EXISTS idx_connections_remote_ip ON connections(remote_ip);
CREATE INDEX IF NOT EXISTS idx_connections_ts_first ON connections(ts_first);
CREATE INDEX IF NOT EXISTS idx_app_sessions_process_ts_start ON app_sessions(process_name, ts_start);
CREATE INDEX IF NOT EXISTS idx_window_titles_ts ON window_titles(ts);
CREATE INDEX IF NOT EXISTS idx_window_titles_process_ts ON window_titles(process_name, ts);
CREATE INDEX IF NOT EXISTS idx_hostnames_source ON hostnames(source);
"#;
