//! Compose a runnable `mars_config::Config` from a slim `MarsService`, the
//! cluster-scoped `MarsServiceCluster` catalog, and the resolved
//! `RenderDefinition`. Pure function: no kube I/O, no Secret reads. Secret
//! refs (DSN, S3 creds, bootstrap admin) are resolved upstream by the
//! reconcile wiring and injected into the cluster `sources_catalog` /
//! `artifact_store` payloads before this function is called.
//!
//! Algorithm mirrors `SPLIT_TODO_NOCOMMIT.md` Phase C: filter the catalog by
//! `spec.sources`, deserialise each opaque `serde_json::Value` payload into
//! the matching `mars_config` typed struct, build a `Deployment`, and call
//! `mars_config::compose` to merge with the render definition. Cross-cutting
//! validation (binding source ids resolve, etc.) is left to
//! `mars_config::validate` on the composed `Config`.

use std::collections::HashSet;

use mars_config::{
    Artifacts, Compiler, Config, Deployment, Observability, Render, RenderDefinition, Reprojection, Source, SourceId,
    compose,
};
use serde::de::DeserializeOwned;
use serde_json::Value as JsonValue;

use crate::crd::cluster::MarsServiceCluster;
use crate::crd::spec::MarsService;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ComposeError {
    #[error("MarsService references source id '{id}' which is not in the cluster catalog (known ids: [{known}])")]
    UnknownSourceId { id: String, known: String },

    #[error("MarsService.spec.sources is empty but the render definition has {count} layer(s) referencing sources")]
    EmptySourcesWithLayerRefs { count: usize },

    #[error("cluster catalog entry at index {index} does not deserialise into mars_config::Source: {source}")]
    InvalidCatalogEntry {
        index: usize,
        #[source]
        source: serde_json::Error,
    },

    #[error("cluster catalog entry at index {index} is missing the required 'id' field")]
    CatalogEntryMissingId { index: usize },

    #[error("cluster {field} does not deserialise into mars_config::{target}: {source}")]
    InvalidClusterField {
        field: &'static str,
        target: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

pub(crate) type Result<T> = std::result::Result<T, ComposeError>;

/// Compose the runtime `Config` for `svc` from `cluster` and the resolved
/// `def`. Pure — no I/O. Cross-cutting validation runs separately via
/// `mars_config::validate` on the returned `Config`.
pub(crate) fn compose_config(
    svc: &MarsService,
    cluster: &MarsServiceCluster,
    mut def: RenderDefinition,
) -> Result<Config> {
    let spec_sources = &svc.spec.sources;

    // empty spec.sources is only valid when no layer references any source
    if spec_sources.is_empty() {
        let layer_source_refs = count_layer_source_refs(&def);
        if layer_source_refs > 0 {
            return Err(ComposeError::EmptySourcesWithLayerRefs {
                count: layer_source_refs,
            });
        }
    }

    let sources = select_sources(&cluster.spec.sources_catalog, spec_sources)?;
    let artifacts: Artifacts = from_value_field(&cluster.spec.artifact_store, "artifactStore", "Artifacts")?;
    let cluster_reprojection: Reprojection =
        from_optional_field(cluster.spec.reprojection.as_ref(), "reprojection", "Reprojection")?;
    let observability: Observability =
        from_optional_field(cluster.spec.observability.as_ref(), "observability", "Observability")?;
    let render: Render = from_optional_field(cluster.spec.defaults.render.as_ref(), "defaults.render", "Render")?;
    let compiler: Compiler =
        from_optional_field(cluster.spec.defaults.compiler.as_ref(), "defaults.compiler", "Compiler")?;

    // service-side reprojection narrowing on MarsService.spec wins over the
    // render definition's own allowlist; intersection with the cluster
    // default happens inside mars_config::compose.
    if let Some(spec_repro) = svc.spec.reprojection.as_ref() {
        def.reprojection = from_value_field(spec_repro, "spec.reprojection", "Reprojection")?;
    }

    let deployment = Deployment {
        sources,
        artifacts,
        observability,
        render,
        compiler,
        reprojection: cluster_reprojection,
    };

    Ok(compose(def, deployment))
}

/// Filter the cluster catalog down to the entries named in `wanted`, preserving
/// the order in `wanted` so authors control the resulting `sources` order.
/// Errors on unknown ids and on payloads that fail to deserialise as a
/// `mars_config::Source`.
fn select_sources(catalog: &[JsonValue], wanted: &[String]) -> Result<Vec<Source>> {
    let mut indexed: Vec<(SourceId, &JsonValue, usize)> = Vec::with_capacity(catalog.len());
    for (i, entry) in catalog.iter().enumerate() {
        let id = entry
            .get("id")
            .and_then(JsonValue::as_str)
            .ok_or(ComposeError::CatalogEntryMissingId { index: i })?;
        indexed.push((SourceId::new(id), entry, i));
    }

    let known: HashSet<&SourceId> = indexed.iter().map(|(id, _, _)| id).collect();
    let mut out: Vec<Source> = Vec::with_capacity(wanted.len());
    for want in wanted {
        let want_id = SourceId::new(want.as_str());
        if !known.contains(&want_id) {
            let known_ids = indexed
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(ComposeError::UnknownSourceId {
                id: want.clone(),
                known: known_ids,
            });
        }
        // safe: known.contains(&want_id) implies a match exists
        let Some((_, value, index)) = indexed.iter().find(|(id, _, _)| id == &want_id) else {
            continue;
        };
        let parsed: Source =
            serde_json::from_value((*value).clone()).map_err(|e| ComposeError::InvalidCatalogEntry {
                index: *index,
                source: e,
            })?;
        out.push(parsed);
    }
    Ok(out)
}

fn from_value_field<T: DeserializeOwned>(value: &JsonValue, field: &'static str, target: &'static str) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|e| ComposeError::InvalidClusterField {
        field,
        target,
        source: e,
    })
}

fn from_optional_field<T: DeserializeOwned + Default>(
    value: Option<&JsonValue>,
    field: &'static str,
    target: &'static str,
) -> Result<T> {
    match value {
        Some(v) => from_value_field(v, field, target),
        None => Ok(T::default()),
    }
}

/// Count layer bindings (vector + raster ignored — raster uses a separate
/// `SourceCollectionId`). Used only to decide whether an empty `spec.sources`
/// is consistent with the render definition.
fn count_layer_source_refs(def: &RenderDefinition) -> usize {
    def.layers.iter().map(|l| l.sources.len()).sum()
}

#[cfg(test)]
mod tests;
