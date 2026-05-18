#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[tokio::test]
async fn new_rejects_empty_cache_dir() {
    let err = VectorFileSource::new(
        mars_config::SourceId::new("vf"),
        CrsCode::new("EPSG:25832"),
        mars_config::VectorFileBackend {
            cache_dir: String::new(),
            ..Default::default()
        },
    )
    .await
    .expect_err("empty cache_dir must reject");
    assert!(matches!(err, VectorFileError::InvalidConfig { .. }));
}

#[tokio::test]
async fn new_succeeds_on_valid_config() {
    let tmp = tempfile::tempdir().unwrap();
    let src = VectorFileSource::new(
        mars_config::SourceId::new("vf"),
        CrsCode::new("EPSG:25832"),
        mars_config::VectorFileBackend {
            cache_dir: tmp.path().display().to_string(),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(src.id().as_str(), "vf");
    assert_eq!(src.native_crs().as_str(), "EPSG:25832");
}
