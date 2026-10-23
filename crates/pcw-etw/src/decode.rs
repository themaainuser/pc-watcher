//! Pure decoding helpers shared by the ETW source and the startup snapshot.
//!
//! All port values in this crate arrive in network byte order (Kernel-Network
//! `win:Port` fields and `GetExtendedTcpTable` port fields alike); every one of
//! them must go through [`port_from_network_order`] before use.

use pcw_types::{Direction, EventKind, Protocol};
use std::collections::HashSet;
use std::net::IpAddr;

/// Converts a `win:Port` (or table port) from network byte order.
pub fn port_from_network_order(raw: u16) -> u16 {
    u16::from_be(raw)
}

/// Maps a Kernel-Network event id to `(protocol, direction, kind)`.
///
/// Event ids are (IPv4, IPv6) pairs: TCP connect 12/28 (out), accept 15/31
/// (in), disconnect 13/29; UDP send 42/58 (out), receive 43/59 (in). Close
/// events carry no direction in the event itself — the caller guesses it from
/// the local port; the high-volume data events (10/11/26/27) return `None`.
pub fn kernel_network_kind(event_id: u16) -> Option<(Protocol, Option<Direction>, EventKind)> {
    match event_id {
        12 | 28 => Some((Protocol::Tcp, Some(Direction::Out), EventKind::Open)),
        15 | 31 => Some((Protocol::Tcp, Some(Direction::In), EventKind::Open)),
        13 | 29 => Some((Protocol::Tcp, None, EventKind::Close)),
        42 | 58 => Some((Protocol::Udp, Some(Direction::Out), EventKind::Packet)),
        43 | 59 => Some((Protocol::Udp, Some(Direction::In), EventKind::Packet)),
        _ => None,
    }
}

/// Direction guess for a close event: Windows assigns ephemeral source ports
/// from the 49152..=65535 dynamic range, so those reads as "out".
pub fn guess_direction(local_port: u16) -> Direction {
    if (49152..=65535).contains(&local_port) {
        Direction::Out
    } else {
        Direction::In
    }
}

/// Direction for a snapshot row: a local port that also has a listening
/// socket is an accepted ("in") connection, anything else dialed out.
pub fn snapshot_direction(local_port: u16, listening: &HashSet<u16>) -> Direction {
    if listening.contains(&local_port) {
        Direction::In
    } else {
        Direction::Out
    }
}

/// Splits the DNS-Client `QueryResults` string into IP addresses.
///
/// Entries are separated by `;`. CNAME records appear as `type:  5 name` and
/// are skipped; IPv4-mapped IPv6 addresses are stripped of their `::ffff:`
/// prefix; only entries that parse as an [`IpAddr`] survive.
pub fn parse_dns_results(results: &str) -> Vec<String> {
    let mut ips = Vec::new();
    for entry in results.split(';') {
        let entry = entry.trim();
        if entry.is_empty() || entry.starts_with("type:") {
            continue;
        }
        let entry = entry.strip_prefix("::ffff:").unwrap_or(entry);
        if entry.parse::<IpAddr>().is_ok() {
            ips.push(entry.to_string());
        }
    }
    ips
}

/// Converts a Kernel/FILETIME timestamp (100 ns units since 1601-01-01) to
/// milliseconds since the Unix epoch.
pub fn filetime_to_unix_ms(raw: i64) -> u64 {
    (raw as u64)
        .checked_div(10_000)
        .and_then(|ms| ms.checked_sub(11_644_473_600_000))
        .unwrap_or(0)
}
