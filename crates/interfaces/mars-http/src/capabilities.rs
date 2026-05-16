//! cached capabilities-document serving with ETag negotiation.

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::CapabilitiesHandle;

pub(crate) fn serve_capabilities(handle: &CapabilitiesHandle, headers: &HeaderMap) -> Response {
    let doc = handle.load_full();
    let etag_value = match HeaderValue::from_str(&doc.etag) {
        Ok(v) => v,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "bad etag").into_response(),
    };
    if let Some(req_etag) = headers.get(header::IF_NONE_MATCH)
        && *req_etag == etag_value
    {
        let mut h = HeaderMap::new();
        h.insert(header::ETAG, etag_value);
        return (StatusCode::NOT_MODIFIED, h).into_response();
    }
    let mut h = HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/xml"));
    h.insert(header::ETAG, etag_value);
    (StatusCode::OK, h, doc.body.clone()).into_response()
}
