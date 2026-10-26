use std::path::PathBuf;

use pcw_config::{Config, ConfigError, data_dir_from, parse_export_schedule};

#[test]
fn export_schedule_days_are_mapped() {
    // Named schedules (spec §7 "weekly (configurable)" + the §8 default).
    assert_eq!(parse_export_schedule("daily"), Some(1));
    assert_eq!(parse_export_schedule("weekly"), Some(7));
    assert_eq!(parse_export_schedule("monthly"), Some(30));
    // A plain integer is a day count.
    assert_eq!(parse_export_schedule("3"), Some(3));
    assert_eq!(parse_export_schedule(" 14 "), Some(14));
    // Unparseable values are rejected (the agent falls back to weekly).
    assert_eq!(parse_export_schedule("fortnightly"), None);
    assert_eq!(parse_export_schedule("0"), None);
    assert_eq!(parse_export_schedule("1.5"), None);
    assert_eq!(parse_export_schedule(""), None);
}

#[test]
fn creates_default_config_with_spec_values() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load_or_create(dir.path()).unwrap();
    assert_eq!(cfg.retention_days, 10);
    assert!(cfg.resolve_dns);
    assert!(cfg.capture_dns_names);
    assert!(cfg.capture_window_titles);
    assert_eq!(cfg.export_folder, PathBuf::from("D:\\PC-Watcher"));
    assert_eq!(cfg.export_schedule, "weekly");
    assert_eq!(cfg.dashboard_port, 7411);
    assert_eq!(cfg.udp_rate_cap, 0);
    assert_eq!(cfg.min_free_disk_gb, 2);
    assert!(dir.path().join("config.toml").exists());
}

#[test]
fn reloads_edited_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "retention_days = 5\n").unwrap();
    let cfg = Config::load_or_create(dir.path()).unwrap();
    assert_eq!(cfg.retention_days, 5);
    // other keys take their defaults
    assert!(cfg.resolve_dns);
    assert!(cfg.capture_dns_names);
    assert!(cfg.capture_window_titles);
    assert_eq!(cfg.export_folder, PathBuf::from("D:\\PC-Watcher"));
    assert_eq!(cfg.export_schedule, "weekly");
    assert_eq!(cfg.dashboard_port, 7411);
    assert_eq!(cfg.udp_rate_cap, 0);
    assert_eq!(cfg.min_free_disk_gb, 2);
}

#[test]
fn invalid_toml_is_parse_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "not = = toml\n").unwrap();
    let err = Config::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(err, ConfigError::Parse(_)));
}

#[test]
fn data_dir_env_override_wins() {
    assert_eq!(
        data_dir_from(Some("D:\\x".into()), Some("C:\\L".into())).unwrap(),
        PathBuf::from("D:\\x")
    );
    assert_eq!(
        data_dir_from(None, Some("C:\\L".into())).unwrap(),
        PathBuf::from("C:\\L\\PC-Watcher")
    );
    // F7: an env value set to the empty string counts as absent, so the
    // fallback (or NoDataDir) applies instead of the current directory.
    assert_eq!(
        data_dir_from(Some("".into()), Some("C:\\L".into())).unwrap(),
        PathBuf::from("C:\\L\\PC-Watcher")
    );
    assert!(matches!(
        data_dir_from(None, Some("".into())),
        Err(ConfigError::NoDataDir)
    ));
    assert!(matches!(
        data_dir_from(Some("".into()), None),
        Err(ConfigError::NoDataDir)
    ));
    assert!(matches!(
        data_dir_from(None, None),
        Err(ConfigError::NoDataDir)
    ));
}
