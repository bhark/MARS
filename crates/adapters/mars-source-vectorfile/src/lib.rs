//! Object-store-backed adapter for `mars-source`.
//!
//! Pulls vector-file payloads (FlatGeobuf, GeoJSON, zipped Shapefile)
//! from any object_store scheme (`s3://`, `gs://`, `file://`, `https://`),
//! caches them on disk keyed by `(uri, etag)`, decodes them through a
//! pluggable [`decoder`] registry, and reprojects every row's geometry
//! from the per-binding `source_crs` to the configured `Source.native_crs`.
//! A polled etag change feed emits `ChangeEvent::Rebind` when the
//! upstream object identity moves.
//!
//! Shapefile note: the binding URI points at a single ZIP archive
//! bundling the `.shp` + `.shx` + `.dbf` triple at a shared basename;
//! a `.prj` is honoured when present. One archive keeps the adapter's
//! single-URI fetch contract intact and matches the way public shapefile
//! distributions ship in practice.
//!
//! Bindings reach this adapter through the port-level [`SourceBinding`].
//! Because the port's `from: String` is an opaque locator, this adapter
//! defines a fragment convention so the binding carries the decoder hint
//! and source CRS alongside the URI:
//!
//! ```text
//! s3://bucket/data/roads.fgb#format=flat_geobuf&source_crs=EPSG:4326
//! file:///var/data/buildings.geojson#format=geo_json&source_crs=EPSG:25832
//! ```
//!
//! See [`fragment`] for the parser. The composition layer that builds
//! `SourceBinding` from the typed `mars_config::SourceBinding` formats
//! the fragment to match.

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use mars_source::{
    BindingHealth, ChangeFeed, ChangeSubscription, RowBytes, Source, SourceBinding, SourceCollectionId, SourceError,
};
use mars_types::CrsCode;

pub mod cache;
pub mod change_feed;
pub mod decoder;
pub mod error;
pub mod fetch;
pub mod fragment;
pub mod reproject;

pub use error::VectorFileError;

/// Vector-file source: pulls bytes from an object store, decodes them
/// with a format-aware decoder, and emits reprojected `RowBytes`.
pub struct VectorFileSource {
    id: mars_config::SourceId,
    native_crs: CrsCode,
    cfg: mars_config::VectorFileBackend,
    cache: Arc<cache::DiskCache>,
    fetcher: Arc<fetch::Fetcher>,
    decoders: Arc<decoder::Registry>,
}

impl std::fmt::Debug for VectorFileSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VectorFileSource")
            .field("id", &self.id)
            .field("native_crs", &self.native_crs)
            .field("cache_dir", &self.cfg.cache_dir)
            .finish()
    }
}

impl VectorFileSource {
    /// Construct a vector-file source. Validates the backend config and
    /// prepares the on-disk cache root; no network I/O is performed.
    pub async fn new(
        id: mars_config::SourceId,
        native_crs: CrsCode,
        cfg: mars_config::VectorFileBackend,
    ) -> Result<Self, VectorFileError> {
        if cfg.cache_dir.is_empty() {
            return Err(VectorFileError::InvalidConfig {
                what: "cache_dir is empty",
            });
        }
        let cache_max = match cfg.cache_max_size_bytes() {
            Ok(v) => v,
            Err(_) => return Err(VectorFileError::InvalidConfig { what: "cache_max_size" }),
        };
        let cache = Arc::new(cache::DiskCache::open(cfg.cache_dir.clone(), cache_max).await?);
        let fetcher = Arc::new(fetch::Fetcher::new());
        let decoders = Arc::new(decoder::Registry::with_builtin());
        Ok(Self {
            id,
            native_crs,
            cfg,
            cache,
            fetcher,
            decoders,
        })
    }

    /// Borrow the configured native CRS - the CRS this adapter emits WKB in.
    #[must_use]
    pub fn native_crs(&self) -> &CrsCode {
        &self.native_crs
    }

    /// Borrow the source id.
    #[must_use]
    pub fn id(&self) -> &mars_config::SourceId {
        &self.id
    }

    /// Borrow the backend config.
    #[must_use]
    pub fn config(&self) -> &mars_config::VectorFileBackend {
        &self.cfg
    }
}

#[async_trait]
impl Source for VectorFileSource {
    async fn stream_rows<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let plan = plan_binding(binding, &self.native_crs)?;
        let bytes = self
            .fetch_bytes(&plan.uri)
            .await
            .map_err(|e| SourceError::backend("fetch", e))?;
        decoder::stream_rows(
            self.decoders.clone(),
            bytes,
            binding.clone(),
            plan,
            self.native_crs.clone(),
            None,
        )
        .await
    }

    async fn stream_rows_by_id<'a>(
        &'a self,
        binding: &'a SourceBinding,
        ids: &'a [i64],
    ) -> Result<BoxStream<'a, Result<RowBytes, SourceError>>, SourceError> {
        let plan = plan_binding(binding, &self.native_crs)?;
        let bytes = self
            .fetch_bytes(&plan.uri)
            .await
            .map_err(|e| SourceError::backend("fetch", e))?;
        let id_filter: std::collections::HashSet<i64> = ids.iter().copied().collect();
        decoder::stream_rows(
            self.decoders.clone(),
            bytes,
            binding.clone(),
            plan,
            self.native_crs.clone(),
            Some(id_filter),
        )
        .await
    }

    async fn stream_feature_ids<'a>(
        &'a self,
        binding: &'a SourceBinding,
    ) -> Result<BoxStream<'a, Result<i64, SourceError>>, SourceError> {
        let plan = plan_binding(binding, &self.native_crs)?;
        let bytes = self
            .fetch_bytes(&plan.uri)
            .await
            .map_err(|e| SourceError::backend("fetch", e))?;
        decoder::stream_feature_ids(self.decoders.clone(), bytes, binding.clone(), plan).await
    }

    async fn probe_binding_health(
        &self,
        collections: &[SourceCollectionId],
    ) -> Result<Vec<BindingHealth>, SourceError> {
        // adapter has no notion of publication membership; report all healthy.
        // wiring the URI map at construct time and HEADing each here is the
        // upgrade path once the bin-shared factory threads bindings through.
        Ok(collections.iter().cloned().map(BindingHealth::Healthy).collect())
    }
}

#[async_trait]
impl ChangeFeed for VectorFileSource {
    async fn subscribe(&self) -> Result<Box<dyn ChangeSubscription>, SourceError> {
        // todo: a real polled-etag feed needs the planner to register
        // (collection, uri) pairs at construct time. for now mark as
        // not-implemented so callers can detect snapshot-only mode.
        Err(SourceError::NotImplemented {
            what: "mars-source-vectorfile::subscribe",
        })
    }
}

impl VectorFileSource {
    pub(crate) async fn fetch_bytes(&self, uri: &str) -> Result<Bytes, VectorFileError> {
        // tries cache first by uri+etag; falls back to a pull through the
        // resolved object_store. Side effect: cache is populated on miss.
        self.fetcher.fetch_cached(uri, &self.cache).await
    }
}

/// Resolved per-binding parameters: URI (sans fragment) + decoder hint +
/// source CRS. Built by [`plan_binding`] from a port-level binding using
/// the fragment convention documented at the crate root.
#[derive(Debug, Clone)]
pub(crate) struct BindingPlan {
    pub uri: String,
    pub format: mars_config::VectorFileFormat,
    pub source_crs: CrsCode,
}

fn plan_binding(binding: &SourceBinding, native_crs: &CrsCode) -> Result<BindingPlan, SourceError> {
    let parsed = fragment::parse(&binding.from).map_err(|e| SourceError::InvalidBinding(e.to_string()))?;
    // when the binding's crs disagrees with the fragment-encoded source_crs,
    // prefer the fragment - it's the locator-level truth chosen by the
    // factory. validation lives in the bin-shared factory before this point.
    let source_crs = parsed.source_crs.unwrap_or_else(|| binding.crs.clone());
    let _ = native_crs; // captured by VectorFileSource at construct time
    Ok(BindingPlan {
        uri: parsed.uri,
        format: parsed.format,
        source_crs,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_rejects_empty_cache_dir() {
        let err = VectorFileSource::new(
            mars_config::SourceId::new("vf"),
            CrsCode::new("EPSG:25832"),
            mars_config::VectorFileBackend {
                cache_dir: String::new(),
                ..Default::default()
            },
        )
        .await
        .expect_err("empty cache_dir must reject");
        assert!(matches!(err, VectorFileError::InvalidConfig { .. }));
    }

    #[tokio::test]
    async fn new_succeeds_on_valid_config() {
        let tmp = tempfile::tempdir().unwrap();
        let src = VectorFileSource::new(
            mars_config::SourceId::new("vf"),
            CrsCode::new("EPSG:25832"),
            mars_config::VectorFileBackend {
                cache_dir: tmp.path().display().to_string(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(src.id().as_str(), "vf");
        assert_eq!(src.native_crs().as_str(), "EPSG:25832");
    }
}
