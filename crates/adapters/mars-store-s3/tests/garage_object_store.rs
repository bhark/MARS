//! `S3Store` object_store surface against a real Garage container.
//! pins put/get/delete/list and the 8 MiB multipart path that
//! `object_store::memory::InMemory` cannot exercise.

#![cfg(feature = "integration")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/mod.rs"]
mod common;

use bytes::Bytes;
use mars_artifact::compute_content_hash;
use mars_store::{ObjectStore, StoreError};
use mars_test_support::garage::boot_garage;
use mars_types::{ArtifactKey, ContentHash};
use rand::TryRng;

use crate::common::{s3_config_from_garage, s3_store_from_config};

fn random_payload(bytes: usize) -> Vec<u8> {
    let mut v = vec![0u8; bytes];
    rand::rng().try_fill_bytes(&mut v).expect("os rng");
    v
}

#[tokio::test(flavor = "multi_thread")]
async fn put_then_get_returns_identical_bytes_and_hash_matches() {
    let g = boot_garage().await;
    let cfg = s3_config_from_garage(&g, "p1");
    let store = s3_store_from_config(&cfg);

    let payload = random_payload(1024);
    let key = ArtifactKey::new("a/b/one.bin");
    let hash = store.put(&key, Bytes::from(payload.clone())).await.unwrap();
    assert_eq!(hash, compute_content_hash(&payload));

    let back = store.get(&key, hash).await.unwrap();
    assert_eq!(back.as_ref(), payload.as_slice());
}

#[tokio::test(flavor = "multi_thread")]
async fn get_with_wrong_expected_hash_yields_hash_mismatch() {
    let g = boot_garage().await;
    let cfg = s3_config_from_garage(&g, "p2");
    let store = s3_store_from_config(&cfg);

    let key = ArtifactKey::new("a/b/two.bin");
    store.put(&key, Bytes::from_static(b"hello")).await.unwrap();
    let err = store.get(&key, ContentHash::zero()).await.unwrap_err();
    assert!(matches!(err, StoreError::HashMismatch { .. }), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_is_idempotent_on_missing_key() {
    let g = boot_garage().await;
    let cfg = s3_config_from_garage(&g, "p3");
    let store = s3_store_from_config(&cfg);

    store
        .delete(&ArtifactKey::new("never/written.bin"))
        .await
        .expect("delete of absent key must be Ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn list_returns_keys_under_prefix_only() {
    let g = boot_garage().await;
    let cfg = s3_config_from_garage(&g, "p4");
    let store = s3_store_from_config(&cfg);

    store
        .put(&ArtifactKey::new("group_a/x.bin"), Bytes::from_static(b"a1"))
        .await
        .unwrap();
    store
        .put(&ArtifactKey::new("group_a/y.bin"), Bytes::from_static(b"a2"))
        .await
        .unwrap();
    store
        .put(&ArtifactKey::new("group_b/z.bin"), Bytes::from_static(b"b1"))
        .await
        .unwrap();

    let got = store.list("group_a").await.unwrap();
    let mut got_strs: Vec<String> = got.into_iter().map(|k| k.as_str().to_owned()).collect();
    got_strs.sort();
    assert_eq!(got_strs, vec!["group_a/x.bin".to_string(), "group_a/y.bin".into()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn put_rejects_keys_with_traversal_and_backslash() {
    let g = boot_garage().await;
    let cfg = s3_config_from_garage(&g, "p5");
    let store = s3_store_from_config(&cfg);

    for bad in ["", "/leading", "rel/../escape", "back\\slash", "trail\0null"] {
        let err = store
            .put(&ArtifactKey::new(bad), Bytes::from_static(b"x"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Backend(_)),
            "expected Backend for key {bad:?}, got {err:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn put_8mib_object_round_trips() {
    let g = boot_garage().await;
    let cfg = s3_config_from_garage(&g, "p6");
    let store = s3_store_from_config(&cfg);

    let payload = random_payload(8 * 1024 * 1024);
    let key = ArtifactKey::new("big/blob.bin");
    let hash = store.put(&key, Bytes::from(payload.clone())).await.unwrap();
    let back = store.get(&key, hash).await.unwrap();
    assert_eq!(back.len(), payload.len(), "size mismatch on 8MiB round trip");
    assert_eq!(
        compute_content_hash(&back),
        compute_content_hash(&payload),
        "hash mismatch on 8MiB round trip"
    );
}
