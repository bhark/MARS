//! Test-only adapters for the unified compile pipeline.
//!
//! [`FullScanCompileSession`] wraps any [`Source`] into a [`CompileSession`]
//! by streaming `fetch_full_table_streaming` for pass-1 summaries and
//! `fetch_by_feature_ids` for pass-2 hydration. Used by integration tests
//! and benches whose fakes don't open a real snapshot transaction; the
//! production postgres adapter overrides `Source::open_compile_session`
//! with the real REPEATABLE READ session.

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::stream::BoxStream;
use futures_util::StreamExt;
use mars_artifact::wkb_to_feature_geom;
use mars_source::{CompileSession, RowBytes, RowSummary, Source, SourceBinding, SourceError};

/// `CompileSession` wrapper that derives summaries by re-decoding WKB on
/// the client. Not snapshot-isolated — only correct when the underlying
/// `Source` is stable for the lifetime of the session (test fixtures, in
/// particular).
pub struct FullScanCompileSession<'a> {
    source: &'a dyn Source,
    binding: &'a SourceBinding,
}

impl<'a> FullScanCompileSession<'a> {
    /// Build a wrapper around `source` bound to `binding`.
    pub fn new(source: &'a dyn Source, binding: &'a SourceBinding) -> Self {
        Self { source, binding }
    }

    /// Box the wrapper as a `CompileSession` trait object — convenience for
    /// `Source::open_compile_session` overrides.
    pub fn boxed(source: &'a dyn Source, binding: &'a SourceBinding) -> Box<dyn CompileSession + 'a> {
        Box::new(Self::new(source, binding))
    }
}

#[async_trait]
impl<'a> CompileSession for FullScanCompileSession<'a> {
    async fn fetch_geometry_summary<'b>(
        &'b mut self,
    ) -> Result<BoxStream<'b, Result<RowSummary, SourceError>>, SourceError> {
        let stream = self.source.fetch_full_table_streaming(self.binding).await?;
        let mapped = stream.map(|item| item.and_then(summary_from_row_bytes));
        Ok(Box::pin(mapped))
    }

    async fn fetch_by_feature_ids<'b>(
        &'b mut self,
        ids: &'b [i64],
    ) -> Result<BoxStream<'b, Result<RowBytes, SourceError>>, SourceError> {
        self.source.fetch_by_feature_ids(self.binding, ids).await
    }

    fn geom_fingerprint(&self, geometry: &[u8]) -> u64 {
        fnv1a64(geometry)
    }

    async fn close(self: Box<Self>) -> Result<(), SourceError> {
        Ok(())
    }
}

/// Decode a `RowBytes` into a `RowSummary` by extracting the bbox via the
/// shared WKB decoder. `feature_id` casts saturating into i64 (the postgres
/// adapter validates the cast upstream; tests use small ids).
fn summary_from_row_bytes(row: RowBytes) -> Result<RowSummary, SourceError> {
    let feature = wkb_to_feature_geom(&row.geometry, row.feature_id).map_err(|e| SourceError::Backend {
        what: "wkb decode (test session)",
        source: Box::new(WkbWrap(e)),
    })?;
    let geom_byte_length = u32::try_from(row.geometry.len()).unwrap_or(u32::MAX);
    let feature_id = i64::try_from(row.feature_id).unwrap_or(i64::MAX);
    Ok(RowSummary {
        feature_id,
        bbox: feature.bbox,
        geom_byte_length,
        geom_digest: fnv1a64(geom_bytes(&row.geometry)),
    })
}

fn geom_bytes(b: &Bytes) -> &[u8] {
    b.as_ref()
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct WkbWrap(mars_artifact::WkbError);
