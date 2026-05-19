//! ConfigMap builder. The rendered `mars.yaml` carries `${VAR}` placeholders
//! verbatim so the pod's `mars_config::load` substitutes against the real
//! environment at startup.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use serde_json::Value as JsonValue;

use crate::children::labels::{self, config_map_name};
use crate::crd::spec::MarsService;
use crate::error::Result;

/// Build the ConfigMap for a MarsService. Returns the configmap and the
/// content checksum (the operator surfaces this on pod-template annotations
/// so config edits roll the deployments). `config` is the effective config
/// payload — on the legacy path it is `cr.spec.config`; on the new path it is
/// the operator-composed `mars_config::Config` serialised back to a `Value`.
pub(crate) fn build(cr: &MarsService, config: &JsonValue, owner_ref: OwnerReference) -> Result<(ConfigMap, String)> {
    let svc = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| crate::error::OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr.metadata.namespace.clone();

    let yaml = crate::config::canonicalize_yaml(config)?;
    let checksum = blake3::hash(yaml.as_bytes()).to_hex().to_string();

    let mut data: BTreeMap<String, String> = BTreeMap::new();
    data.insert("mars.yaml".into(), yaml);

    let cm = ConfigMap {
        metadata: ObjectMeta {
            name: Some(config_map_name(&svc)),
            namespace: ns,
            labels: Some(labels::labels(&svc, "config")),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        data: Some(data),
        ..Default::default()
    };

    Ok((cm, checksum))
}

#[cfg(test)]
mod tests;
