// pcw-dash binary: the on-demand dashboard (plan Task 15). GUI subsystem:
// no console window. Launched by the agent tray (`Open Dashboard`, which
// spawns the sibling `pcw-dash.exe`) or by hand with an optional `--port N`
// override; otherwise the port comes from `config.toml`.

#![windows_subsystem = "windows"]

use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use pcw_config::Config;
use pcw_dashboard::{DashError, DashServer};

/// Shut down the dashboard after one hour with no requests (spec §5).
const IDLE_TIMEOUT: Duration = Duration::from_secs(3600);

fn main() {
    // `--port N` wins over the config's dashboard_port.
    let arg_port = parse_port_arg(&std::env::args().collect::<Vec<_>>());

    let data_dir = match pcw_config::default_data_dir() {
        Ok(dir) => dir,
        Err(_) => {
            // A windows-subsystem process has no console: best-effort only.
            eprintln!(
                "pcw-dash: cannot determine the data directory (PCW_DATA_DIR/LOCALAPPDATA unset)"
            );
            std::process::exit(1);
        }
    };
    // Creates the dir and the default config.toml on first run, so
    // `Config::load_or_create` is all that is needed here.
    let cfg = match Config::load_or_create(&data_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("pcw-dash: cannot load the configuration: {e}");
            std::process::exit(1);
        }
    };
    let port = arg_port.unwrap_or(cfg.dashboard_port);

    match DashServer::bind(port) {
        Ok(server) => {
            open_browser(port);
            // The route handler for `POST /api/shutdown` (the UI's Stop
            // button) sets this flag, which makes `run` return.
            let stop = Arc::new(AtomicBool::new(false));
            if let Err(e) = server.run(data_dir.join("data.db"), stop, IDLE_TIMEOUT) {
                eprintln!("pcw-dash: dashboard stopped with an error: {e}");
                std::process::exit(1);
            }
        }
        // A dashboard is already serving this port: open the browser to it
        // and quit successfully instead of failing on a second bind.
        Err(DashError::PortInUse) => {
            open_browser(port);
            std::process::exit(0);
        }
        Err(e) => {
            // No console to report to; note the choice: nothing is written
            // anywhere (the agent's agent.log only sees agent-side events).
            eprintln!("pcw-dash: {e}");
            std::process::exit(1);
        }
    }
}

/// Opens the loopback dashboard URL through `explorer.exe` so the browser
/// runs as the *normal* user even when pcw-dash was spawned by the elevated
/// agent (spec §5: never start an elevated browser).
///
/// The spawn is best-effort: a windows-subsystem process has no console to
/// report a failure to, and the server still runs so the URL can be opened
/// manually.
fn open_browser(port: u16) {
    let url = format!("http://127.0.0.1:{port}/");
    let _ = Command::new("explorer.exe").arg(url).spawn();
}

/// The only flag pcw-dash takes: `--port N`. Returns the parsed port on a
/// valid `--port <u16>` pair; anything else — no flag, dangling flag,
/// non-numeric value, out-of-range value — is `None`, which means "use the
/// config's `dashboard_port`". An unparseable override never panics (a GUI
/// process cannot ask the user; the config default is the safe fallback).
fn parse_port_arg(args: &[String]) -> Option<u16> {
    args.iter()
        .zip(args.iter().skip(1))
        .find(|(flag, _)| flag.as_str() == "--port")
        .and_then(|(_, value)| value.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::parse_port_arg;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn port_flag_is_parsed() {
        assert_eq!(
            parse_port_arg(&args(&["pcw-dash.exe", "--port", "8000"])),
            Some(8000)
        );
    }

    #[test]
    fn no_flag_means_the_config_default() {
        assert_eq!(parse_port_arg(&args(&["pcw-dash.exe"])), None);
        assert_eq!(parse_port_arg(&args(&["pcw-dash.exe", "--help"])), None);
    }

    #[test]
    fn bad_value_falls_back_to_the_config_default() {
        // Missing value, non-numeric, out of u16 range.
        assert_eq!(parse_port_arg(&args(&["pcw-dash.exe", "--port"])), None);
        assert_eq!(
            parse_port_arg(&args(&["pcw-dash.exe", "--port", "abc"])),
            None
        );
        assert_eq!(
            parse_port_arg(&args(&["pcw-dash.exe", "--port", "99999"])),
            None
        );
    }
}
