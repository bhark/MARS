use bytes::Bytes;
use futures_util::StreamExt;
use mars_definition_source::DefinitionSource;

use crate::{InlineDefinitionSource, REVISION_HEX_LEN};

const PAYLOAD: &[u8] = b"service: { name: dagi }\nlayers: []\n";

#[tokio::test]
async fn fetch_returns_payload_and_stable_revision() {
    let src = InlineDefinitionSource::new(Bytes::from_static(PAYLOAD));

    let a = src.fetch().await.unwrap();
    let b = src.fetch().await.unwrap();

    assert_eq!(a.data.as_ref(), PAYLOAD);
    assert_eq!(a.revision, b.revision, "fetch must be deterministic");
    assert_eq!(a.revision.len(), REVISION_HEX_LEN);
    assert!(
        a.revision.chars().all(|c| c.is_ascii_hexdigit()),
        "revision must be hex: {:?}",
        a.revision
    );
}

#[tokio::test]
async fn different_inputs_yield_different_revisions() {
    let a = InlineDefinitionSource::new(Bytes::from_static(b"layers: []\n"));
    let b = InlineDefinitionSource::new(Bytes::from_static(b"layers: [foo]\n"));

    let ra = a.fetch().await.unwrap().revision;
    let rb = b.fetch().await.unwrap().revision;

    assert_ne!(ra, rb);
}

#[tokio::test]
async fn watch_is_empty() {
    let src = InlineDefinitionSource::new(Bytes::from_static(PAYLOAD));
    let collected: Vec<_> = src.watch().collect().await;
    assert!(collected.is_empty());
}

#[tokio::test]
async fn accepts_vec_and_static_str() {
    let _ = InlineDefinitionSource::new(b"x".to_vec());
    let _ = InlineDefinitionSource::new(Bytes::from_static(b"y"));
}
