use std::sync::Arc;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mars_runtime::{RenderPlan, Runtime, RuntimeError};
use mars_wms::{ExceptionsFormat, WmsError, WmsVersion};
use mars_wmts::WmtsError;

/// Service-agnostic exception payload. The same fields drive both the WMS
/// `ServiceExceptionReport` and the OWS `ExceptionReport` envelopes; only the
/// XML wrapping differs.
///
/// `code` is optional for WMS (omitted attribute) but required by OWS, where
/// `None` is rendered as `"NoApplicableCode"` per OWS Annex A.
struct EdgeException {
    status: StatusCode,
    code: Option<&'static str>,
    /// OWS `locator` attribute. Ignored by the WMS emitter.
    locator: Option<&'static str>,
    message: String,
}

fn wms_exception_response(version: WmsVersion, exc: EdgeException) -> Response {
    let xml = mars_wms::service_exception_report(version, exc.code, &exc.message);
    let mut resp = (exc.status, xml).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/xml; charset=utf-8"),
    );
    resp
}

pub fn wms_error_response(version: WmsVersion, e: WmsError) -> Response {
    let exc = match e {
        WmsError::MissingParam(name) => EdgeException {
            status: StatusCode::BAD_REQUEST,
            code: Some("MissingParameterValue"),
            locator: Some(name),
            message: format!("Missing required parameter: {name}"),
        },
        WmsError::InvalidParam { name, reason } => EdgeException {
            status: StatusCode::BAD_REQUEST,
            code: Some("InvalidParameterValue"),
            locator: Some(name),
            message: format!("Invalid parameter '{name}': {reason}"),
        },
        WmsError::NotImplemented { what } => EdgeException {
            status: StatusCode::NOT_IMPLEMENTED,
            code: Some("OperationNotSupported"),
            locator: None,
            message: format!("Operation not supported: {what}"),
        },
        WmsError::OperationNotPermitted { layer, op } => EdgeException {
            status: StatusCode::FORBIDDEN,
            code: Some("OperationNotSupported"),
            locator: None,
            message: format!("Operation {} not permitted on layer '{layer}'", op.as_str()),
        },
    };
    wms_exception_response(version, exc)
}

/// XML-only WMS error response for paths that have no EXCEPTIONS= contract
/// (GetFeatureInfo, GetLegendGraphic). Mirrors `runtime_error_response` but
/// without the BLANK image branch.
pub fn wms_runtime_xml_response(version: WmsVersion, e: RuntimeError, plan: &RenderPlan) -> Response {
    log_render_failure(&e, plan);
    wms_exception_response(version, map_runtime_error(&e))
}

/// Plan-less XML error response. Used by GetLegendGraphic, which has no
/// RenderPlan; logs the op name instead of layer/bbox.
pub fn wms_runtime_xml_response_plain(version: WmsVersion, e: RuntimeError, op: &'static str) -> Response {
    match &e {
        RuntimeError::NotReady => tracing::warn!(error = %e, op, "wms op failed"),
        _ => tracing::error!(error = %e, op, "wms op failed"),
    }
    wms_exception_response(version, map_runtime_error(&e))
}

pub fn runtime_error_response(
    version: WmsVersion,
    e: RuntimeError,
    plan: &RenderPlan,
    exceptions: ExceptionsFormat,
    runtime: &Arc<Runtime>,
) -> Response {
    log_render_failure(&e, plan);
    match exceptions {
        ExceptionsFormat::Xml => wms_exception_response(version, map_runtime_error(&e)),
        ExceptionsFormat::Blank => match runtime.blank_image(plan) {
            Ok(bytes) => image_response(plan.format.mime(), bytes),
            // last-resort fallback: if the encoder fails for some bizarre
            // reason, surface as XML rather than a zero-byte image. the
            // operator gets a proper signal.
            Err(encode_err) => wms_exception_response(version, map_runtime_error(&encode_err)),
        },
        ExceptionsFormat::Inimage => {
            let message = map_runtime_error(&e).message;
            match runtime.inimage_error(plan, &message) {
                Ok(bytes) => image_response(plan.format.mime(), bytes),
                Err(render_err) => wms_exception_response(version, map_runtime_error(&render_err)),
            }
        }
    }
}

fn image_response(mime: &'static str, bytes: Vec<u8>) -> Response {
    let mut h = axum::http::HeaderMap::new();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    (StatusCode::OK, h, bytes).into_response()
}

fn wmts_exception_response(exc: EdgeException) -> Response {
    let xml = mars_wmts::ows_exception_report(exc.code.unwrap_or("NoApplicableCode"), exc.locator, &exc.message);
    let mut resp = (exc.status, xml).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/xml; charset=utf-8"),
    );
    resp
}

pub fn wmts_error_response(e: WmtsError) -> Response {
    let exc = match e {
        WmtsError::MissingParam(name) => EdgeException {
            status: StatusCode::BAD_REQUEST,
            code: Some("MissingParameterValue"),
            locator: Some(name),
            message: format!("Missing required parameter: {name}"),
        },
        WmtsError::InvalidParam { name, reason } => EdgeException {
            status: StatusCode::BAD_REQUEST,
            code: Some("InvalidParameterValue"),
            locator: Some(name),
            message: format!("Invalid parameter '{name}': {reason}"),
        },
        WmtsError::NotImplemented { what } => EdgeException {
            status: StatusCode::NOT_IMPLEMENTED,
            code: Some("OperationNotSupported"),
            locator: None,
            message: format!("Operation not supported: {what}"),
        },
        WmtsError::OperationNotPermitted { layer, op } => EdgeException {
            status: StatusCode::FORBIDDEN,
            code: Some("OperationNotSupported"),
            locator: None,
            message: format!("Operation {} not permitted on layer '{layer}'", op.as_str()),
        },
    };
    wmts_exception_response(exc)
}

pub fn wmts_runtime_error_response(e: RuntimeError, plan: &RenderPlan) -> Response {
    log_render_failure(&e, plan);
    wmts_exception_response(map_runtime_error(&e))
}

fn log_render_failure(e: &RuntimeError, plan: &RenderPlan) {
    match e {
        RuntimeError::NotReady => {
            tracing::warn!(error = %e, layers = ?plan.layers, bbox = ?plan.bbox, "render failed")
        }
        _ => {
            tracing::error!(error = %e, layers = ?plan.layers, bbox = ?plan.bbox, "render failed")
        }
    }
}

fn map_runtime_error(e: &RuntimeError) -> EdgeException {
    match e {
        RuntimeError::NotReady => EdgeException {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: None,
            locator: None,
            message: "Service temporarily unavailable".into(),
        },
        RuntimeError::NotImplemented { what } => EdgeException {
            status: StatusCode::NOT_IMPLEMENTED,
            code: Some("OperationNotSupported"),
            locator: None,
            message: format!("Operation not supported: {what}"),
        },
        RuntimeError::LayerNotDefined { layer } => EdgeException {
            status: StatusCode::BAD_REQUEST,
            code: Some("LayerNotDefined"),
            locator: Some("LAYERS"),
            message: format!("Layer '{layer}' is not defined"),
        },
        RuntimeError::PixelBudgetExceeded { requested, budget } => EdgeException {
            status: StatusCode::BAD_REQUEST,
            code: Some("InvalidParameterValue"),
            locator: None,
            message: format!("Request requires {requested} pixels but server budget is {budget}"),
        },
        RuntimeError::Config(_)
        | RuntimeError::Store(_)
        | RuntimeError::Render(_)
        | RuntimeError::Encode(_)
        | RuntimeError::InvalidManifest { .. }
        | RuntimeError::ConfigManifestMismatch { .. }
        | RuntimeError::StylesheetDrift { .. }
        | RuntimeError::RasterSourceNotRegistered { .. }
        | RuntimeError::RasterSource(_) => internal_error(),
    }
}

fn internal_error() -> EdgeException {
    EdgeException {
        status: StatusCode::INTERNAL_SERVER_ERROR,
        code: None,
        locator: None,
        message: "Internal server error".into(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use mars_config::ServiceOp;
    use mars_types::LayerId;

    use super::*;

    #[tokio::test]
    async fn operation_not_permitted_maps_to_403_with_operation_not_supported() {
        let e = mars_wms::WmsError::OperationNotPermitted {
            layer: LayerId::new("roads"),
            op: ServiceOp::WmsGetMap,
        };
        let resp = wms_error_response(WmsVersion::V130, e);
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let body = std::str::from_utf8(&bytes).unwrap();
        assert!(body.contains("ServiceException"));
        assert!(body.contains("code=\"OperationNotSupported\""));
        assert!(body.contains("roads"));
    }
}
