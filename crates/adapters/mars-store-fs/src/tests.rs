use std::os::unix::fs::symlink;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use mars_artifact::compute_content_hash;
use mars_store::{LocalCache, ManifestStore, ObjectStore, StoreError};
use mars_types::{ArtifactKey, ContentHash, Manifest};
use tempfile::TempDir;

use super::*;

fn k(s: &str) -> ArtifactKey {
    ArtifactKey::new(s)
}

fn manifest(version: u64) -> Manifest {
    Manifest::new(version, "svc", vec![], vec![], None, vec![])
}

#[tokio::test]
async fn put_get_roundtrip_50() {
    let td = TempDir::new().unwrap();
    let store = FsStore::new(td.path()).unwrap();

    let mut written: Vec<(ArtifactKey, ContentHash, Vec<u8>)> = Vec::new();
    for i in 0..50u32 {
        let key = k(&format!("lyr/x/y/z{i:03}.mars"));
        let body: Vec<u8> = (0..(32 + i as usize)).map(|n| (n as u8).wrapping_mul(7)).collect();
        let h = store.put(&key, Bytes::from(body.clone())).await.unwrap();
        assert_eq!(h, compute_content_hash(&body));
        written.push((key, h, body));
    }

    // list returns deterministic, lex-sorted keys
    let listed = store.list("lyr").await.unwrap();
    assert_eq!(listed.len(), 50);
    let mut sorted = listed.clone();
    sorted.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    assert_eq!(listed, sorted);

    // get one back and verify
    let (key, hash, body) = &written[7];
    let got = store.get(key, *hash).await.unwrap();
    assert_eq!(got.as_ref(), body.as_slice());
}

#[tokio::test]
async fn hash_mismatch_on_corruption() {
    let td = TempDir::new().unwrap();
    let store = FsStore::new(td.path()).unwrap();
    let key = k("a/b.bin");
    let body = b"hello world".to_vec();
    let real = store.put(&key, Bytes::from(body.clone())).await.unwrap();

    // overwrite out-of-band with junk
    let path = td.path().join("a").join("b.bin");
    std::fs::write(&path, b"corrupted-bytes").unwrap();

    let err = store.get(&key, real).await.unwrap_err();
    assert!(matches!(err, StoreError::HashMismatch { .. }), "got {err:?}");
}

#[tokio::test]
async fn list_prefix_correctness() {
    let td = TempDir::new().unwrap();
    let store = FsStore::new(td.path()).unwrap();
    for k_ in ["a/1", "a/2", "b/1", "b/2", "b/sub/3"] {
        store.put(&k(k_), Bytes::from_static(b"x")).await.unwrap();
    }
    let a = store.list("a").await.unwrap();
    assert_eq!(
        a.iter().map(|x| x.as_str().to_owned()).collect::<Vec<_>>(),
        vec!["a/1", "a/2"]
    );
    let b = store.list("b").await.unwrap();
    assert_eq!(
        b.iter().map(|x| x.as_str().to_owned()).collect::<Vec<_>>(),
        vec!["b/1", "b/2", "b/sub/3"]
    );
}

#[tokio::test]
async fn delete_missing_returns_not_found() {
    let td = TempDir::new().unwrap();
    let store = FsStore::new(td.path()).unwrap();
    let err = store.delete(&k("nope")).await.unwrap_err();
    assert!(matches!(err, StoreError::NotFound(_)), "got {err:?}");
}

#[tokio::test]
async fn publish_atomicity_and_recovery() {
    let td = TempDir::new().unwrap();
    let pub1 = FsPublisher::new(td.path()).unwrap();

    let m1 = manifest(1);
    pub1.publish(&m1).await.unwrap();
    assert_eq!(pub1.read_current().unwrap().as_deref(), Some("v1"));

    let m2 = Manifest {
        version: 2,
        ..m1.clone()
    };
    pub1.publish(&m2).await.unwrap();
    assert_eq!(pub1.read_current().unwrap().as_deref(), Some("v2"));

    // simulate a crash mid-publish: write v3 body but skip the pointer swap.
    let m3 = Manifest {
        version: 3,
        ..m1.clone()
    };
    let v3_body = serde_json::to_vec_pretty(&m3).unwrap();
    let v3_path = pub1.manifests_dir().join("v3.json");
    std::fs::write(&v3_path, &v3_body).unwrap();

    // a freshly-constructed publisher must still see v2 as current.
    let pub2 = FsPublisher::new(td.path()).unwrap();
    assert_eq!(pub2.read_current().unwrap().as_deref(), Some("v2"));
}

#[tokio::test]
async fn watch_waits_when_current_is_absent() {
    let td = TempDir::new().unwrap();
    let publisher = FsPublisher::new_with_poll_interval(td.path(), Duration::from_millis(10)).unwrap();
    let mut stream = publisher.watch().await.unwrap();

    let next = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
    assert!(next.is_err(), "watcher yielded without current");
}

#[tokio::test]
async fn watch_yields_existing_current_on_subscribe() {
    let td = TempDir::new().unwrap();
    let publisher = FsPublisher::new_with_poll_interval(td.path(), Duration::from_millis(10)).unwrap();
    publisher.publish(&manifest(1)).await.unwrap();

    let mut stream = publisher.watch().await.unwrap();
    let got = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got.version, 1);
}

#[tokio::test]
async fn watch_yields_changed_pointer() {
    let td = TempDir::new().unwrap();
    let publisher = FsPublisher::new_with_poll_interval(td.path(), Duration::from_millis(10)).unwrap();
    publisher.publish(&manifest(1)).await.unwrap();

    let mut stream = publisher.watch().await.unwrap();
    let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.version, 1);

    publisher.publish(&manifest(2)).await.unwrap();
    let second = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(second.version, 2);
}

#[tokio::test]
async fn watch_reports_bad_pointer_then_recovers() {
    let td = TempDir::new().unwrap();
    let publisher = FsPublisher::new_with_poll_interval(td.path(), Duration::from_millis(10)).unwrap();
    std::fs::create_dir_all(publisher.manifests_dir()).unwrap();
    std::fs::write(publisher.manifests_dir().join("current"), "../bad").unwrap();

    let mut stream = publisher.watch().await.unwrap();
    let bad = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(bad, Err(StoreError::Backend(_))));

    publisher.publish(&manifest(1)).await.unwrap();
    let recovered = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(recovered.version, 1);
}

#[tokio::test]
async fn watch_throttles_repeated_bad_pointer_errors() {
    let td = TempDir::new().unwrap();
    let poll_interval = Duration::from_millis(50);
    let publisher = FsPublisher::new_with_poll_interval(td.path(), poll_interval).unwrap();
    std::fs::create_dir_all(publisher.manifests_dir()).unwrap();
    std::fs::write(publisher.manifests_dir().join("current"), "../bad").unwrap();

    let mut stream = publisher.watch().await.unwrap();
    let first = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(first, Err(StoreError::Backend(_))));

    let repeated = tokio::time::timeout(Duration::from_millis(15), stream.next()).await;
    assert!(repeated.is_err(), "watcher repeated an error without sleeping");

    let second = tokio::time::timeout(Duration::from_millis(100), stream.next())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(second, Err(StoreError::Backend(_))));
}

#[tokio::test]
async fn symlink_escape_rejected() {
    let td = TempDir::new().unwrap();
    let store = FsStore::new(td.path()).unwrap();
    // create <root>/escape -> current executable (guaranteed to exist)
    let target = std::env::current_exe().unwrap();
    assert!(target.exists(), "current exe must exist for symlink escape test");
    let link = td.path().join("escape");
    symlink(&target, &link).unwrap();

    let err = store.get(&k("escape"), ContentHash::zero()).await.unwrap_err();
    assert!(
        matches!(err, StoreError::Backend(_)),
        "expected backend rejection, got {err:?}"
    );
}

#[tokio::test]
async fn cache_miss_then_hit_then_recover() {
    let store_dir = TempDir::new().unwrap();
    let cache_dir = TempDir::new().unwrap();
    let store = FsStore::new(store_dir.path()).unwrap();
    let cache = FsCache::new(cache_dir.path(), u64::MAX).unwrap();

    let key = k("a/b.bin");
    let body = b"some bytes here".to_vec();
    let h = store.put(&key, Bytes::from(body.clone())).await.unwrap();

    // first call: miss → fetch from origin, persist.
    let first = cache.get_or_fetch(&key, h, &store).await.unwrap();
    assert_eq!(first.as_ref(), body.as_slice());
    let cached_path = cache_dir.path().canonicalize().unwrap().join("a").join("b.bin");
    assert!(cached_path.exists());

    // second call: hit local. corrupt origin to prove we don't refetch.
    let bad_origin_path = store_dir.path().join("a").join("b.bin");
    std::fs::write(&bad_origin_path, b"junk").unwrap();
    let second = cache.get_or_fetch(&key, h, &store).await.unwrap();
    assert_eq!(second.as_ref(), body.as_slice());

    // restore origin, corrupt the cache → must re-fetch.
    std::fs::write(&bad_origin_path, &body).unwrap();
    std::fs::write(&cached_path, b"poisoned-cache").unwrap();
    let third = cache.get_or_fetch(&key, h, &store).await.unwrap();
    assert_eq!(third.as_ref(), body.as_slice());
}
