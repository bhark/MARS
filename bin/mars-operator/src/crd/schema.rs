//! schemars `schema_with` helpers for opaque CR fields that the apiserver
//! must accept verbatim and the operator validates at reconcile.

/// Schema function for `spec.config`: emits `{type: object,
/// x-kubernetes-preserve-unknown-fields: true}` so the apiserver accepts any
/// shape under that key. The operator does real validation at reconcile.
pub(super) fn preserve_unknown_fields(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut map = serde_json::Map::new();
    map.insert("type".into(), serde_json::Value::String("object".into()));
    map.insert(
        "x-kubernetes-preserve-unknown-fields".into(),
        serde_json::Value::Bool(true),
    );
    schemars::Schema::from(map)
}

/// Same as [`preserve_unknown_fields`] but for an `Option<serde_json::Value>`
/// field: marks the schema nullable so the apiserver does not require the
/// key to be present.
pub(super) fn preserve_unknown_fields_optional(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut map = serde_json::Map::new();
    map.insert(
        "type".into(),
        serde_json::Value::Array(vec![
            serde_json::Value::String("object".into()),
            serde_json::Value::String("null".into()),
        ]),
    );
    map.insert(
        "x-kubernetes-preserve-unknown-fields".into(),
        serde_json::Value::Bool(true),
    );
    schemars::Schema::from(map)
}

/// Schema for `Vec<serde_json::Value>` where each element is an opaque object
/// (validated at reconcile time, not at admission). Used for the
/// `extraVolumes` / `extraVolumeMounts` passthroughs whose typed schemas are
/// too large to mirror.
pub(super) fn preserve_unknown_fields_array(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut item = serde_json::Map::new();
    item.insert("type".into(), serde_json::Value::String("object".into()));
    item.insert(
        "x-kubernetes-preserve-unknown-fields".into(),
        serde_json::Value::Bool(true),
    );
    let mut map = serde_json::Map::new();
    map.insert("type".into(), serde_json::Value::String("array".into()));
    map.insert("items".into(), serde_json::Value::Object(item));
    schemars::Schema::from(map)
}
