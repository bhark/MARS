#![allow(clippy::unwrap_used)]

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
