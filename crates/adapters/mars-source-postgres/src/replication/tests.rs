#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn topology_lookup() {
    let t = ReplicationTopology {
        collections: vec![CollectionTopology {
            collection: "roads".into(),
            schema: "public".into(),
            table: "roads_t".into(),
            geometry_column: "geom".into(),
            id_column: "gid".into(),
        }],
    };
    assert!(t.find("public", "roads_t").is_some());
    assert!(t.find("public", "buildings").is_none());
}

// cells_for_bbox tests retired.
