//! Produce the effective `mars_config::Config` payload for one reconcile.
//!
//! Resolves the `MarsServiceCluster` catalog + the `RenderDefinition` source,
//! composes them into a `Config`, and returns a `serde_json::Value` so the
//! ConfigMap builder and downstream validators consume one shape.

use kube::api::Api;
use mars_config::RenderDefinition;
use mars_definition_source::{DefinitionBytes, DefinitionSource};
use serde_json::Value as JsonValue;
use tracing::info;

use crate::compose::compose_config;
use crate::crd::cluster::MarsServiceCluster;
use crate::crd::definition::DefinitionSpec;
use crate::crd::spec::MarsService;
use crate::definition;
use crate::error::{OperatorError, Result};

/// Identity of the resolved render definition; surfaced via status conditions.
/// The variant label keeps the message human-readable; revision is the
/// adapter-stable identity (sha, etag, ...).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedDefinition {
    pub(crate) adapter: &'static str,
    pub(crate) revision: String,
}

/// Outcome of effective-config resolution.
#[derive(Debug)]
pub(crate) struct EffectiveConfig {
    pub(crate) config: JsonValue,
    pub(crate) definition: ResolvedDefinition,
}

/// Fetch cluster CR, resolve the definition source, fetch the payload, parse,
/// and compose into a `Config`.
pub(crate) async fn resolve(cr: &MarsService, kube: &kube::Client, ns: &str) -> Result<EffectiveConfig> {
    let clusters: Api<MarsServiceCluster> = Api::all(kube.clone());
    let cluster = clusters
        .get_opt(&cr.spec.cluster_ref.name)
        .await?
        .ok_or_else(|| OperatorError::ClusterNotFound(cr.spec.cluster_ref.name.clone()))?;

    let source = definition::resolve(&cr.spec.definition, ns, kube).await?;
    compose_from_source(cr, &cluster, &cr.spec.definition, source.as_ref()).await
}

/// Fetch, parse, and compose given pre-resolved cluster and definition source.
/// Pure of kube I/O: the cluster CR is passed by ref and the adapter is the
/// only async surface, so tests can inject `FakeDefinitionSource` without a
/// live API server.
pub(crate) async fn compose_from_source(
    cr: &MarsService,
    cluster: &MarsServiceCluster,
    definition_spec: &DefinitionSpec,
    source: &dyn DefinitionSource,
) -> Result<EffectiveConfig> {
    let DefinitionBytes { data, revision } = source.fetch().await?;
    let yaml = std::str::from_utf8(&data)
        .map_err(|e| OperatorError::DefinitionDecode(format!("definition payload not utf-8: {e}")))?;
    let def = RenderDefinition::from_yaml(yaml)
        .map_err(|e| OperatorError::DefinitionDecode(format!("parse RenderDefinition: {e}")))?;

    let adapter = definition_adapter_label(definition_spec);
    info!(adapter = adapter, revision = %revision, "resolved RenderDefinition");

    let config = compose_config(cr, cluster, def)?;
    let value = serde_json::to_value(&config)?;

    Ok(EffectiveConfig {
        config: value,
        definition: ResolvedDefinition { adapter, revision },
    })
}

fn definition_adapter_label(spec: &DefinitionSpec) -> &'static str {
    if spec.inline.is_some() {
        "inline"
    } else if spec.config_map_ref.is_some() {
        "configMapRef"
    } else if spec.git_ref.is_some() {
        "gitRef"
    } else if spec.s3_ref.is_some() {
        "s3Ref"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests;
