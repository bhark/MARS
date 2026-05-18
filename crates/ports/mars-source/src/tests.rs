#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn binding_constructor_accepts_valid() {
    let b = SourceBinding::new(
        SourceCollectionId::new("roads"),
        "public.roads",
        "geom",
        "gid",
        vec!["name".into(), "class".into()],
        CrsCode::new("EPSG:25832"),
    )
    .unwrap();
    assert_eq!(b.collection.as_str(), "roads");
    assert_eq!(b.from, "public.roads");
    assert_eq!(b.geometry_field, "geom");
    assert_eq!(b.id_field, "gid");
    assert_eq!(b.attributes, vec!["name".to_string(), "class".to_string()]);
    assert_eq!(b.crs.as_str(), "EPSG:25832");
}

#[test]
fn binding_rejects_duplicate_attribute() {
    let r = SourceBinding::new(
        SourceCollectionId::new("c"),
        "s.t",
        "g",
        "id",
        vec!["a".into(), "a".into()],
        CrsCode::new("EPSG:4326"),
    );
    assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
}

#[test]
fn binding_rejects_empty_field() {
    let r = SourceBinding::new(
        SourceCollectionId::new("c"),
        "",
        "g",
        "id",
        vec![],
        CrsCode::new("EPSG:4326"),
    );
    assert!(matches!(r, Err(SourceError::InvalidBinding(_))));
}

// phase-c will reintroduce page-keyed Source surface and its tests.
