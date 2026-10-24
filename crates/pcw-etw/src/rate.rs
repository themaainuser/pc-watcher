//! Per-key datagram rate limiting for the UDP fallback cap.
//!
//! The cap (`udp_rate_cap`) is rows per second per (PID, remote IP, remote
//! port) key. `0` disables the limiter entirely: the map is never touched and
//! everything is allowed. Datagrams that the cap rejects are counted with the
//! `udp_capped` counter by the caller — separately from ETW event loss.

use std::collections::HashMap;
use std::net::IpAddr;

/// Keys idle for longer than this are dropped.
const IDLE_MS: u64 = 10_000;
/// The idle sweep runs at most once a minute.
const SWEEP_INTERVAL_MS: u64 = 60_000;

#[derive(Debug)]
pub struct RateLimiter {
    cap_per_sec: u32,
    last_sweep: u64,
    buckets: HashMap<(u32, IpAddr, u16), Bucket>,
    /// How many datagrams this limiter has rejected. Test-observable state
    /// (tests assert on it directly); production counts rejections via the
    /// `udp_capped` counter in `source.rs`, which resets per session anyway.
    pub capped: u64,
}

#[derive(Debug, Default)]
struct Bucket {
    count: u32,
    window_start: u64,
    last_used: u64,
}

impl RateLimiter {
    pub fn new(cap_per_sec: u32) -> Self {
        Self {
            cap_per_sec,
            last_sweep: 0,
            buckets: HashMap::new(),
            capped: 0,
        }
    }

    /// Whether a datagram from this (PID, remote IP, remote port) may be
    /// recorded at time `now_ms`. `cap_per_sec == 0` means "no cap": no bucket
    /// is created and this always returns `true`.
    pub fn allow(&mut self, pid: u32, ip: IpAddr, port: u16, now_ms: u64) -> bool {
        if self.cap_per_sec == 0 {
            return true;
        }
        self.sweep_if_due(now_ms);
        let bucket = self
            .buckets
            .entry((pid, ip, port))
            .or_insert_with(|| Bucket {
                count: 0,
                window_start: now_ms,
                last_used: now_ms,
            });
        bucket.last_used = now_ms;
        if now_ms.saturating_sub(bucket.window_start) >= 1000 {
            bucket.window_start = now_ms;
            bucket.count = 0;
        }
        if bucket.count >= self.cap_per_sec {
            self.capped += 1;
            return false;
        }
        bucket.count += 1;
        true
    }

    fn sweep_if_due(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_sweep) < SWEEP_INTERVAL_MS {
            return;
        }
        self.last_sweep = now_ms;
        self.buckets
            .retain(|_, b| now_ms.saturating_sub(b.last_used) <= IDLE_MS);
    }
}
