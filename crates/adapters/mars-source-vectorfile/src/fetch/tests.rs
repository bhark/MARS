#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn parses_s3_uri() {
    let p = ParsedUri::parse("s3://bucket/path/to/file.fgb").unwrap();
    assert_eq!(p.scheme, "s3");
    assert_eq!(p.authority, "bucket");
    assert_eq!(p.object_path, "path/to/file.fgb");
}

#[test]
fn parses_file_uri() {
    let p = ParsedUri::parse("file:///tmp/x.fgb").unwrap();
    assert_eq!(p.scheme, "file");
    assert_eq!(p.authority, "");
    assert_eq!(p.object_path, "tmp/x.fgb");
}

#[test]
fn parses_https_uri() {
    let p = ParsedUri::parse("https://example.org/data/x.fgb").unwrap();
    assert_eq!(p.scheme, "https");
    assert_eq!(p.authority, "example.org");
    assert_eq!(p.object_path, "data/x.fgb");
}

#[test]
fn rejects_unknown_scheme() {
    let err = ParsedUri::parse("weird://x").unwrap_err();
    assert!(matches!(err, VectorFileError::UnsupportedScheme { .. }));
}

#[test]
fn scheme_dispatch_for_file() {
    // file:// resolves to LocalFileSystem without touching disk during build.
    let parsed = ParsedUri::parse("file:///tmp/anywhere.fgb").unwrap();
    let store = build_store(&parsed, false).unwrap();
    let dbg = format!("{:?}", store);
    assert!(dbg.contains("LocalFileSystem"), "got {dbg}");
}

#[tokio::test]
async fn fetch_through_local_file_roundtrips() {
    let tmp_payload = tempfile::NamedTempFile::new().unwrap();
    let path = tmp_payload.path().to_path_buf();
    std::fs::write(&path, b"hello-vectorfile").unwrap();

    let cache_dir = tempfile::tempdir().unwrap();
    let cache = DiskCache::open(cache_dir.path(), None).await.unwrap();
    let fetcher = Fetcher::new(false);
    let uri = format!("file://{}", path.display());
    let got = fetcher.fetch_cached(&uri, &cache).await.unwrap();
    assert_eq!(&got[..], b"hello-vectorfile");

    // second fetch should hit the cache.
    let got2 = fetcher.fetch_cached(&uri, &cache).await.unwrap();
    assert_eq!(got, got2);
}
