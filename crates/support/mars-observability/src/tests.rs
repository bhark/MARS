#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn metrics_round_trip() {
    let m = Metrics::new().unwrap();
    m.observe_request("wms", 200, Duration::from_millis(12));
    m.set_manifest_version(42);
    m.inc_manifest_reject(reject_reason::BACKWARDS_VERSION);
    m.inc_compiler_change_events();
    m.inc_compiler_dirty_cells(7);
    m.observe_compiler_rebuild_duration(Duration::from_secs_f64(1.23));
    m.set_compiler_window_lag(Duration::from_secs_f64(0.5));
    m.inc_render_feature_unstyled("Bygning", 3);
    m.inc_compiler_features_unmatched("buildings_live", 5);
    m.inc_adapter_error(adapter::POSTGRES, "fetch_cells");
    m.inc_adapter_error(adapter::STORE_FS, "open");
    let text = m.encode_text().unwrap();
    assert!(text.contains("mars_request_total"));
    assert!(text.contains("interface=\"wms\""));
    assert!(text.contains("status=\"2xx\""));
    assert!(text.contains("mars_request_duration_seconds"));
    assert!(text.contains("mars_manifest_version 42"));
    assert!(text.contains("mars_manifest_reject_total"));
    assert!(text.contains("reason=\"backwards_version\""));
    assert!(text.contains("mars_compiler_change_events_total"));
    assert!(text.contains("mars_compiler_dirty_cells_total"));
    assert!(text.contains("mars_compiler_rebuild_duration_seconds"));
    assert!(text.contains("mars_compiler_window_lag_seconds"));
    assert!(text.contains("mars_render_feature_unstyled_total"));
    assert!(text.contains("layer=\"Bygning\""));
    assert!(text.contains("mars_compiler_features_unmatched_total"));
    assert!(text.contains("binding=\"buildings_live\""));
    assert!(text.contains("mars_adapter_error_total"));
    assert!(text.contains("adapter=\"postgres\""));
    assert!(text.contains("kind=\"fetch_cells\""));
    assert!(text.contains("adapter=\"store_fs\""));
}
