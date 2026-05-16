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
    let user_annotations = cr.spec.runtime.service.annotations.clone();
    let annotations = if user_annotations.is_empty() {
        None
    } else {
        Some(user_annotations)
    };

    Ok(Service {
        metadata: ObjectMeta {
            name: Some(runtime_service_name(&svc)),
            namespace: ns,
            labels: Some(labels_map),
            annotations,
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::children::test_support;

    #[test]
    fn build_targets_runtime_pods_only() {
        let cr = test_support::cr("demo", "svc-ns");
        let svc = build(&cr, test_support::owner_ref()).unwrap();
        let spec = svc.spec.unwrap();
        let selector = spec.selector.unwrap();
        assert_eq!(
            selector.get("app.kubernetes.io/component").map(String::as_str),
            Some(COMPONENT_RUNTIME)
        );
        assert_eq!(
            selector.get("app.kubernetes.io/instance").map(String::as_str),
            Some("demo")
        );
        let ports = spec.ports.unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].name.as_deref(), Some("http"));
        assert_eq!(ports[0].port, 8080);
        // target_port must reference the container port by name so the runtime
        // container can rename its port without breaking the service.
        assert!(matches!(&ports[0].target_port, Some(IntOrString::String(s)) if s == "http"));
    }

    #[test]
    fn build_omits_annotations_when_unset() {
        let cr = test_support::cr("demo", "svc-ns");
        let svc = build(&cr, test_support::owner_ref()).unwrap();
        assert!(svc.metadata.annotations.is_none());
    }

    #[test]
    fn build_propagates_user_annotations() {
        let mut cr = test_support::cr("demo", "svc-ns");
        cr.spec.runtime.service.annotations.insert(
            "traefik.ingress.kubernetes.io/router.entrypoints".into(),
            "websecure".into(),
        );
        let svc = build(&cr, test_support::owner_ref()).unwrap();
        let ann = svc.metadata.annotations.unwrap();
        assert_eq!(
            ann.get("traefik.ingress.kubernetes.io/router.entrypoints")
                .map(String::as_str),
            Some("websecure")
        );
    }
}
