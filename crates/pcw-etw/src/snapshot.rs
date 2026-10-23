//! The startup snapshot: connections already open when the session starts.
//!
//! Connections open before collection starts never produce a connect event,
//! so each time the session starts (first start and every watchdog restart)
//! the OS TCP table is read (`GetExtendedTcpTable`, `TCP_TABLE_OWNER_PID_ALL`,
//! IPv4 and IPv6) and one Open event is emitted per established connection.
//! A local port that belongs to a listening socket reads as an accepted,
//! "in" connection; anything else dialed out. Ports in the table are network
//! byte order, exactly like the ETW event fields.

use std::collections::HashSet;
use std::net::{Ipv4Addr, Ipv6Addr};

use pcw_types::{ConnectionEvent, EventKind, Protocol, SharedProcessCache};
use windows::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCP_STATE_ESTAB, MIB_TCP_STATE_LISTEN, MIB_TCP6ROW_OWNER_PID,
    MIB_TCP6TABLE_OWNER_PID, MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_ALL,
};
use windows::Win32::Networking::WinSock::{AF_INET, AF_INET6};

use crate::decode::{port_from_network_order, snapshot_direction};
use crate::source::SourceError;

const NO_ERROR: u32 = 0;
const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

/// One row of the TCP table, fields already converted to host order.
struct Row {
    local_port: u16,
    remote_ip: String,
    remote_port: u16,
    pid: u32,
    listening: bool,
}

/// Reads the OS TCP table (IPv4 + IPv6) and returns one Open event per
/// established row. An unreadable table yields an empty snapshot — the ETW
/// session is still good, so this is best effort, not a reason to kill the
/// session.
pub fn tcp_snapshot_events(cache: &SharedProcessCache, ts_ms: u64) -> Vec<ConnectionEvent> {
    let (rows, listening) = match table_rows() {
        Ok(rows_and_listening) => rows_and_listening,
        // Kept visible on stderr (real in console/example runs); the ETW loss
        // counters remain the volume-honesty mechanism for silent loss.
        Err(err) => {
            eprintln!("pcw-etw: TCP table snapshot failed: {err}");
            return Vec::new();
        }
    };
    let mut events = Vec::new();
    for row in rows {
        let (process_name, process_path) = {
            let mut cache = cache.lock().expect("process cache poisoned");
            cache.get(row.pid)
        };
        events.push(ConnectionEvent {
            ts_ms,
            kind: EventKind::Open,
            pid: row.pid,
            process_name,
            process_path,
            remote_ip: row.remote_ip,
            remote_port: row.remote_port,
            local_port: row.local_port,
            protocol: Protocol::Tcp,
            direction: snapshot_direction(row.local_port, &listening),
        });
    }
    events
}

/// Reads both tables, returning (established rows, listening ports).
fn table_rows() -> Result<(Vec<Row>, HashSet<u16>), SourceError> {
    let v4 = read_table(AF_INET.0 as u32, parse_v4)?;
    let v6 = read_table(AF_INET6.0 as u32, parse_v6)?;
    let mut listening: HashSet<u16> = HashSet::new();
    let mut rows: Vec<Row> = Vec::new();
    for row in v4.into_iter().chain(v6) {
        if row.listening {
            listening.insert(row.local_port);
        } else {
            rows.push(row);
        }
    }
    Ok((rows, listening))
}

/// Parses one of the row slice shapes; only used where the two tables differ.
type ParseFn = fn(&[u8]) -> Vec<Row>;

/// Calls `GetExtendedTcpTable(af, TCP_TABLE_OWNER_PID_ALL)`: one sizing call,
/// then retries while the table keeps growing between our calls.
fn read_table(af: u32, parse: ParseFn) -> Result<Vec<Row>, SourceError> {
    let mut size: u32 = 0;
    for _ in 0..4 {
        if size == 0 {
            let status = unsafe {
                GetExtendedTcpTable(None, &mut size, false, af, TCP_TABLE_OWNER_PID_ALL, 0)
            };
            if status != ERROR_INSUFFICIENT_BUFFER {
                return Err(SourceError::Win32(format!(
                    "GetExtendedTcpTable(af={af}) sizing failed: {status}"
                )));
            }
            continue;
        }
        let mut buf = vec![0u8; size as usize];
        let status = unsafe {
            GetExtendedTcpTable(
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut size,
                false,
                af,
                TCP_TABLE_OWNER_PID_ALL,
                0,
            )
        };
        if status == NO_ERROR {
            return Ok(parse(&buf));
        }
        if status != ERROR_INSUFFICIENT_BUFFER {
            return Err(SourceError::Win32(format!(
                "GetExtendedTcpTable(af={af}) failed: {status}"
            )));
        }
    }
    Err(SourceError::Win32(format!(
        "GetExtendedTcpTable(af={af}): table kept growing, giving up"
    )))
}

fn parse_v4(buf: &[u8]) -> Vec<Row> {
    if buf.len() < std::mem::size_of::<MIB_TCPTABLE_OWNER_PID>() {
        return Vec::new();
    }
    let table = unsafe { &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID) };
    let count = table.dwNumEntries as usize;
    if count == 0 {
        return Vec::new();
    }
    let needed = (count - 1) * std::mem::size_of::<MIB_TCPROW_OWNER_PID>()
        + std::mem::size_of::<MIB_TCPTABLE_OWNER_PID>();
    if needed > buf.len() {
        return Vec::new();
    }
    let entries = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), count) };
    entries
        .iter()
        .filter_map(|r| {
            if r.dwState == MIB_TCP_STATE_ESTAB.0 as u32
                || r.dwState == MIB_TCP_STATE_LISTEN.0 as u32
            {
                Some(Row {
                    local_port: port_from_network_order((r.dwLocalPort & 0xFFFF) as u16),
                    remote_ip: if r.dwState == MIB_TCP_STATE_ESTAB.0 as u32 {
                        Ipv4Addr::from(u32::from_be(r.dwRemoteAddr)).to_string()
                    } else {
                        String::new()
                    },
                    remote_port: port_from_network_order((r.dwRemotePort & 0xFFFF) as u16),
                    pid: r.dwOwningPid,
                    listening: r.dwState == MIB_TCP_STATE_LISTEN.0 as u32,
                })
            } else {
                None
            }
        })
        .collect()
}

fn parse_v6(buf: &[u8]) -> Vec<Row> {
    if buf.len() < std::mem::size_of::<MIB_TCP6TABLE_OWNER_PID>() {
        return Vec::new();
    }
    let table = unsafe { &*(buf.as_ptr() as *const MIB_TCP6TABLE_OWNER_PID) };
    let count = table.dwNumEntries as usize;
    if count == 0 {
        return Vec::new();
    }
    let needed = (count - 1) * std::mem::size_of::<MIB_TCP6ROW_OWNER_PID>()
        + std::mem::size_of::<MIB_TCP6TABLE_OWNER_PID>();
    if needed > buf.len() {
        return Vec::new();
    }
    let entries = unsafe { std::slice::from_raw_parts(table.table.as_ptr(), count) };
    entries
        .iter()
        .filter_map(|r| {
            let listening = r.dwState == MIB_TCP_STATE_LISTEN.0 as u32;
            if r.dwState != MIB_TCP_STATE_ESTAB.0 as u32 && !listening {
                return None;
            }
            Some(Row {
                local_port: port_from_network_order((r.dwLocalPort & 0xFFFF) as u16),
                remote_ip: if listening {
                    String::new()
                } else {
                    Ipv6Addr::from(r.ucRemoteAddr).to_string()
                },
                remote_port: port_from_network_order((r.dwRemotePort & 0xFFFF) as u16),
                pid: r.dwOwningPid,
                listening,
            })
        })
        .collect()
}
