//! PC-Watcher configuration: loads or creates `%LOCALAPPDATA%\PC-Watcher\config.toml`.
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use thiserror::Error;

/// Default `config.toml` written on first run, with the spec §8 comments.
const DEFAULT_CONFIG_TOML: &str = "\
# PC-Watcher configuration.
# Missing keys take their defaults.

retention_days = 10
resolve_dns = true              # reverse-DNS fallback (makes PTR queries)
capture_dns_names = true        # names from Windows DNS client answers (no traffic)
capture_window_titles = true    # owner decision: on
export_folder = \"G:\\\\My Drive\\\\PC-Watcher\"
export_schedule = \"weekly\"
dashboard_port = 7411
udp_rate_cap = 0                # 0 = off; thinning fallback, §4.1
min_free_disk_gb = 2            # below this, UDP datagram rows stop, §4.6
";

#[derive(Debug, Error)]
pub enum ConfigError {
    /// IO failure while creating the data dir, writing, or reading `config.toml`.
    #[error("i/o error: {0}")]
    Io(#[from] io::Error),
    /// `config.toml` exists but is not valid TOML.
    #[error("failed to parse config.toml: {0}")]
    Parse(String),
    /// Neither `PCW_DATA_DIR` nor `LOCALAPPDATA` is set, so no data dir can be derived.
    #[error("no data directory: neither PCW_DATA_DIR nor LOCALAPPDATA is set")]
    NoDataDir,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub retention_days: u32,
    pub resolve_dns: bool,
    pub capture_dns_names: bool,
    pub capture_window_titles: bool,
    pub export_folder: PathBuf,
    pub export_schedule: String,
    pub dashboard_port: u16,
    pub udp_rate_cap: u32,
    pub min_free_disk_gb: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            retention_days: 10,
            resolve_dns: true,
            capture_dns_names: true,
            capture_window_titles: true,
            export_folder: PathBuf::from("G:\\My Drive\\PC-Watcher"),
            export_schedule: "weekly".to_string(),
            dashboard_port: 7411,
            udp_rate_cap: 0,
            min_free_disk_gb: 2,
        }
    }
}

/// Resolve the data directory from explicit env values so callers (and tests)
/// never need to mutate the process environment.
///
/// `pcw_data_dir` (`PCW_DATA_DIR`) wins when set; otherwise
/// `<local_app_data>\PC-Watcher`; otherwise [`ConfigError::NoDataDir`].
/// An env value SET TO THE EMPTY STRING counts as absent (review flag F7):
/// a bare `PCW_DATA_DIR=""` must not resolve to the current directory and an
/// empty `LOCALAPPDATA` must not resolve to `\PC-Watcher` on the current drive.
pub fn data_dir_from(
    pcw_data_dir: Option<OsString>,
    local_app_data: Option<OsString>,
) -> Result<PathBuf, ConfigError> {
    if let Some(dir) = pcw_data_dir.filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    local_app_data
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .map(|base| base.join("PC-Watcher"))
        .ok_or(ConfigError::NoDataDir)
}

/// Resolve the data directory from the process environment.
pub fn default_data_dir() -> Result<PathBuf, ConfigError> {
    data_dir_from(env::var_os("PCW_DATA_DIR"), env::var_os("LOCALAPPDATA"))
}

impl Config {
    /// Create `data_dir` if missing, write the spec §8 default `config.toml`
    /// when absent, then parse it. Missing keys take defaults; unknown keys
    /// are tolerated (serde ignores fields it does not know about).
    pub fn load_or_create(data_dir: &Path) -> Result<Config, ConfigError> {
        fs::create_dir_all(data_dir)?;
        let path = data_dir.join("config.toml");
        if !path.exists() {
            fs::write(&path, DEFAULT_CONFIG_TOML)?;
        }
        let text = fs::read_to_string(&path)?;
        toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))
    }
}
