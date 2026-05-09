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
    Manifest::empty(version, "test".to_owned())
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
        let err = s
            .put(&ArtifactKey::new(bad), Bytes::from_static(b""))
            .await
            .unwrap_err();
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

/// helper for the version-rejection tests: write `manifests/v{n}.json` and
/// `manifests/current` directly through the store, sidestepping the
/// publisher (which only emits the current `MANIFEST_FORMAT_VERSION`).
async fn write_legacy_manifest(s: &S3Store, version: u64, body: &str) {
    s.put(
        &ArtifactKey::new(format!("manifests/v{version}.json")),
        Bytes::from(body.to_owned()),
    )
    .await
    .unwrap();
    s.put(
        &ArtifactKey::new("manifests/current"),
        Bytes::from(format!("v{version}")),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn current_rejects_v1_manifest() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    write_legacy_manifest(
        &s,
        1,
        r#"{"format_version":1,"version":1,"service":"svc","source_artifacts":[],"layer_artifacts":[],"style_artifact":null}"#,
    )
    .await;

    let pub_ = S3Publisher::from_store(&s);
    let err = pub_.current().await.unwrap_err();
    assert!(
        matches!(err, StoreError::UnsupportedManifestVersion { found: 1, supported: 4 }),
        "expected UnsupportedManifestVersion {{ found: 1, supported: 4 }}, got {err:?}"
    );
}

#[tokio::test]
async fn current_rejects_v2_manifest() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    write_legacy_manifest(
        &s,
        2,
        r#"{"format_version":2,"version":2,"service":"svc","created_at":{"secs_since_epoch":0,"nanos_since_epoch":0},"source_artifacts":[],"layer_artifacts":[],"style_artifact":null,"empty_layer_cells":[],"source_version":null}"#,
    )
    .await;

    let pub_ = S3Publisher::from_store(&s);
    let err = pub_.current().await.unwrap_err();
    assert!(
        matches!(err, StoreError::UnsupportedManifestVersion { found: 2, supported: 4 }),
        "expected UnsupportedManifestVersion {{ found: 2, supported: 4 }}, got {err:?}"
    );
}

#[tokio::test]
async fn current_rejects_v3_manifest() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    // v3 had `hilbert_range_table: Vec<(HilbertKey, HilbertKey)>`; v4 widens
    // it to a 3-tuple carrying a stable PageId. v3 must be rejected.
    write_legacy_manifest(
        &s,
        3,
        r#"{"format_version":3,"version":3,"service":"svc","created_at":{"secs_since_epoch":0,"nanos_since_epoch":0},"bindings":[],"pages":[],"class_sidecars":[],"label_sidecars":[],"style_artifact":null,"source_version":null,"epoch":0}"#,
    )
    .await;

    let pub_ = S3Publisher::from_store(&s);
    let err = pub_.current().await.unwrap_err();
    assert!(
        matches!(err, StoreError::UnsupportedManifestVersion { found: 3, supported: 4 }),
        "expected UnsupportedManifestVersion {{ found: 3, supported: 4 }}, got {err:?}"
    );
}

#[tokio::test]
async fn current_rejects_future_manifest_version() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    // forwards-incompatibility: a v5 body must also be rejected, not silently
    // accepted as "newer therefore probably ok".
    write_legacy_manifest(
        &s,
        1,
        r#"{"format_version":5,"version":1,"service":"svc","created_at":{"secs_since_epoch":0,"nanos_since_epoch":0},"bindings":[],"pages":[],"class_sidecars":[],"label_sidecars":[],"style_artifact":null,"source_version":null,"epoch":0}"#,
    )
    .await;

    let pub_ = S3Publisher::from_store(&s);
    let err = pub_.current().await.unwrap_err();
    assert!(
        matches!(err, StoreError::UnsupportedManifestVersion { found: 5, supported: 4 }),
        "expected UnsupportedManifestVersion {{ found: 5, supported: 4 }}, got {err:?}"
    );
}

// wrapper that rejects conditional put with NotSupported
#[derive(Debug)]
struct NoCasBackend(Arc<InMemory>);

impl std::fmt::Display for NoCasBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "NoCasBackend")
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for NoCasBackend {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        if matches!(opts.mode, object_store::PutMode::Update(_)) {
            return Err(object_store::Error::NotSupported {
                source: Box::new(std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "conditional put not supported",
                )),
            });
        }
        self.0.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.0.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.0.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.0.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.0.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&object_store::path::Path>,
        offset: &object_store::path::Path,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.0.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.0.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.0.copy_opts(from, to, options).await
    }

    async fn rename_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::RenameOptions,
    ) -> object_store::Result<()> {
        self.0.rename_opts(from, to, options).await
    }
}

#[tokio::test]
async fn manifest_publish_rejects_duplicate_body_version() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    let pub_ = S3Publisher::from_store(&s);
    pub_.publish(&manifest(1)).await.unwrap();

    // simulate an orphaned body (e.g. a prior crash between body write and
    // pointer CAS): publish v1 again. PutMode::Create must refuse to
    // overwrite the existing body.
    let err = pub_.publish(&manifest(1)).await.unwrap_err();
    assert!(
        err.to_string().contains("already exists"),
        "expected duplicate-body refusal, got {err}"
    );
}

#[tokio::test]
async fn manifest_publish_rejects_non_atomic_by_default() {
    let backend: Arc<dyn object_store::ObjectStore> = Arc::new(NoCasBackend(Arc::new(InMemory::new())));
    let s = S3Store::from_backend(backend, String::new());
    // first publish succeeds (no prior -> PutMode::Create)
    let pub_ok = S3Publisher::from_store(&s).with_allow_non_atomic_publish(true);
    pub_ok.publish(&manifest(1)).await.unwrap();

    // second publish uses PutMode::Update -> NotSupported -> rejected
    let pub_strict = S3Publisher::from_store(&s);
    let err = pub_strict.publish(&manifest(2)).await.unwrap_err();
    assert!(
        err.to_string().contains("allow_non_atomic_publish"),
        "expected refusal, got {err}"
    );
}

#[tokio::test]
async fn manifest_publish_allows_non_atomic_when_flag_set() {
    let backend: Arc<dyn object_store::ObjectStore> = Arc::new(NoCasBackend(Arc::new(InMemory::new())));
    let s = S3Store::from_backend(backend, String::new());
    let pub_ = S3Publisher::from_store(&s).with_allow_non_atomic_publish(true);
    let v = pub_.publish(&manifest(1)).await.unwrap();
    assert_eq!(v, 1);
}

// flaky-on-purpose backend that fails reads N times with `Generic` then
// delegates to InMemory. used to verify transient retry behaviour.
#[derive(Debug)]
struct FlakyBackend {
    inner: Arc<InMemory>,
    fails_remaining: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Display for FlakyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FlakyBackend")
    }
}

#[async_trait::async_trait]
impl object_store::ObjectStore for FlakyBackend {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        use std::sync::atomic::Ordering;
        let prev = self.fails_remaining.load(Ordering::SeqCst);
        if prev > 0
            && self
                .fails_remaining
                .compare_exchange(prev, prev - 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        {
            return Err(object_store::Error::Generic {
                store: "Flaky",
                source: Box::new(std::io::Error::other("transient")),
            });
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&object_store::path::Path>,
        offset: &object_store::path::Path,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }

    async fn rename_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::RenameOptions,
    ) -> object_store::Result<()> {
        self.inner.rename_opts(from, to, options).await
    }
}

#[tokio::test(start_paused = true)]
async fn get_retries_transient_errors() {
    let backend = Arc::new(FlakyBackend {
        inner: Arc::new(InMemory::new()),
        fails_remaining: std::sync::atomic::AtomicUsize::new(2),
    });
    let s = S3Store::from_backend(backend, String::new());
    let key = ArtifactKey::new("a/b.bin");
    let payload = Bytes::from_static(b"hello");
    let hash = compute_content_hash(&payload);
    s.put(&key, payload.clone()).await.unwrap();
    let got = s.get(&key, hash).await.expect("should succeed after retries");
    assert_eq!(got, payload);
}

#[tokio::test]
async fn manifest_watch_yields_on_change() {
    let backend = Arc::new(InMemory::new());
    let s = store_with("", backend);
    let pub_ = S3Publisher::from_store(&s).with_poll_interval(Duration::from_millis(20));

    pub_.publish(&manifest(1)).await.unwrap();
    let mut stream = pub_.watch().await.unwrap();

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap();
    let m = first.unwrap().unwrap();
    assert_eq!(m.version, 1);

    pub_.publish(&manifest(2)).await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .unwrap();
    let m = second.unwrap().unwrap();
    assert_eq!(m.version, 2);
}
