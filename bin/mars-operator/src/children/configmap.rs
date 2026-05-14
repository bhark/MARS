//! ConfigMap builder. The rendered `mars.yaml` carries `${VAR}` placeholders
//! verbatim so the pod's `mars_config::load` substitutes against the real
//! environment at startup.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

use crate::children::labels::{self, config_map_name};
use crate::crd::MarsService;
use crate::error::Result;

/// Build the ConfigMap for a MarsService. Returns the configmap and the
/// content checksum (the operator surfaces this on pod-template annotations
/// so config edits roll the deployments).
pub(crate) fn build(cr: &MarsService, owner_ref: OwnerReference) -> Result<(ConfigMap, String)> {
    let svc = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| crate::error::OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr.metadata.namespace.clone();

    let yaml = crate::config::canonicalize_yaml(&cr.spec.config)?;
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
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::children::test_support;

    #[test]
    fn build_produces_mars_yaml_with_stable_checksum() {
        let cr = test_support::cr("demo", "svc-ns");
        let (cm, checksum) = build(&cr, test_support::owner_ref()).unwrap();
        assert_eq!(cm.metadata.name.as_deref(), Some("demo-config"));
        assert_eq!(cm.metadata.namespace.as_deref(), Some("svc-ns"));
        let data = cm.data.unwrap();
        let yaml = data.get("mars.yaml").unwrap();
        assert!(yaml.contains("service"));
        // checksum is stable across calls with identical input
        let (_, again) = build(&cr, test_support::owner_ref()).unwrap();
        assert_eq!(checksum, again);
        // and changes when config changes
        let mut cr2 = cr.clone();
        cr2.spec.config = serde_json::json!({"service": {"name": "other"}});
        let (_, different) = build(&cr2, test_support::owner_ref()).unwrap();
        assert_ne!(checksum, different);
    }
}
