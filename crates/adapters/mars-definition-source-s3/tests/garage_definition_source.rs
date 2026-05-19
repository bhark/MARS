//! Round-trip a small `RenderDefinition` payload through `S3DefinitionSource::fetch`
//! against a real Garage container. Pins the put-then-fetch contract plus the
//! "bucket+key present -> revision is the bucket-side ETag" guarantee.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use bytes::Bytes;
use mars_definition_source::DefinitionSource;
use mars_definition_source_s3::{S3Credentials, S3DefinitionSource};
use mars_test_support::garage::boot_garage;
use object_store::ObjectStoreExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as OsPath;

const PAYLOAD: &[u8] = b"service: { name: roundtrip }\nlayers: []\n";
const KEY: &str = "defs/roundtrip.yaml";

async fn put_payload(g: &mars_test_support::garage::GarageFixture, body: Bytes) -> String {
    let backend = AmazonS3Builder::new()
        .with_bucket_name(&g.bucket)
        .with_region(&g.region)
        .with_endpoint(&g.endpoint)
        .with_access_key_id(&g.access_key)
        .with_secret_access_key(&g.secret_key)
        .with_allow_http(true)
        .build()
        .expect("build object_store backend");
    let path = OsPath::from(KEY);
    let outcome = backend.put(&path, body.into()).await.expect("put");
    outcome.e_tag.expect("garage returns etag")
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_round_trips_payload_and_carries_etag_revision() {
    let g = boot_garage().await;
    let etag = put_payload(&g, Bytes::from_static(PAYLOAD)).await;

    let src = S3DefinitionSource::new(
        Some(g.endpoint.clone()),
        g.region.clone(),
        g.bucket.clone(),
        KEY.into(),
        None,
        Some(S3Credentials {
            access_key: g.access_key.clone(),
            secret_key: g.secret_key.clone(),
            session_token: None,
        }),
    )
    .expect("adapter constructs");

    let got = src.fetch().await.expect("fetch ok");
    assert_eq!(got.data.as_ref(), PAYLOAD);

    // adapter strips wrapping quotes; raw put outcome may or may not include them.
    let expected = etag.trim_matches('"');
    assert_eq!(got.revision, expected, "revision must equal object ETag");
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_missing_key_yields_not_found() {
    use mars_definition_source::DefinitionSourceError;

    let g = boot_garage().await;
    let src = S3DefinitionSource::new(
        Some(g.endpoint.clone()),
        g.region.clone(),
        g.bucket.clone(),
        "does/not/exist.yaml".into(),
        None,
        Some(S3Credentials {
            access_key: g.access_key.clone(),
            secret_key: g.secret_key.clone(),
            session_token: None,
        }),
    )
    .expect("adapter constructs");

    let err = src.fetch().await.expect_err("missing key");
    assert!(matches!(err, DefinitionSourceError::NotFound { .. }), "{err:?}");
}
