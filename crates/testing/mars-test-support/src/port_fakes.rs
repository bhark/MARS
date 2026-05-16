//! port-level fake adapters that satisfy the trait surfaces with
//! `NotImplemented`. used to compose `Deps` in tests without naming a real
//! backend, and to keep the port crates free of test scaffolding.

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::stream;
use mars_source::{
    ChangeFeed, ChangeSubscription, RasterBinding, RasterSource, RowBytes, Source, SourceBinding, SourceError,
    TileBytes,
};
use mars_store::{LocalCache, ManifestStore, ObjectStore, StoreError};
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
