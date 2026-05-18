#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use super::*;

#[tokio::test]
async fn roundtrip_writes_then_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(tmp.path(), None).await.unwrap();
    let uri = "s3://bucket/x.fgb";
    let etag = "\"abc123\"";
    assert!(cache.get(uri, etag).await.unwrap().is_none());
    let payload = Bytes::from_static(b"hello vector world");
    cache.put(uri, etag, &payload).await.unwrap();
    let got = cache.get(uri, etag).await.unwrap().unwrap();
    assert_eq!(got, payload);
}

#[tokio::test]
async fn layout_keyed_by_scheme_and_uri() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(tmp.path(), None).await.unwrap();
    let p1 = cache.entry_path("s3://bucket/a.fgb", "etag1");
    let p2 = cache.entry_path("file:///tmp/a.fgb", "etag1");
    // different schemes -> different prefixes
    assert!(p1.starts_with(tmp.path().join("s3")));
    assert!(p2.starts_with(tmp.path().join("file")));
    // different uris -> different uri hash
    assert_ne!(p1.parent().unwrap(), p2.parent().unwrap());
}

#[tokio::test]
async fn put_over_cap_evicts_oldest() {
    let tmp = tempfile::tempdir().unwrap();
    // cap holds ~1.5 entries of 100 bytes each.
    let cache = DiskCache::open(tmp.path(), Some(150)).await.unwrap();
    let etag = "v1";
    let payload = Bytes::from(vec![0u8; 100]);

    cache.put("s3://b/a", etag, &payload).await.unwrap();
    cache.put("s3://b/b", etag, &payload).await.unwrap();

    // first entry is the lru victim once the second pushes us over budget.
    assert!(cache.get("s3://b/a", etag).await.unwrap().is_none());
    assert!(cache.get("s3://b/b", etag).await.unwrap().is_some());
    assert!(!cache.entry_path("s3://b/a", etag).exists());
}

#[tokio::test]
async fn get_refreshes_lru_position() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(tmp.path(), Some(220)).await.unwrap();
    let etag = "v1";
    let payload = Bytes::from(vec![0u8; 100]);

    cache.put("s3://b/a", etag, &payload).await.unwrap();
    cache.put("s3://b/b", etag, &payload).await.unwrap();
    // touch a so it's now mru; b becomes the eviction candidate.
    let _ = cache.get("s3://b/a", etag).await.unwrap();
    cache.put("s3://b/c", etag, &payload).await.unwrap();

    assert!(cache.get("s3://b/a", etag).await.unwrap().is_some());
    assert!(cache.get("s3://b/b", etag).await.unwrap().is_none());
    assert!(cache.get("s3://b/c", etag).await.unwrap().is_some());
}

#[tokio::test]
async fn scan_on_open_seeds_lru_and_evicts_to_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let payload = Bytes::from(vec![0u8; 100]);

    // pre-populate via an unbounded cache; sleeps space mtimes so the
    // scan ordering is deterministic across filesystem timestamp resolutions.
    let bootstrap = DiskCache::open(tmp.path(), None).await.unwrap();
    bootstrap.put("s3://b/old", "v", &payload).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    bootstrap.put("s3://b/mid", "v", &payload).await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    bootstrap.put("s3://b/new", "v", &payload).await.unwrap();
    drop(bootstrap);

    // reopen with budget that fits two of three entries; oldest by mtime evicts.
    let cache = DiskCache::open(tmp.path(), Some(220)).await.unwrap();
    assert!(cache.get("s3://b/old", "v").await.unwrap().is_none());
    assert!(cache.get("s3://b/mid", "v").await.unwrap().is_some());
    assert!(cache.get("s3://b/new", "v").await.unwrap().is_some());
}

#[tokio::test]
async fn none_cap_disables_eviction() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(tmp.path(), None).await.unwrap();
    let etag = "v1";
    let payload = Bytes::from(vec![0u8; 1024]);

    for i in 0..32 {
        let uri = format!("s3://b/x{i}");
        cache.put(&uri, etag, &payload).await.unwrap();
    }
    for i in 0..32 {
        let uri = format!("s3://b/x{i}");
        assert!(
            cache.get(&uri, etag).await.unwrap().is_some(),
            "entry {i} evicted under None cap",
        );
    }
}

#[tokio::test]
async fn single_entry_larger_than_cap_is_evicted() {
    let tmp = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(tmp.path(), Some(10)).await.unwrap();
    let payload = Bytes::from(vec![0u8; 100]);

    cache.put("s3://b/big", "v", &payload).await.unwrap();
    // matches mars-store-fs contract: an entry exceeding the cap is evicted
    // by the budget loop on insert. operator gets a warn log.
    assert!(cache.get("s3://b/big", "v").await.unwrap().is_none());
}
