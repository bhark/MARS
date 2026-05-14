use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tracing::Instrument;

use mars_wms::WmsRequest;
use mars_wmts::WmtsRequest;

use crate::{AppState, CapabilitiesHandle};
use crate::{
    request_id, runtime_error_response, wms_error_response, wms_runtime_xml_response, wms_runtime_xml_response_plain,
    wmts_error_response, wmts_runtime_error_response,
};

pub async fn handle_wms(State(state): State<AppState>, headers: HeaderMap, raw_query: RawQuery) -> Response {
    let req_id = request_id(&state, &headers);
    let span = tracing::info_span!("wms", req_id = %req_id);

    async move {
        let raw = raw_query.0.unwrap_or_default();

        let (version, parsed) = match mars_wms::parse_request(&raw, &state.wms_cfg) {
            Ok(r) => r,
            // recover the version from the raw query so the error response
            // is tagged with the version the client appeared to request.
            Err(e) => return wms_error_response(mars_wms::version_for_error_response(&raw), e),
        };

        match parsed {
            WmsRequest::GetCapabilities => serve_capabilities(state.wms_capabilities.for_version(version), &headers),
            WmsRequest::GetMap(mars_wms::ResolvedGetMap { plan, exceptions }) => {
                let mime = plan.format.mime();
                match state.runtime.render(&plan).await {
                    Ok(bytes) => {
                        let mut h = HeaderMap::new();
                        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                        (StatusCode::OK, h, bytes).into_response()
                    }
                    Err(e) => runtime_error_response(version, e, &plan, exceptions, &state.runtime),
                }
            }
            WmsRequest::GetLegendGraphic(mars_wms::ResolvedGetLegend { plan }) => {
                let mime = plan.format.mime();
                match state.runtime.render_legend(&plan) {
                    Ok(bytes) => {
                        let mut h = HeaderMap::new();
                        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                        (StatusCode::OK, h, bytes).into_response()
                    }
                    Err(e) => wms_runtime_xml_response_plain(version, e, "GetLegendGraphic"),
                }
            }
            WmsRequest::GetFeatureInfo(gfi) => {
                let mime = gfi.info_format.mime();
                match state.runtime.get_feature_info(&gfi.plan, (gfi.i, gfi.j)).await {
                    Ok(hits) => {
                        let count = gfi.feature_count as usize;
                        let trimmed: Vec<_> = hits.into_iter().take(count).collect();
                        let body = mars_wms::format_feature_info(&trimmed, gfi.info_format);
                        let mut h = HeaderMap::new();
                        // mime strings carry charset where applicable; the
                        // gfi formatter contract pre-sets it for text/*.
                        match HeaderValue::from_str(mime) {
                            Ok(v) => {
                                h.insert(header::CONTENT_TYPE, v);
                            }
                            Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "bad mime").into_response(),
                        }
                        (StatusCode::OK, h, body).into_response()
                    }
                    Err(e) => wms_runtime_xml_response(version, e, &gfi.plan),
                }
            }
        }
    }
    .instrument(span)
    .await
}

pub async fn handle_wmts(State(state): State<AppState>, headers: HeaderMap, raw_query: RawQuery) -> Response {
    let req_id = request_id(&state, &headers);
    let span = tracing::info_span!("wmts", req_id = %req_id);

    async move {
        let raw = raw_query.0.unwrap_or_default();

        let parsed = match mars_wmts::parse_request(&raw, &state.wmts_cfg) {
            Ok(r) => r,
            Err(e) => return wmts_error_response(e),
        };

        match parsed {
            WmtsRequest::GetCapabilities => serve_capabilities(&state.wmts_capabilities, &headers),
            WmtsRequest::GetTile(mars_wmts::ResolvedGetTile { plan }) => {
                let mime = plan.format.mime();
                match state.runtime.render(&plan).await {
                    Ok(bytes) => {
                        let mut h = HeaderMap::new();
                        h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                        (StatusCode::OK, h, bytes).into_response()
                    }
                    Err(e) => wmts_runtime_error_response(e, &plan),
                }
            }
        }
    }
    .instrument(span)
    .await
}

/// REST-style WMTS GetTile entry point.
///
/// URL template: `/wmts/{Layer}/{Style}/{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.{ext}`.
/// Extension lives on the final path segment and selects the output format.
pub async fn handle_wmts_rest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((layer, style, tms, z, y, x_ext)): Path<(String, String, String, String, String, String)>,
) -> Response {
    let req_id = request_id(&state, &headers);
    let span = tracing::info_span!("wmts_rest", req_id = %req_id);

    async move {
        // split `x.ext` on the final `.` so layer-name dots stay untouched.
        let (x, ext) = match x_ext.rsplit_once('.') {
            Some((x, ext)) if !x.is_empty() && !ext.is_empty() => (x, ext),
            _ => {
                return wmts_error_response(mars_wmts::WmtsError::InvalidParam {
                    name: "format",
                    reason: format!("expected `<TileCol>.<ext>`, got `{x_ext}`"),
                });
            }
        };

        let plan = match mars_wmts::parse_rest_get_tile(&layer, &style, &tms, &z, &y, x, ext, &state.wmts_cfg) {
            Ok(p) => p,
            Err(e) => return wmts_error_response(e),
        };
        let mime = plan.format.mime();
        match state.runtime.render(&plan).await {
            Ok(bytes) => {
                let mut h = HeaderMap::new();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                (StatusCode::OK, h, bytes).into_response()
            }
            Err(e) => wmts_runtime_error_response(e, &plan),
        }
    }
    .instrument(span)
    .await
}

fn serve_capabilities(handle: &CapabilitiesHandle, headers: &HeaderMap) -> Response {
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

pub async fn handle_ready(State(state): State<AppState>) -> Response {
    if state.runtime.is_ready() {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no manifest").into_response()
    }
}

pub async fn handle_metrics(State(state): State<AppState>) -> Response {
    match state.metrics.encode_text() {
        Ok(body) => {
            let mut h = HeaderMap::new();
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; version=0.0.4"),
            );
            (StatusCode::OK, h, body).into_response()
        }
        Err(e) => {
            tracing::error!(error = %e, "metrics encode failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "metrics encode failed").into_response()
        }
    }
}

pub fn etag_for(bytes: &[u8]) -> String {
    let hash = blake3::hash(bytes);
    // strong validator, hex-truncated to 16 chars (64 bits) - collision-safe for caps.
    format!("\"{}\"", &hash.to_hex().as_str()[..16])
}
