//! port-level fake adapters that satisfy the trait surfaces with
//! `NotImplemented` or capture-and-return. used to compose `Deps` in tests
//! without naming a real backend, and to keep the port crates free of test
//! scaffolding.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer, TextMetrics};
use mars_source::{
    ChangeFeed, ChangeSubscription, RasterBinding, RasterSource, RowBytes, Source, SourceBinding, SourceError,
    TileBytes,
};
use mars_store::{LocalCache, ManifestStore, ObjectStore, StoreError};
use mars_style::ResolvedLabelStyle;
use mars_types::{ArtifactKey, ContentHash, Manifest};

/// `Source` + `ChangeFeed` impl that always returns `NotImplemented`.
#[derive(Debug, Default)]
pub struct NotImplementedSource;

#[async_trait]
impl Source for NotImplementedSource {
    async fn stream_rows<'a>(
        &'a self,
        _binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Err(SourceError::NotImplemented { what: "stream_rows" })
    }

    async fn stream_rows_by_id<'a>(
        &'a self,
        _binding: &'a SourceBinding,
        _ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "stream_rows_by_id",
        })
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        _binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "stream_feature_ids",
        })
    }
}

#[async_trait]
impl ChangeFeed for NotImplementedSource {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        Err(SourceError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedSource::subscribe",
        })
    }
}

/// `RasterSource` impl that always returns `NotImplemented`.
#[derive(Debug, Default)]
pub struct NotImplementedRasterSource;

#[async_trait]
impl RasterSource for NotImplementedRasterSource {
    async fn read_tile(&self, _binding: &RasterBinding, _z: u32, _x: u32, _y: u32) -> Result<TileBytes, SourceError> {
        Err(SourceError::NotImplemented {
            what: "RasterSource::read_tile",
        })
    }
}

/// `ManifestStore` impl that always returns `NotImplemented`.
#[derive(Debug, Default)]
pub struct NotImplementedManifestStore;

#[async_trait]
impl ManifestStore for NotImplementedManifestStore {
    async fn publish(&self, _manifest: &Manifest) -> Result<u64, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedManifestStore::publish",
        })
    }
    async fn current(&self) -> Result<Option<Manifest>, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedManifestStore::current",
        })
    }
    async fn watch(&self) -> Result<BoxStream<'static, Result<Manifest, StoreError>>, StoreError> {
        // empty stream is valid for a stub: consumers will simply observe
        // no manifest swaps.
        Ok(Box::pin(stream::empty()))
    }
}

/// `ObjectStore` impl that always returns `NotImplemented`.
#[derive(Debug, Default)]
pub struct NotImplementedStore;

#[async_trait]
impl ObjectStore for NotImplementedStore {
    async fn get(&self, _key: &ArtifactKey, _expected: ContentHash) -> Result<Bytes, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedStore::get",
        })
    }
    async fn put(&self, _key: &ArtifactKey, _body: Bytes) -> Result<ContentHash, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedStore::put",
        })
    }
    async fn delete(&self, _key: &ArtifactKey) -> Result<(), StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedStore::delete",
        })
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<ArtifactKey>, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedStore::list",
        })
    }
}

/// `Renderer` fake that records every `DrawOp` it sees, then returns a
/// zeroed pixmap so the encoder has something to encode. tests inspect the
/// captured ops list rather than pixel signatures. `measure_text` returns a
/// coarse char-width approximation; layout assertions stay stable across
/// runs because the metric is deterministic.
#[derive(Default, Clone)]
pub struct CapturingRenderer {
    pub log: Arc<Mutex<Vec<DrawOp>>>,
}

impl Renderer for CapturingRenderer {
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        let mut log = self.log.lock().unwrap();
        log.extend(ops.iter().cloned());
        let n = canvas.width as usize * canvas.height as usize * 4;
        Ok(Pixmap {
            width: canvas.width,
            height: canvas.height,
            premultiplied_rgba: vec![0u8; n],
        })
    }

    fn measure_text(&self, text: &str, style: &ResolvedLabelStyle) -> Result<TextMetrics, RenderError> {
        let chars = text.chars().count().max(1) as f32;
        let fs = style.font_size.max(1.0);
        Ok(TextMetrics {
            advance_x: chars * 0.55 * fs,
            ascent: fs * 0.8,
            descent: fs * 0.2,
        })
    }
}

/// `Encoder` fake that returns a sentinel byte vec sized off the pixmap's
/// dimensions; tests don't inspect the encoded bytes.
#[derive(Default)]
pub struct StubEncoder;

impl Encoder for StubEncoder {
    fn encode(&self, pixmap: &Pixmap, _format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        Ok(vec![0u8; (pixmap.width * pixmap.height) as usize])
    }
}

/// `LocalCache` impl that always returns `NotImplemented`.
#[derive(Debug, Default)]
pub struct NotImplementedCache;

#[async_trait]
impl LocalCache for NotImplementedCache {
    async fn get_or_fetch(
        &self,
        _key: &ArtifactKey,
        _expected: ContentHash,
        _origin: &dyn ObjectStore,
    ) -> Result<Bytes, StoreError> {
        Err(StoreError::NotImplemented {
            what: "mars-test-support::port_fakes::NotImplementedCache::get_or_fetch",
        })
    }
}
