#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_expr::parse;
use mars_source::SourceCollectionId;
use mars_types::CrsCode;

fn b() -> SourceBinding {
    SourceBinding::new(
        SourceCollectionId::new("c"),
        "public.t",
        "geom",
        "gid",
        vec!["name".into(), "kind".into()],
        CrsCode::new("EPSG:25832"),
    )
    .unwrap()
}

#[test]
fn feature_ids_query_quotes_identifiers() {
    let (sql, params) = build_feature_ids_query(&b()).unwrap();
    assert_eq!(
        sql,
        "SELECT \"gid\", ST_AsBinary(\"geom\") AS geom, \"name\", \"kind\" FROM \"public\".\"t\" WHERE \"gid\" = ANY($1::bigint[]) AND \"geom\" IS NOT NULL"
    );
    assert!(params.is_empty());
}

#[test]
fn binding_filter_lands_in_full_table_query() {
    let mut b = b();
    b.filter = Some(parse("name = 'x'").unwrap());
    let (sql, params) = build_full_table_query(&b).unwrap();
    assert!(sql.ends_with(" AND (\"name\" = $1)"), "{sql}");
    assert_eq!(params.len(), 1);
}

#[test]
fn binding_filter_starts_at_two_in_feature_ids_query() {
    let mut b = b();
    b.filter = Some(parse("name = 'x'").unwrap());
    let (sql, params) = build_feature_ids_query(&b).unwrap();
    assert!(sql.ends_with(" AND (\"name\" = $2)"), "{sql}");
    assert_eq!(params.len(), 1);
}
