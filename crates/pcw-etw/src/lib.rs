// pcw-etw: ETW event tracing capture.
//!
//! Collects TCP/UDP connections and DNS answers from the ETW providers
//! `Microsoft-Windows-Kernel-Network`, `Microsoft-Windows-DNS-Client` and
//! `Microsoft-Windows-Kernel-Process` in one real-time session, plus the
//! supporting pieces: the startup snapshot of the OS TCP table, the process
//! lookup behind the shared cache, the UDP rate cap and the session watchdog
//! bookkeeping.
//!
//! Module map:
//! - [`decode`] — pure decoding helpers (byte order, event ids, DNS results)
//! - [`process`] — PID → (process name, path) via Win32
//! - [`rate`] — per-key datagram rate limiter (UDP fallback cap)
//! - [`snapshot`] — connections open at session start, from the TCP table
//! - [`source`] — the three providers, `run_session`, loss query, stop
//! - [`supervisor`] — restart bookkeeping and event-loss accounting

pub mod decode;
pub mod process;
pub mod rate;
pub mod snapshot;
pub mod source;
pub mod supervisor;

pub use pcw_types::{DnsAnswer, EventKind};

use pcw_types::ConnectionEvent;

/// Name of the real-time ETW session (visible in `logman query`).
pub const SESSION_NAME: &str = "PC-Watcher-Net";

/// `Microsoft-Windows-Kernel-Network` (spec §4.1).
pub const KERNEL_NETWORK_PROVIDER: &str = "7dd42a49-5329-4832-8dfd-43d979153a88";
/// `Microsoft-Windows-DNS-Client`.
pub const DNS_CLIENT_PROVIDER: &str = "1c95126e-7eea-49a9-a3fe-a378b03ddb4d";
/// `Microsoft-Windows-Kernel-Process`.
pub const KERNEL_PROCESS_PROVIDER: &str = "22fb2cd6-0e7b-422b-a0c7-2fad1fd0e716";

/// One event to be recorded: a connection row or a DNS answer.
#[derive(Debug, Clone, PartialEq)]
pub enum NetEvent {
    Connection(ConnectionEvent),
    Dns(DnsAnswer),
}
