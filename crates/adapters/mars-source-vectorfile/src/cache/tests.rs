#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

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
