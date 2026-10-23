//! ETW session source: providers, callbacks, loss queries.
//!
//! `run_session` builds the three providers (Kernel-Network with keyword
//! `0x30`, DNS-Client with keywords at ferrisetw's default 0 = all,
//! Kernel-Process with keyword `0x10`), each narrowed by an event-id filter,
//! starts the `PC-Watcher-Net` real-time session and blocks in
//! `UserTrace::process_from_handle(handle)` until the session ends — which
//! happens when the program stops it, an external `logman stop` kills it, or
//! ETW errors out (the returned result feeds the watchdog).
//!
//! Every event carries a `PID` field (verified against this machine's
//! manifests), so attribution can use the event's own PID; Kernel-Process
//! start/stop events feed the shared process cache.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ferrisetw::EventRecord;
use ferrisetw::parser::Parser;
use ferrisetw::provider::{EventFilter, Provider};
use ferrisetw::schema_locator::SchemaLocator;
use ferrisetw::trace::{TraceProperties, TraceTrait, UserTrace, stop_trace_by_name};
use pcw_types::{ConnectionEvent, Counters, DnsAnswer, SharedProcessCache};
use windows::Win32::Foundation::NO_ERROR;
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, ControlTraceW, EVENT_TRACE_CONTROL_QUERY, EVENT_TRACE_PROPERTIES,
};
use windows::core::PCWSTR;

use crate::decode::{
    filetime_to_unix_ms, guess_direction, kernel_network_kind, parse_dns_results,
    port_from_network_order,
};
use crate::rate::RateLimiter;
use crate::snapshot;
use crate::{NetEvent, SESSION_NAME};

pub use crate::DNS_CLIENT_PROVIDER;
pub use crate::KERNEL_NETWORK_PROVIDER;
pub use crate::KERNEL_PROCESS_PROVIDER;

/// Kernel-Network event ids kept in: TCP connect (12/28, out), accept (15/31,
/// in), disconnect (13/29); UDP send (42/58, out), receive (43/59, in). The
/// high-volume data events (10/11/26/27) are filtered out.
pub const KERNEL_NETWORK_IDS: [u16; 10] = [12, 13, 15, 28, 29, 31, 42, 43, 58, 59];
/// DNS-Client event ids kept in: query completed.
pub const DNS_QUERY_IDS: [u16; 1] = [3008];
/// Kernel-Process event ids kept in: process start/stop.
pub const KERNEL_PROCESS_IDS: [u16; 2] = [1, 2];

/// Errors reported by `run_session`.
#[derive(Debug, Clone)]
pub enum SourceError {
    Trace(String),
    Win32(String),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trace(e) => write!(f, "etw trace error: {e}"),
            Self::Win32(e) => write!(f, "win32 error: {e}"),
        }
    }
}

impl std::error::Error for SourceError {}

/// Session options.
#[derive(Debug, Clone)]
pub struct SourceConfig {
    /// Whether DNS-Client 3008 events are turned into [`NetEvent::Dns`].
    pub capture_dns: bool,
    /// Max UDP rows per second per (pid, remote ip, remote port); 0 = off.
    pub udp_rate_cap: u32,
}

/// Returns ms since the Unix epoch for "now" (snapshot timestamps are
/// qualified by the read time, not the event time).
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Builds the three providers, starts the `PC-Watcher-Net` session, emits the
/// startup snapshot, then blocks in `process_from_handle` until the session
/// ends. Nothing is sent while `paused` is set.
pub fn run_session(
    cfg: &SourceConfig,
    cache: SharedProcessCache,
    counters: Arc<Counters>,
    paused: Arc<AtomicBool>,
    sink: Sender<NetEvent>,
) -> Result<(), SourceError> {
    let limiter = Arc::new(Mutex::new(RateLimiter::new(cfg.udp_rate_cap)));

    let network = network_provider(
        cache.clone(),
        counters.clone(),
        paused.clone(),
        sink.clone(),
        limiter,
    );
    let dns = dns_provider(cfg.capture_dns, paused.clone(), sink.clone());
    let process = process_provider(cache.clone());

    let (trace, handle) = UserTrace::new()
        .named(SESSION_NAME.into())
        .enable(network)
        .enable(dns)
        .enable(process)
        .set_trace_properties(TraceProperties {
            buffer_size: 64,
            ..Default::default()
        })
        .start()
        .map_err(|e| SourceError::Trace(format!("{e:?}")))?;

    // Connections open before this session started emit no connect event:
    // read the TCP table now (first start and every watchdog restart).
    for event in snapshot::tcp_snapshot_events(&cache, now_unix_ms()) {
        if !paused.load(Ordering::Relaxed) {
            let _ = sink.send(NetEvent::Connection(event));
        }
    }

    let result =
        UserTrace::process_from_handle(handle).map_err(|e| SourceError::Trace(format!("{e:?}")));
    // Drop stops the trace session itself (or reports it already stopped).
    drop(trace);
    result
}

/// Kernel-Network events → `ConnectionEvent`s. Remote = `daddr`/`dport`,
/// local port = `sport`; `Close` events have no direction field, so the
/// direction is guessed from the (converted) local port.
fn network_provider(
    cache: SharedProcessCache,
    counters: Arc<Counters>,
    paused: Arc<AtomicBool>,
    sink: Sender<NetEvent>,
    limiter: Arc<Mutex<RateLimiter>>,
) -> Provider {
    Provider::by_guid(KERNEL_NETWORK_PROVIDER)
        .any(0x30) // IPv4 + IPv6
        .add_filter(EventFilter::ByEventIds(KERNEL_NETWORK_IDS.to_vec()))
        .add_callback(move |record: &EventRecord, locator: &SchemaLocator| {
            let Some((protocol, direction, kind)) = kernel_network_kind(record.event_id()) else {
                return;
            };
            if paused.load(Ordering::Relaxed) {
                return;
            }
            let Ok(schema) = locator.event_schema(record) else {
                return;
            };
            let parser = Parser::create(record, &schema);
            let Ok(pid) = parser.try_parse::<u32>("PID") else {
                return;
            };
            let Ok(remote_ip) = parser.try_parse::<IpAddr>("daddr") else {
                return;
            };
            let Ok(remote_port_net) = parser.try_parse::<u16>("dport") else {
                return;
            };
            let Ok(local_port_net) = parser.try_parse::<u16>("sport") else {
                return;
            };
            let remote_port = port_from_network_order(remote_port_net);
            let local_port = port_from_network_order(local_port_net);

            if protocol == pcw_types::Protocol::Udp {
                let now_ms = filetime_to_unix_ms(record.raw_timestamp());
                // Poison-tolerant: ferrisetw wraps callbacks in
                // catch_unwind; a poisoned mutex here would otherwise take
                // down the whole session (and the agent) on the next event.
                let allowed = limiter.lock().unwrap_or_else(|e| e.into_inner()).allow(
                    pid,
                    remote_ip,
                    remote_port,
                    now_ms,
                );
                if !allowed {
                    counters.add("udp_capped", 1);
                    return;
                }
            }

            let (process_name, process_path) = {
                let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.get(pid)
            };
            let event = ConnectionEvent {
                ts_ms: filetime_to_unix_ms(record.raw_timestamp()),
                kind,
                pid,
                process_name,
                process_path,
                remote_ip: remote_ip.to_string(),
                remote_port,
                local_port,
                protocol,
                direction: direction.unwrap_or_else(|| guess_direction(local_port)),
            };
            let _ = sink.send(NetEvent::Connection(event));
        })
        .build()
}

/// DNS-Client 3008 → `DnsAnswer`s, only when `QueryStatus == 0` and
/// `capture_dns` is on.
fn dns_provider(capture_dns: bool, paused: Arc<AtomicBool>, sink: Sender<NetEvent>) -> Provider {
    Provider::by_guid(DNS_CLIENT_PROVIDER)
        .add_filter(EventFilter::ByEventIds(DNS_QUERY_IDS.to_vec()))
        .add_callback(move |record: &EventRecord, locator: &SchemaLocator| {
            if record.event_id() != 3008 {
                return;
            }
            if !capture_dns || paused.load(Ordering::Relaxed) {
                return;
            }
            let Ok(schema) = locator.event_schema(record) else {
                return;
            };
            let parser = Parser::create(record, &schema);
            let Ok(status) = parser.try_parse::<u32>("QueryStatus") else {
                return;
            };
            if status != 0 {
                return;
            }
            let Ok(name) = parser.try_parse::<String>("QueryName") else {
                return;
            };
            let Ok(results) = parser.try_parse::<String>("QueryResults") else {
                return;
            };
            let ips = parse_dns_results(&results);
            if ips.is_empty() {
                return;
            }
            let _ = sink.send(NetEvent::Dns(DnsAnswer {
                ts_ms: filetime_to_unix_ms(record.raw_timestamp()),
                name,
                ips,
            }));
        })
        .build()
}

/// Kernel-Process 1 (start) / 2 (stop) → `cache.note_process`. Both record
/// the image path as the fallback; a start drops the cached entry so the
/// next lookup re-queries (the pid may have been reused).
fn process_provider(cache: SharedProcessCache) -> Provider {
    Provider::by_guid(KERNEL_PROCESS_PROVIDER)
        .any(0x10) // process keyword
        .add_filter(EventFilter::ByEventIds(KERNEL_PROCESS_IDS.to_vec()))
        .add_callback(move |record: &EventRecord, locator: &SchemaLocator| {
            let event_id = record.event_id();
            if event_id != 1 && event_id != 2 {
                return;
            }
            let Ok(schema) = locator.event_schema(record) else {
                return;
            };
            let parser = Parser::create(record, &schema);
            let Ok(pid) = parser.try_parse::<u32>("ProcessID") else {
                return;
            };
            let Ok(image) = parser.try_parse::<String>("ImageName") else {
                return;
            };
            let started = event_id == 1;
            let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.note_process(pid, &image, started);
        })
        .build()
}

/// Reads `EventsLost` / `RealTimeBuffersLost` of a live session via
/// `ControlTraceW(EVENT_TRACE_CONTROL_QUERY)`. Returns `None` on failure
/// (no session, no rights, …).
pub fn query_loss(session: &str) -> Option<(u64, u64)> {
    // The queried logger name is laid out directly after the properties
    // struct; 1024 bytes is room for long names.
    const NAME_SPACE_BYTES: usize = 1024;

    let properties_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>();
    let total = properties_size + NAME_SPACE_BYTES;
    // `EVENT_TRACE_PROPERTIES` needs pointer alignment; a plain `u8` vec only
    // guarantees align 1, so the region is backed by `u64` cells (both sizes
    // are multiples of 8, keeping the byte layout identical).
    debug_assert_eq!(properties_size % 8, 0);
    let mut buf = vec![0u64; total / 8 + usize::from(total % 8 != 0)];
    let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;

    unsafe {
        (*props).Wnode.BufferSize = total as u32;
        (*props).LoggerNameOffset = properties_size as u32;

        let name: Vec<u16> = session.encode_utf16().chain(std::iter::once(0)).collect();
        if name.len() * 2 > NAME_SPACE_BYTES {
            return None;
        }
        // Null-terminated UTF-16 name written right behind the properties
        // struct, through the same pointer API as `ControlTraceW` — never
        // through an aliased `u8` slice view.
        debug_assert_eq!(properties_size % 2, 0);
        let name_slot = buf
            .as_mut_ptr()
            .cast::<u8>()
            .add(properties_size)
            .cast::<u16>();
        std::ptr::copy_nonoverlapping(name.as_ptr(), name_slot, name.len());

        let status = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR::from_raw(name.as_ptr()),
            props,
            EVENT_TRACE_CONTROL_QUERY,
        );
        if status == NO_ERROR {
            Some((
                (*props).EventsLost as u64,
                (*props).RealTimeBuffersLost as u64,
            ))
        } else {
            None
        }
    }
}

/// Stops the named session (if any). Errors are ignored: a missing session is
/// the normal case at startup. Used both to clear leftovers before a start
/// and as the documented external way to stop (mirrors `logman stop`).
pub fn stop_session(session: &str) {
    let _ = stop_trace_by_name(session);
}
