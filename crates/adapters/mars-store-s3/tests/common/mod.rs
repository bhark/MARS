//! shared fixtures for the garage-backed integration tests. each test file
//! pulls this via `#[path = "common/mod.rs"] mod common;` so it doesn't trip
//! the implicit-mod unused warning when individual fns aren't called by
//! every file.

use mars_store_s3::{S3Config, S3Publisher, S3Store};
use mars_test_support::garage::GarageFixture;

/// build an `S3Config` pointing at the garage fixture's bucket. defaults
/// match the AWS-equivalent codepath (etag-based CAS enabled,
/// allow_non_atomic_publish off). callers wanting the Garage/SeaweedFS
/// fallback path adjust via `with_non_atomic`.
pub(crate) fn s3_config_from_garage(g: &GarageFixture, prefix: &str) -> S3Config {
    S3Config {
        endpoint: Some(g.endpoint.clone()),
        region: g.region.clone(),
        bucket: g.bucket.clone(),
        prefix: prefix.to_owned(),
        access_key_id: Some(g.access_key.clone()),
        secret_access_key: Some(g.secret_key.clone()),
        allow_http: true,
        allow_non_atomic_publish: false,
        conditional_put: None,
    }
}

/// the operator-configured fallback shape: `conditional_put = disabled`
/// (client never sends If-Match / If-None-Match) and
/// `allow_non_atomic_publish = true` (publisher overwrites on
/// NotSupported/NotImplemented). this is the codepath Garage and SeaweedFS
/// deployments take in production.
#[allow(dead_code)]
pub(crate) fn s3_config_non_atomic_from_garage(g: &GarageFixture, prefix: &str) -> S3Config {
    let mut cfg = s3_config_from_garage(g, prefix);
    cfg.conditional_put = Some("disabled".into());
    cfg.allow_non_atomic_publish = true;
    cfg
}

#[allow(dead_code)]
pub(crate) fn s3_store_from_config(cfg: &S3Config) -> S3Store {
    S3Store::from_config(cfg).expect("s3 store from config")
}

#[allow(dead_code)]
pub(crate) fn s3_publisher_from_store(store: &S3Store, allow_non_atomic: bool) -> S3Publisher {
    S3Publisher::from_store(store).with_allow_non_atomic_publish(allow_non_atomic)
}
