use bytes::Bytes;
use futures_util::StreamExt;
use mars_definition_source::{Change, DefinitionSource, DefinitionSourceError};
use std::time::Duration;
use tokio::time::timeout;

use super::FakeDefinitionSource;

#[tokio::test]
async fn fetch_returns_initial_payload() {
    let src = FakeDefinitionSource::new(Bytes::from_static(b"hi"), "r1");
    let got = src.fetch().await.unwrap();
    assert_eq!(got.data.as_ref(), b"hi");
    assert_eq!(got.revision, "r1");
}

#[tokio::test]
async fn set_payload_updates_fetch_and_queues_change() {
    let src = FakeDefinitionSource::new(Bytes::from_static(b"a"), "r1");
    src.set_payload(Bytes::from_static(b"b"), "r2").await;

    let mut watch = src.watch();
    let evt = timeout(Duration::from_secs(1), watch.next())
        .await
        .expect("change delivered")
        .expect("stream open");
    assert_eq!(evt, Change { revision: "r2".into() });

    let got = src.fetch().await.unwrap();
    assert_eq!(got.data.as_ref(), b"b");
    assert_eq!(got.revision, "r2");
}

#[tokio::test]
async fn fail_next_fetch_is_one_shot_fifo() {
    let src = FakeDefinitionSource::new(Bytes::from_static(b"x"), "r1");
    src.fail_next_fetch(DefinitionSourceError::NotFound { what: "first".into() });
    src.fail_next_fetch(DefinitionSourceError::Auth { what: "second".into() });

    assert!(matches!(
        src.fetch().await,
        Err(DefinitionSourceError::NotFound { what }) if what == "first"
    ));
    assert!(matches!(
        src.fetch().await,
        Err(DefinitionSourceError::Auth { what }) if what == "second"
    ));
    // queue drained -> back to normal
    let got = src.fetch().await.unwrap();
    assert_eq!(got.revision, "r1");
}

#[tokio::test]
async fn watch_is_single_consumer() {
    let src = FakeDefinitionSource::new(Bytes::from_static(b""), "r0");
    src.emit_change("r1").await;

    let mut first = src.watch();
    let evt = timeout(Duration::from_secs(1), first.next())
        .await
        .expect("delivered")
        .expect("open");
    assert_eq!(evt.revision, "r1");

    // second subscriber gets the empty stream
    let mut second = src.watch();
    let next = timeout(Duration::from_millis(50), second.next()).await;
    // pending until timeout (empty stream returns None immediately, not Pending)
    assert!(matches!(next, Ok(None)));
}

#[tokio::test]
async fn revision_reflects_latest_set_payload() {
    let src = FakeDefinitionSource::new(Bytes::from_static(b""), "r0");
    assert_eq!(src.revision(), "r0");
    src.set_payload(Bytes::from_static(b"x"), "r1").await;
    assert_eq!(src.revision(), "r1");
}
