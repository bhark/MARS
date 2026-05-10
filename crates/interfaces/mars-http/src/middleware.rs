use std::sync::atomic::Ordering;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

use crate::AppState;

pub async fn observe_request(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let interface = interface_label(req.uri().path());
    let start = Instant::now();
    let resp = next.run(req).await;
    state
        .metrics
        .observe_request(interface, resp.status().as_u16(), start.elapsed());
    resp
}

fn interface_label(path: &str) -> &'static str {
    // strict prefix match; anything outside the known set is bucketed as "other"
    // to keep cardinality flat regardless of probing/garbage requests.
    if path == "/healthz" {
        "health"
    } else if path == "/readyz" {
        "ready"
    } else if path == "/metrics" {
        "metrics"
    } else if path.starts_with("/wms") {
        "wms"
    } else if path.starts_with("/wmts") {
        "wmts"
    } else {
        "other"
    }
}

/// Hard cap on incoming `x-request-id`: long enough for UUIDs, short enough
/// to keep structured-log cardinality and per-line size bounded.
const REQUEST_ID_MAX_LEN: usize = 128;

pub fn request_id(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(v) = headers.get("x-request-id").and_then(|h| h.to_str().ok()) {
        // accept only printable ascii (plus space-equivalents) within the cap.
        // anything else falls back to a counter so a malicious client cannot
        // inject newlines into structured logs or blow the per-line budget.
        if (1..=REQUEST_ID_MAX_LEN).contains(&v.len()) && v.bytes().all(|b| matches!(b, 0x21..=0x7e)) {
            return v.to_owned();
        }
    }
    let n = state.request_counter.fetch_add(1, Ordering::Relaxed);
    format!("req-{n}")
}
