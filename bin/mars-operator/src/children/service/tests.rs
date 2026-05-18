#![allow(clippy::unwrap_used)]

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
fn build_emits_default_prometheus_scrape_annotations() {
    let cr = test_support::cr("demo", "svc-ns");
    let svc = build(&cr, test_support::owner_ref()).unwrap();
    let ann = svc.metadata.annotations.unwrap();
    assert_eq!(ann.get("prometheus.io/scrape").map(String::as_str), Some("true"));
    assert_eq!(ann.get("prometheus.io/port").map(String::as_str), Some("8080"));
    assert_eq!(ann.get("prometheus.io/path").map(String::as_str), Some("/metrics"));
}

#[test]
fn build_omits_annotations_when_scrape_disabled_and_no_user_annotations() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.runtime.service.metrics_scrape = false;
    let svc = build(&cr, test_support::owner_ref()).unwrap();
    assert!(svc.metadata.annotations.is_none());
}

#[test]
fn build_propagates_user_annotations() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec.runtime.service.metrics_scrape = false;
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
    // disabling scrape suppresses all three defaults.
    assert!(!ann.contains_key("prometheus.io/scrape"));
}

#[test]
fn build_user_annotations_override_scrape_defaults() {
    let mut cr = test_support::cr("demo", "svc-ns");
    cr.spec
        .runtime
        .service
        .annotations
        .insert("prometheus.io/path".into(), "/foo".into());
    let svc = build(&cr, test_support::owner_ref()).unwrap();
    let ann = svc.metadata.annotations.unwrap();
    assert_eq!(ann.get("prometheus.io/path").map(String::as_str), Some("/foo"));
    // the other two defaults still fire.
    assert_eq!(ann.get("prometheus.io/scrape").map(String::as_str), Some("true"));
}
