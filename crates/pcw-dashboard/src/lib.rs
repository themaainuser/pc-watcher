//! Dashboard server (plan Task 14): a loopback-only HTTP server that serves
//! the embedded single-file UI and read-only JSON endpoints over the storage
//! queries.
//!
//! Security posture (spec §5.2, F16a):
//! - binds `127.0.0.1` only, never `0.0.0.0`/`[::]`;
//! - the store is opened **read-only** — the agent owns the writer, and WAL
//!   plus the 5 s busy timeout make concurrent reads safe;
//! - the store is opened lazily and the open is retried on every data request
//!   until it succeeds (the agent may not be running yet);
//! - `POST /api/shutdown` is the only mutating route, and it only flips the
//!   caller's stop flag; the static route is exactly `/`.
//!
//! The store is opened once per server run ("lazy-once"), never held across
//! requests in a transaction; every query is its own short read transaction.
//! If a query fails, the cached store is dropped so the next request retries
//! the open — an empty-but-present file (agent still initializing) recovers
//! the same way a missing one does. `run` is single-threaded, matching the
//! `!Send` store; the local handle lives only inside `run`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use pcw_storage::Store;
use thiserror::Error;

pub mod export;
pub mod routes;

pub use routes::Resp;

/// How long the run loop waits inside one `recv_timeout` slice before
/// re-checking the stop flag.
const POLL_SLICE: Duration = Duration::from_millis(250);

#[derive(Debug, Error)]
pub enum DashError {
    /// The dashboard port is already taken (another dashboard is likely
    /// running there; the caller may just open a browser to it).
    #[error("dashboard port already in use")]
    PortInUse,
    /// Binding failed for any other reason.
    #[error("cannot bind loopback dashboard: {0}")]
    Bind(String),
    /// A storage failure while opening or reading the database.
    ///
    /// Currently unreachable from [`DashServer::run`]: storage failures in
    /// the request path are answered with an error response inside the route
    /// layer and never bubble out of `run`. Kept as a specced interface
    /// (plan T14) for callers that want the typed conversion.
    #[error(transparent)]
    Storage(#[from] pcw_storage::StorageError),
}

/// A loopback dashboard server. Create with [`DashServer::bind`], then
/// [`DashServer::run`] on a dedicated thread.
pub struct DashServer {
    server: tiny_http::Server,
}

impl DashServer {
    /// Binds to loopback only — `tiny_http::Server::http(("127.0.0.1", port))`,
    /// never `0.0.0.0` or `[::]`. Port 0 lets the OS pick an ephemeral port.
    pub fn bind(port: u16) -> Result<DashServer, DashError> {
        let server = tiny_http::Server::http(("127.0.0.1", port));
        match server {
            Ok(server) => Ok(DashServer { server }),
            Err(err) => Err(match err.downcast_ref::<std::io::Error>() {
                Some(io_err) if io_err.kind() == std::io::ErrorKind::AddrInUse => {
                    DashError::PortInUse
                }
                _ => DashError::Bind(err.to_string()),
            }),
        }
    }

    /// The loopback address the server is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.server
            .server_addr()
            .to_ip()
            .expect("DashServer always binds an IP address")
    }

    /// Serves requests until the shared `stop` flag is set, until
    /// `POST /api/shutdown`, or until `idle_timeout` elapses with no request.
    ///
    /// Opens the read-only store lazily on the first data request and retries
    /// until it succeeds, so the server can come up before the agent has ever
    /// created the database.
    pub fn run(
        self,
        db_path: PathBuf,
        stop: Arc<AtomicBool>,
        idle_timeout: Duration,
    ) -> Result<(), DashError> {
        let mut store: Option<Store> = None;
        let mut last_activity = std::time::Instant::now();
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let idle = last_activity.elapsed();
            if idle >= idle_timeout {
                return Ok(());
            }
            let slice = (idle_timeout - idle).min(POLL_SLICE);
            match self.server.recv_timeout(slice) {
                Ok(Some(request)) => {
                    handle_one(request, &db_path, &mut store, &stop);
                    last_activity = std::time::Instant::now();
                }
                // Slice elapsed without a request: loop re-checks stop/idle.
                Ok(None) => {}
                // One malformed request; not fatal for the server.
                Err(_) => last_activity = std::time::Instant::now(),
            }
        }
    }
}

/// Reads one request, routes it and writes the response. Response delivery
/// happens here, so `POST /api/shutdown` has answered 200 before `run`
/// returns on the next loop iteration (`stop` gets set by the route).
fn handle_one(
    request: tiny_http::Request,
    db_path: &Path,
    store: &mut Option<Store>,
    stop: &Arc<AtomicBool>,
) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let resp = routes::handle(&method, &url, db_path, store, stop);
    let mut response = tiny_http::Response::from_data(resp.body).with_status_code(resp.status);
    if let Ok(header) = tiny_http::Header::from_bytes(&b"Content-Type"[..], resp.content_type) {
        response.add_header(header);
    }
    if let Some(disposition) = &resp.content_disposition {
        if let Ok(header) =
            tiny_http::Header::from_bytes(&b"Content-Disposition"[..], disposition.as_bytes())
        {
            response.add_header(header);
        }
    }
    let _ = request.respond(response);
}
