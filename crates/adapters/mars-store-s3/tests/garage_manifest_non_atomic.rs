//! Garage/SeaweedFS production codepath: `conditional_put = "disabled"` +
//! `allow_non_atomic_publish = true`. proves that the manifest body and
//! pointer round-trip correctly when the backend cannot CAS, and that the
//! opt-in guardrail rejects the unsafe configuration if the operator
//! forgets to enable the fallback.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use mars_store::{ManifestStore, StoreError};
use mars_store_s3::S3Publisher;
use mars_test_support::garage::boot_garage;
use mars_types::Manifest;

use crate::common::{
    s3_config_from_garage, s3_config_non_atomic_from_garage, s3_publisher_from_store, s3_store_from_config,
};

#[tokio::test(flavor = "multi_thread")]
async fn publish_v1_then_v2_via_non_atomic_path_swaps_current() {
    let g = boot_garage().await;
    let cfg = s3_config_non_atomic_from_garage(&g, "nat1");
    let store = s3_store_from_config(&cfg);
    let publisher = s3_publisher_from_store(&store, true);

    let v1 = Manifest::empty(1, "test");
    publisher.publish(&v1).await.unwrap();
    let cur = publisher.current().await.unwrap().expect("v1");
    assert_eq!(cur.version, 1);

    let v2 = Manifest::empty(2, "test");
    publisher.publish(&v2).await.unwrap();
    let cur = publisher.current().await.unwrap().expect("v2");
    assert_eq!(cur.version, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_requires_allow_non_atomic_when_conditional_put_disabled() {
    let g = boot_garage().await;
    // conditional_put = disabled, but allow_non_atomic_publish stays off.
    // the publisher must refuse rather than silently overwriting.
    let mut cfg = s3_config_from_garage(&g, "nat2");
    cfg.conditional_put = Some("disabled".into());
    cfg.allow_non_atomic_publish = false;
    let store = s3_store_from_config(&cfg);
    let publisher = S3Publisher::from_store(&store).with_allow_non_atomic_publish(false);

    let v1 = Manifest::empty(1, "test");
    let err = publisher.publish(&v1).await.unwrap_err();
    match &err {
        StoreError::Backend(msg) => {
            assert!(
                msg.contains("allow_non_atomic_publish"),
                "error must name the missing flag, got {msg}"
            );
        }
        other => panic!("expected Backend, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_publish_under_non_atomic_does_not_corrupt_state() {
    let g = boot_garage().await;
    let cfg = s3_config_non_atomic_from_garage(&g, "nat3");

    // two publishers share the same backend by both pointing at the same
    // bucket+prefix. under non-atomic, the bucket can end with either
    // publisher's pointer; whichever lands, `current()` must resolve to a
    // valid manifest with format_version == MANIFEST_FORMAT_VERSION.
    let store_a = s3_store_from_config(&cfg);
    let store_b = s3_store_from_config(&cfg);
    let publisher_a = s3_publisher_from_store(&store_a, true);
    let publisher_b = s3_publisher_from_store(&store_b, true);

    publisher_a.publish(&Manifest::empty(1, "test")).await.unwrap();

    let v2_a = Manifest::empty(2, "test");
    let v2_b = Manifest::empty(2, "test");
    let (res_a, res_b) = tokio::join!(publisher_a.publish(&v2_a), publisher_b.publish(&v2_b));
    // under non-atomic both can succeed; the requirement is only that the
    // bucket isn't corrupted afterwards.
    let _ = res_a;
    let _ = res_b;

    let cur = publisher_a.current().await.unwrap().expect("a v2 must be current");
    assert_eq!(cur.version, 2);
    assert_eq!(cur.format_version, mars_types::MANIFEST_FORMAT_VERSION);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_emits_v2_after_pointer_swap_via_non_atomic_publish() {
    let g = boot_garage().await;
    let cfg = s3_config_non_atomic_from_garage(&g, "nat4");
    let store = s3_store_from_config(&cfg);
    // tight poll so the test doesn't drag.
    let publisher = Arc::new(
        S3Publisher::from_store(&store)
            .with_allow_non_atomic_publish(true)
            .with_poll_interval(Duration::from_millis(200)),
    );

    publisher.publish(&Manifest::empty(1, "test")).await.unwrap();
    let mut stream = publisher.watch().await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("watch must emit v1 promptly")
        .expect("stream not closed")
        .expect("v1 not error");
    assert_eq!(first.version, 1);

    publisher.publish(&Manifest::empty(2, "test")).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("watch must emit v2 within poll interval")
        .expect("stream not closed")
        .expect("v2 not error");
    assert_eq!(second.version, 2);
}
