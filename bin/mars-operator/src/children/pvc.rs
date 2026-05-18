//! PVC builder. Create-only contract: the operator never patches existing
//! PVCs in v1 - expansion/StorageClass changes must surface to the user as a
//! Degraded condition rather than getting silently applied.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, PersistentVolumeClaimSpec, VolumeResourceRequirements};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};

pub(crate) struct PvcSpec<'a> {
    pub(crate) name: &'a str,
    pub(crate) namespace: Option<&'a str>,
    pub(crate) labels: BTreeMap<String, String>,
    pub(crate) size: &'a str,
    pub(crate) storage_class: &'a str,
    pub(crate) access_modes: &'a [String],
}

pub(crate) fn build(spec: PvcSpec<'_>, owner_ref: OwnerReference) -> PersistentVolumeClaim {
    let mut requests: BTreeMap<String, Quantity> = BTreeMap::new();
    requests.insert("storage".into(), Quantity(spec.size.to_string()));

    let storage_class = if spec.storage_class.is_empty() {
        None
    } else {
        Some(spec.storage_class.to_string())
    };

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some(spec.name.to_string()),
            namespace: spec.namespace.map(str::to_string),
            labels: Some(spec.labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(spec.access_modes.to_vec()),
            storage_class_name: storage_class,
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
