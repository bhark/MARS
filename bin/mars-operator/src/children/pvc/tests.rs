#![allow(clippy::unwrap_used)]

use super::*;
use crate::children::test_support;

#[test]
fn build_sets_storage_request_and_access_modes() {
    let pvc = build(
        PvcSpec {
            name: "demo-cache",
            namespace: Some("svc-ns"),
            labels: BTreeMap::new(),
            size: "8Gi",
            storage_class: "fast-ssd",
            access_modes: &["ReadWriteOnce".into()],
        },
        test_support::owner_ref(),
    );
    let spec = pvc.spec.unwrap();
    let req = spec.resources.unwrap().requests.unwrap();
    assert_eq!(req.get("storage").map(|q| q.0.as_str()), Some("8Gi"));
    assert_eq!(spec.access_modes.as_deref(), Some(&["ReadWriteOnce".to_string()][..]));
    assert_eq!(spec.storage_class_name.as_deref(), Some("fast-ssd"));
    assert_eq!(pvc.metadata.namespace.as_deref(), Some("svc-ns"));
    assert_eq!(pvc.metadata.owner_references.unwrap().len(), 1);
}

#[test]
fn empty_storage_class_omits_field() {
    // empty string must round to None; otherwise k8s rejects with
    // "spec.storageClassName cannot be empty".
    let pvc = build(
        PvcSpec {
            name: "demo-cache",
            namespace: None,
            labels: BTreeMap::new(),
            size: "1Gi",
            storage_class: "",
            access_modes: &["ReadWriteOnce".into()],
        },
        test_support::owner_ref(),
    );
    assert!(pvc.spec.unwrap().storage_class_name.is_none());
}
