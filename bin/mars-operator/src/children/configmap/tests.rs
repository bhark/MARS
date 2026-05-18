#![allow(clippy::unwrap_used)]

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
