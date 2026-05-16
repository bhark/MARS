//! AWS-equivalent (CAS-enabled) manifest publish behaviour, exercised against
//! `object_store::memory::InMemory`. complements the in-module tests by
//! pinning:
//!   - sequential v1 -> v2 swap moves `current()`
//!   - two concurrent publishes of the same version end with exactly one
//!     `Ok` and one error (body-already-exists or pointer-precondition); the
//!     bucket never lands in a state where neither pointer nor body wins.
//!
//! the in-module tests prove individual variants of these paths (allow_non
//! _atomic disabled, hash mismatch, key validation). this file proves the
//! aggregate CAS contract under the codepath AWS / R2 actually take.
//!
//! the stale-pointer case (publisher A reads etag E1, publisher B publishes
//! E1 -> E2, A's CAS fires with E1 and fails with Precondition) is the same
//! race the concurrent-writers test exercises - there is no externally
//! visible interleaving point between `read_current` and the CAS update, so
//! a deterministic stale-pointer reproduction would need a private hook.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mars_store::ManifestStore;
use mars_store_s3::{S3Publisher, S3Store};
use mars_types::Manifest;
use object_store::memory::InMemory;

fn fresh_publisher() -> (Arc<InMemory>, S3Publisher) {
    let backend: Arc<InMemory> = Arc::new(InMemory::new());
    let store = S3Store::from_backend(backend.clone() as Arc<dyn object_store::ObjectStore>, String::new());
    let publisher = S3Publisher::from_store(&store);
    (backend, publisher)
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_v1_then_v2_atomically_swaps_current() {
    let (_backend, publisher) = fresh_publisher();

    let v1 = Manifest::empty(1, "test");
    let v2 = Manifest::empty(2, "test");

    assert_eq!(publisher.publish(&v1).await.unwrap(), 1);
    let cur = publisher.current().await.unwrap().expect("v1 must be current");
    assert_eq!(cur.version, 1);

    assert_eq!(publisher.publish(&v2).await.unwrap(), 2);
    let cur = publisher.current().await.unwrap().expect("v2 must be current");
    assert_eq!(cur.version, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_concurrent_writers_at_same_version_only_one_wins() {
    let (_backend, publisher_a) = fresh_publisher();
    // share the backend so both publishers race the same object store.
    let backend = _backend.clone();
    let store = S3Store::from_backend(backend as Arc<dyn object_store::ObjectStore>, String::new());
    let publisher_b = S3Publisher::from_store(&store);

    let v1 = Manifest::empty(1, "test");
    publisher_a.publish(&v1).await.unwrap();

    let v2_a = Manifest::empty(2, "test");
    let v2_b = Manifest::empty(2, "test");
    let (res_a, res_b) = tokio::join!(publisher_a.publish(&v2_a), publisher_b.publish(&v2_b));

    // body-create with PutMode::Create is exclusive: exactly one of the two
    // publishes lands. the other surfaces either "body already exists" (lost
    // the body race) or "manifest pointer changed concurrently" (lost the
    // pointer race after winning the body race). either way it is a backend
    // or transient error, not Ok.
    let ok_count = [&res_a, &res_b].iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        ok_count, 1,
        "expected exactly one publish to succeed; got res_a={res_a:?} res_b={res_b:?}"
    );
    let cur = publisher_a.current().await.unwrap().expect("a v2 must be current");
    assert_eq!(cur.version, 2);
}
