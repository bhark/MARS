#![allow(clippy::unwrap_used)]

use super::*;
use crate::children::test_support;

#[test]
fn build_produces_mars_yaml_with_stable_checksum() {
    let cr = test_support::cr("demo", "svc-ns");
    let config = serde_json::json!({"service": {"name": "demo"}});
    let (cm, checksum) = build(&cr, &config, test_support::owner_ref()).unwrap();
    assert_eq!(cm.metadata.name.as_deref(), Some("demo-config"));
    assert_eq!(cm.metadata.namespace.as_deref(), Some("svc-ns"));
    let data = cm.data.unwrap();
    let yaml = data.get("mars.yaml").unwrap();
    assert!(yaml.contains("service"));
    // stable across calls with identical input
    let (_, again) = build(&cr, &config, test_support::owner_ref()).unwrap();
    assert_eq!(checksum, again);
    // changes when input changes
    let other = serde_json::json!({"service": {"name": "other"}});
    let (_, different) = build(&cr, &other, test_support::owner_ref()).unwrap();
    assert_ne!(checksum, different);
}
