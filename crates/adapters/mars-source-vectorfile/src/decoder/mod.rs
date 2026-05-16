//! Pluggable decoder registry.
//!
//! `Decoder` is the seam that lets new vector-file formats slot in
//! without touching the `Source` impl. v1 ships two pure-Rust decoders
//! (FlatGeobuf, GeoJSON); a GDAL FFI variant could implement the same
//! trait later without disturbing callers.
//!
//! Decoding runs on a `spawn_blocking` worker because the parsers and
//! `mars_proj::Transformer` are both synchronous and the transformer is
//! `!Send` (thread-local PJ context). The worker emits decoded +
//! reprojected `RowBytes` through a bounded mpsc channel.

use std::collections::HashSet;
use std::sync::Arc;

use bytes::Bytes;
use futures_core::stream::BoxStream;
use mars_config::VectorFileFormat;
use mars_source::{AttrValue, RowBytes, SourceBinding, SourceError, SourceRowKey};
use mars_types::CrsCode;

pub mod flatgeobuf;
pub mod geojson;

use crate::BindingPlan;
use crate::error::DecoderError;
use crate::reproject;

/// One decoded feature as produced by a decoder, prior to reprojection
/// and attribute filtering.
pub struct DecodedFeature {
    /// stable feature id (native FID when available; else 0-based row index).
    pub feature_id: u64,
    /// geometry in `source_crs` as ogc wkb.
    pub geometry_wkb: Bytes,
    /// attributes keyed by column name. The streamer projects this map
    /// to the binding's attribute list.
    pub attributes: std::collections::HashMap<String, AttrValue>,
}

/// Decoder seam. Implementations parse bytes for one [`VectorFileFormat`]
/// and emit a [`DecodedFeature`] stream synchronously through the supplied
/// sink. Returning early from `decode` (cancel) is up to the caller.
pub trait Decoder: Send + Sync {
    /// Identifier for diagnostics / metric labels.
    fn name(&self) -> &'static str;

    /// True if this decoder handles `format`.
    fn supports(&self, format: VectorFileFormat) -> bool;

    /// Decode `bytes` and emit features through `sink`. Synchronous;
    /// called from a `spawn_blocking` worker. `sink` returns `false` when
    /// the consumer dropped, and the decoder should stop promptly.
    fn decode(&self, bytes: &Bytes, sink: &mut dyn FnMut(DecodedFeature) -> bool) -> Result<(), DecoderError>;
}

/// Registry. Holds decoders in registration order; first match wins.
pub struct Registry {
    decoders: Vec<Box<dyn Decoder>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Registry")
            .field("decoders", &self.decoders.iter().map(|d| d.name()).collect::<Vec<_>>())
            .finish()
    }
}

impl Registry {
    /// Empty registry. Tests use this then `with()` to install a stub.
    #[must_use]
    pub fn new() -> Self {
        Self { decoders: Vec::new() }
    }

    /// Registry pre-populated with this adapter's pure-Rust decoders.
    #[must_use]
    pub fn with_builtin() -> Self {
        let mut r = Self::new();
        r.decoders.push(Box::new(flatgeobuf::FlatGeobufDecoder));
        r.decoders.push(Box::new(geojson::GeoJsonDecoder));
        r
    }

    /// Install an additional decoder. Order matters: earlier entries win.
    pub fn with(mut self, d: Box<dyn Decoder>) -> Self {
        self.decoders.push(d);
        self
    }

    /// Resolve the first decoder that supports `format`.
    pub fn resolve(&self, format: VectorFileFormat) -> Result<&dyn Decoder, DecoderError> {
        self.decoders
            .iter()
            .find(|d| d.supports(format))
            .map(|d| d.as_ref())
            .ok_or(DecoderError::NoDecoder(format))
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::with_builtin()
    }
}

/// Drive the decoder on a blocking worker, reproject geometries, project
/// attributes, and surface `RowBytes` through a bounded mpsc-backed
/// stream. `id_filter` short-circuits non-matching ids before they touch
/// reprojection / attribute projection.
pub(crate) async fn stream_rows(
    registry: Arc<Registry>,
    bytes: Bytes,
    binding: SourceBinding,
    plan: BindingPlan,
    native_crs: CrsCode,
    id_filter: Option<HashSet<i64>>,
) -> Result<BoxStream<'static, Result<RowBytes, SourceError>>, SourceError> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<RowBytes, SourceError>>(64);
    let format = plan.format;
    let source_crs = plan.source_crs.clone();

    tokio::task::spawn_blocking(move || {
        let send =
            |item: Result<RowBytes, SourceError>, tx: &tokio::sync::mpsc::Sender<_>| tx.blocking_send(item).is_ok();
        let decoder = match registry.resolve(format) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.blocking_send(Err(SourceError::backend("decoder resolve", e)));
                return;
            }
        };
        let xform = match reproject::transformer(&source_crs, &native_crs) {
            Ok(x) => x,
            Err(e) => {
                let _ = tx.blocking_send(Err(SourceError::backend("proj transformer", e)));
                return;
            }
        };
        let mut row_idx: u64 = 0;
        let result = decoder.decode(&bytes, &mut |feat| {
            let DecodedFeature {
                feature_id,
                geometry_wkb,
                attributes,
            } = feat;
            let effective_id = if feature_id == 0 { row_idx } else { feature_id };
            row_idx = row_idx.saturating_add(1);

            #[allow(clippy::cast_possible_wrap)]
            if let Some(filter) = id_filter.as_ref()
                && !filter.contains(&(effective_id as i64))
            {
                return true;
            }
            let reprojected = match reproject::reproject_wkb(&geometry_wkb, &xform) {
                Ok(b) => b,
                Err(e) => return send(Err(SourceError::backend("reproject", e)), &tx),
            };
            let attrs = project_attrs(&binding, attributes);
            let row = RowBytes {
                feature_id: effective_id,
                geometry: reprojected,
                attributes: attrs,
                row_key: SourceRowKey::ZERO,
            };
            send(Ok(row), &tx)
        });
        if let Err(e) = result {
            let _ = tx.blocking_send(Err(SourceError::backend("decoder", e)));
        }
    });

    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Ok(Box::pin(stream))
}

/// Same shape as `stream_rows` but emits only the feature-id stream.
pub(crate) async fn stream_feature_ids(
    registry: Arc<Registry>,
    bytes: Bytes,
    _binding: SourceBinding,
    plan: BindingPlan,
) -> Result<BoxStream<'static, Result<i64, SourceError>>, SourceError> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<i64, SourceError>>(64);
    let format = plan.format;
    tokio::task::spawn_blocking(move || {
        let decoder = match registry.resolve(format) {
            Ok(d) => d,
            Err(e) => {
                let _ = tx.blocking_send(Err(SourceError::backend("decoder resolve", e)));
                return;
            }
        };
        let mut row_idx: u64 = 0;
        let _ = decoder.decode(&bytes, &mut |feat| {
            let effective_id = if feat.feature_id == 0 { row_idx } else { feat.feature_id };
            row_idx = row_idx.saturating_add(1);
            #[allow(clippy::cast_possible_wrap)]
            let id = effective_id as i64;
            tx.blocking_send(Ok(id)).is_ok()
        });
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) });
    Ok(Box::pin(stream))
}

fn project_attrs(
    binding: &SourceBinding,
    mut decoded: std::collections::HashMap<String, AttrValue>,
) -> Vec<(String, AttrValue)> {
    binding
        .attributes
        .iter()
        .map(|name| {
            let v = decoded.remove(name).unwrap_or(AttrValue::Null);
            (name.clone(), v)
        })
        .collect()
}
