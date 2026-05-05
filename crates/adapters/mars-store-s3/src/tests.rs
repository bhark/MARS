//! unit tests against `object_store::memory::InMemory`. real S3/MinIO
//! coverage lives in a follow-up PR.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::compute_content_hash;
use mars_store::{ManifestStore, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash, Manifest};
use object_store::memory::InMemory;

use crate::{S3Publisher, S3Store};

fn store(prefix: &str) -> S3Store {
    S3Store::from_backend(Arc::new(InMemory::new()), prefix.to_owned())
}

fn store_with(prefix: &str, backend: Arc<InMemory>) -> S3Store {
    S3Store::from_backend(backend, prefix.to_owned())
}

fn manifest(version: u64) -> Manifest {
    Manifest::new(version, "test".to_owned(), vec![], vec![], None, vec![])
}

#[tokio::test]
async fn put_get_roundtrip() {
    let s = store("");
    let key = ArtifactKey::new("a/b/c.bin");
    let body = Bytes::from_static(b"hello world");
    let hash = s.put(&key, body.clone()).await.unwrap();
    let got = s.get(&key, hash).await.unwrap();
    assert_eq!(got, body);
}

#[tokio::test]
async fn put_get_with_prefix() {
    let s = store("mars/data");
    let key = ArtifactKey::new("lyr/x/y.mars");
    let body = Bytes::from_static(b"payload");
    let hash = s.put(&key, body.clone()).await.unwrap();
    let got = s.get(&key, hash).await.unwrap();
    assert_eq!(got, body);
}

#[tokio::test]
async fn hash_mismatch_detected() {
    let s = store("");
    let key = ArtifactKey::new("k");
    s.put(&key, Bytes::from_static(b"abc")).await.unwrap();
    let bogus = ContentHash([0u8; 32]);
    let err = s.get(&key, bogus).await.unwrap_err();
    assert!(matches!(err, StoreError::HashMismatch { .. }));
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let s = store("");
    let key = ArtifactKey::new("missing");
    let err = s.get(&key, compute_content_hash(b"")).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)));
}

#[tokio::test]
async fn list_with_prefix() {
    let s = store("p");
    for k in ["a/1.bin", "a/2.bin", "b/3.bin"] {
        s.put(&ArtifactKey::new(k), Bytes::from_static(b"x")).await.unwrap();
    }
    let all = s.list("").await.unwrap();
    assert_eq!(all.len(), 3);

    let only_a = s.list("a").await.unwrap();
    let names: Vec<_> = only_a.iter().map(|k| k.as_str()).collect();
    assert_eq!(names, vec!["a/1.bin", "a/2.bin"]);
}

#[tokio::test]
async fn delete_then_missing() {
    let s = store("");
    let key = ArtifactKey::new("d");
    s.put(&key, Bytes::from_static(b"y")).await.unwrap();
    s.delete(&key).await.unwrap();
    // delete is idempotent: removing a missing object is a no-op (matches
    // AWS S3 DeleteObject semantics).
    s.delete(&key).await.unwrap();
}

#[tokio::test]
async fn rejects_bad_keys() {
    let s = store("");
    for bad in ["", "/a", "a/../b", "a\\b", "a\0b", "."] {
        let err = s.put(&ArtifactKey::new(bad), Bytes::from_static(b"")).await.unwrap_err();
        assert!(matches!(err, StoreError::Backend(_)), "{bad:?} should be rejected");
    }
}

#[tokio::test]
async fn manifest_publish_and_current() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    let pub_ = S3Publisher::from_store(&s);
    assert!(pub_.current().await.unwrap().is_none());

    let v = pub_.publish(&manifest(1)).await.unwrap();
    assert_eq!(v, 1);

    let m = pub_.current().await.unwrap().unwrap();
    assert_eq!(m.version, 1);

    pub_.publish(&manifest(2)).await.unwrap();
    let m = pub_.current().await.unwrap().unwrap();
    assert_eq!(m.version, 2);
}

#[tokio::test]
async fn manifest_watch_yields_on_change() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    let pub_ = S3Publisher::from_store(&s).with_poll_interval(Duration::from_millis(20));

    pub_.publish(&manifest(1)).await.unwrap();
    let mut stream = pub_.watch().await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next()).await.unwrap();
    let m = first.unwrap().unwrap();
    assert_eq!(m.version, 1);

    pub_.publish(&manifest(2)).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), stream.next()).await.unwrap();
    let m = second.unwrap().unwrap();
    assert_eq!(m.version, 2);
}
