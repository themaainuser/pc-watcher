//! Route handlers (plan Task 14): turn one HTTP request into a [`Resp`].
//!
//! Splitting them off `lib.rs` keeps the server loop free of URL plumbing
//! and lets [`parse_filter`] stay pure (unit-testable without the server).
//! Every handler is a short-lived function over a `&Store`: no state is
//! kept between requests (F16a), every list query is bounded by an explicit
//! limit, and no query selects unbounded rows into JSON.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pcw_storage::{ConnFilter, Store};
use pcw_types::{Direction, Protocol};
use tiny_http::Method;

/// The embedded single-file dashboard UI. Vanilla JS, zero external
/// resources — the only URL-like string in the file is the SVG namespace
/// (`ui_has_no_external_urls` enforces this).
pub const UI_HTML: &str = include_str!("ui.html");

/// Exact 503 body mandated by the route table (written verbatim, space
/// after the colon included).
const NOT_READY_BODY: &str = r#"{"error": "no data yet — is pcw-agent running?"}"#;

/// Default and maximum row caps for the bounded list endpoints.
const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 1_000;

/// One complete HTTP response: bytes chosen by the route.
pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

fn json_resp(status: u16, body: Vec<u8>) -> Resp {
    Resp {
        status,
        content_type: "application/json",
        body,
    }
}

/// A JSON error body carrying a helpful message.
fn err_resp(status: u16, message: &str) -> Resp {
    let body = serde_json::to_vec(&serde_json::json!({ "error": message }))
        .expect("error JSON cannot fail to serialize");
    json_resp(status, body)
}

/// The 503 shown until the agent's database exists.
fn not_ready_resp() -> Resp {
    Resp {
        status: 503,
        content_type: "application/json",
        body: NOT_READY_BODY.as_bytes().to_vec(),
    }
}

/// The embedded UI page.
fn ui_resp() -> Resp {
    Resp {
        status: 200,
        content_type: "text/html; charset=utf-8",
        body: UI_HTML.as_bytes().to_vec(),
    }
}

/// Routes one request. Params are parsed *before* touching the store, so a
/// bad filter answers 400 even while the database is unavailable.
pub fn handle(
    method: &Method,
    url: &str,
    db_path: &Path,
    store: &mut Option<Store>,
    stop: &Arc<AtomicBool>,
) -> Resp {
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, query),
        None => (url, ""),
    };
    match (method, path) {
        (Method::Get, "/") => ui_resp(),
        (Method::Post, "/api/shutdown") => {
            stop.store(true, Ordering::Relaxed);
            let body = serde_json::to_vec(&serde_json::json!({ "ok": true }))
                .expect("shutdown JSON cannot fail to serialize");
            json_resp(200, body)
        }
        (Method::Get, "/api/overview" | "/api/app" | "/api/service" | "/api/usage") => {
            data_route(path, query, db_path, store)
        }
        // Unknown paths and wrong methods on known paths both 404.
        _ => err_resp(404, "not found"),
    }
}

/// The four data endpoints: parse parameters, lazily open the store, run one
/// bounded query. Any storage failure (missing file, agent still creating
/// the schema) becomes the retryable 503 and drops the cached store.
fn data_route(path: &str, query: &str, db_path: &Path, slot: &mut Option<Store>) -> Resp {
    let filter = match parse_filter(query) {
        Ok(filter) => filter,
        Err(msg) => return err_resp(400, &format!("invalid filter: {msg}")),
    };
    let params = query_params(query);

    let Some(store) = open_store(db_path, slot) else {
        return not_ready_resp();
    };

    let result = match path {
        "/api/overview" => route_overview(store, &filter),
        "/api/app" => route_app(store, &filter, &params),
        "/api/service" => route_service(store, &filter, &params),
        _ => route_usage(store, &filter),
    };
    match result {
        Ok(resp) => resp,
        Err(_) => {
            // Drop the cached store so the next request retries the open —
            // e.g. an empty file the agent has not finished initializing.
            slot.take();
            not_ready_resp()
        }
    }
}

/// Lazily opens and caches the read-only store; retries on every data
/// request until it succeeds. Never opens read-write (the agent holds the
/// writer; WAL + the 5 s busy timeout make concurrent reads safe).
fn open_store<'a>(db_path: &Path, slot: &'a mut Option<Store>) -> Option<&'a Store> {
    if slot.is_none() {
        *slot = Store::open_readonly(db_path).ok();
    }
    slot.as_ref()
}

fn route_overview(store: &Store, filter: &ConnFilter) -> Result<Resp, pcw_storage::StorageError> {
    let summary = store.overview(filter)?;
    let body = serde_json::to_vec(&summary).expect("overview JSON cannot fail to serialize");
    Ok(json_resp(200, body))
}

/// `GET /api/app?name=…&limit=…&<filter>` →
/// `{"destinations", "connections", "usage", "titles"}` — the four keys are
/// fixed, `name` overrides any `app` filter parameter.
fn route_app(
    store: &Store,
    filter: &ConnFilter,
    params: &[(String, String)],
) -> Result<Resp, pcw_storage::StorageError> {
    let Some(name) = param(params, "name").filter(|n| !n.is_empty()) else {
        return Ok(err_resp(400, "missing required parameter: name"));
    };
    let limit = match parse_limit(params) {
        Ok(limit) => limit,
        Err(msg) => return Ok(err_resp(400, &msg)),
    };
    let mut drill = filter.clone();
    drill.app = Some(name.to_string());

    let destinations = store.app_destinations(&drill)?;
    let connections = store.connections(&drill, limit)?;
    let usage = store.usage_summary(name, drill.from_ms, drill.to_ms)?;
    let titles = store.titles(drill.from_ms, drill.to_ms, Some(name))?;
    let body = serde_json::to_vec(&serde_json::json!({
        "destinations": destinations,
        "connections": connections,
        "usage": usage,
        "titles": titles,
    }))
    .expect("app drill-down JSON cannot fail to serialize");
    Ok(json_resp(200, body))
}

/// `GET /api/service?key=…&limit=…&<filter>` → the connections to that
/// hostname/IP, newest first — the rows carry the app names, so the UI can
/// show *which apps talked to it and when*.
fn route_service(
    store: &Store,
    filter: &ConnFilter,
    params: &[(String, String)],
) -> Result<Resp, pcw_storage::StorageError> {
    let Some(key) = param(params, "key").filter(|k| !k.is_empty()) else {
        return Ok(err_resp(400, "missing required parameter: key"));
    };
    let limit = match parse_limit(params) {
        Ok(limit) => limit,
        Err(msg) => return Ok(err_resp(400, &msg)),
    };
    let connections = store.service_connections(key, filter, limit)?;
    let body = serde_json::to_vec(&serde_json::json!({ "connections": connections }))
        .expect("service JSON cannot fail to serialize");
    Ok(json_resp(200, body))
}

/// `GET /api/usage?from&to&app` → `{"sessions", "titles"}`: the foreground
/// timeline and the title log for the same range.
fn route_usage(store: &Store, filter: &ConnFilter) -> Result<Resp, pcw_storage::StorageError> {
    let sessions = store.sessions(filter.from_ms, filter.to_ms, filter.app.as_deref())?;
    let titles = store.titles(filter.from_ms, filter.to_ms, filter.app.as_deref())?;
    let body = serde_json::to_vec(&serde_json::json!({
        "sessions": sessions,
        "titles": titles,
    }))
    .expect("usage JSON cannot fail to serialize");
    Ok(json_resp(200, body))
}

/// Parses the bounded list cap: `limit=<u32>`, clamped to `[1, MAX_LIMIT]`,
/// defaulting to [`DEFAULT_LIMIT`].
fn parse_limit(params: &[(String, String)]) -> Result<u32, String> {
    let Some(raw) = param(params, "limit") else {
        return Ok(DEFAULT_LIMIT);
    };
    let limit: u32 = raw
        .parse()
        .map_err(|_| format!("limit must be a positive integer, got {raw:?}"))?;
    Ok(limit.clamp(1, MAX_LIMIT))
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .rev()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// Splits a query string into decoded `(key, value)` pairs.
fn query_params(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|item| !item.is_empty())
        .filter_map(|item| {
            let (key, value) = item.split_once('=')?;
            Some((decode(key).ok()?, decode(value).ok()?))
        })
        .collect()
}

/// Parses the shared filter parameters: `from`, `to` (epoch ms), `app`,
/// `protocol` (`tcp`|`udp`), `port` (u16), `direction` (`out`|`in`). An
/// empty or absent query yields the all-connections default filter; an
/// unknown value is `Err`, which the server turns into a 400. Pure — no
/// server required to test it.
pub fn parse_filter(query: &str) -> Result<ConnFilter, String> {
    let mut filter = ConnFilter::default();
    for item in query.split('&') {
        if item.is_empty() {
            continue;
        }
        let (key, value) = item
            .split_once('=')
            .ok_or_else(|| format!("query item {item:?} has no '='"))?;
        match decode(key)?.as_str() {
            "from" => {
                let raw = decode(value)?;
                filter.from_ms = raw
                    .parse::<u64>()
                    .map_err(|_| format!("from must be a u64 epoch-ms number, got {raw:?}"))?;
            }
            "to" => {
                let raw = decode(value)?;
                filter.to_ms = raw
                    .parse::<u64>()
                    .map_err(|_| format!("to must be a u64 epoch-ms number, got {raw:?}"))?;
            }
            "app" => filter.app = Some(decode(value)?),
            "protocol" => {
                let raw = decode(value)?;
                filter.protocol = Some(
                    Protocol::parse(&raw)
                        .ok_or_else(|| format!("protocol must be tcp or udp, got {raw:?}"))?,
                );
            }
            "port" => {
                let raw = decode(value)?;
                filter.port = Some(
                    raw.parse::<u16>()
                        .map_err(|_| format!("port must be a u16 (0-65535), got {raw:?}"))?,
                );
            }
            "direction" => {
                let raw = decode(value)?;
                filter.direction = Some(
                    Direction::parse(&raw)
                        .ok_or_else(|| format!("direction must be out or in, got {raw:?}"))?,
                );
            }
            // Route-specific params (name, limit, key) are not filter terms.
            _ => {}
        }
    }
    Ok(filter)
}

/// Percent-decodes one query component (`+` means space, `%XX` a byte).
/// Bytes belonging to multi-byte UTF-8 characters pass through untouched, so
/// the final UTF-8 validation is what makes the result trustworthy.
fn decode(component: &str) -> Result<String, String> {
    let bytes = component.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes
                    .get(i + 1..i + 3)
                    .and_then(|h| std::str::from_utf8(h).ok())
                    .ok_or_else(|| format!("incomplete % escape in {component:?}"))?;
                let byte = u8::from_str_radix(hex, 16)
                    .map_err(|_| format!("bad % escape in {component:?}"))?;
                out.push(byte);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| format!("query component {component:?} is not UTF-8"))
}
