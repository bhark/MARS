//! Runtime ClusterIP Service.

use k8s_openapi::api::core::v1::{Service, ServicePort, ServiceSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::children::labels::{self, COMPONENT_RUNTIME, runtime_service_name};
use crate::crd::MarsService;
use crate::error::Result;

pub(crate) fn build(cr: &MarsService, owner_ref: OwnerReference) -> Result<Service> {
    let svc = cr
        .metadata
        .name
        .clone()
        .ok_or_else(|| crate::error::OperatorError::MissingField("metadata.name".into()))?;
    let ns = cr.metadata.namespace.clone();
    let labels_map = labels::labels(&svc, COMPONENT_RUNTIME);

    Ok(Service {
        metadata: ObjectMeta {
            name: Some(runtime_service_name(&svc)),
            namespace: ns,
            labels: Some(labels_map),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            type_: Some(cr.spec.runtime.service.service_type.clone()),
            selector: Some(labels::selector(&svc, COMPONENT_RUNTIME)),
            ports: Some(vec![ServicePort {
                name: Some("http".into()),
                port: cr.spec.runtime.service.port,
                target_port: Some(IntOrString::String("http".into())),
                protocol: Some("TCP".into()),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    })
}
