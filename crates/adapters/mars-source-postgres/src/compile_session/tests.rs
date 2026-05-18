#![allow(clippy::unwrap_used)]

use super::*;

#[test]
fn summary_sql_is_well_formed() {
    let b = SourceBinding::new(
        mars_source::SourceCollectionId::new("c"),
        "public.t",
        "geom",
        "gid",
        vec![],
        mars_types::CrsCode::new("EPSG:25832"),
    )
    .unwrap();
    let (sql, params) = build_summary_query(&b).unwrap();
    assert!(sql.contains("ST_XMin(\"geom\")::float4"));
    assert!(sql.contains("octet_length(ST_AsBinary(\"geom\"))::int4"));
    assert!(sql.contains("tableoid::oid"));
    assert!(sql.contains("ctid::text"));
    assert!(!sql.contains("md5"));
    assert!(sql.contains("FROM \"public\".\"t\""));
    assert!(sql.contains("WHERE \"geom\" IS NOT NULL"));
    assert!(params.is_empty());
}

#[test]
fn full_table_sql_is_well_formed() {
    let b = SourceBinding::new(
        mars_source::SourceCollectionId::new("c"),
        "public.t",
        "geom",
        "gid",
        vec!["name".into(), "kind".into()],
        mars_types::CrsCode::new("EPSG:25832"),
    )
    .unwrap();
    let (sql, params) = build_full_table_query(&b).unwrap();
    assert_eq!(
        sql,
        "SELECT \"gid\", ST_AsBinary(\"geom\") AS geom, \"name\", \"kind\", tableoid::oid, ctid::text FROM \"public\".\"t\" WHERE \"geom\" IS NOT NULL"
    );
    assert!(!sql.contains("ORDER BY"));
    assert!(params.is_empty());
}

#[test]
fn parse_ctid_round_trips() {
    let (b, o) = super::parse_ctid("(0,1)").unwrap();
    assert_eq!((b, o), (0, 1));
    let (b, o) = super::parse_ctid("(4294967295,65535)").unwrap();
    assert_eq!((b, o), (u32::MAX, u16::MAX));
}

#[test]
fn parse_ctid_rejects_garbage() {
    assert!(super::parse_ctid("0,1").is_err());
    assert!(super::parse_ctid("(0)").is_err());
    assert!(super::parse_ctid("(x,y)").is_err());
}

#[test]
fn pack_row_key_layout() {
    let k = super::pack_row_key(0x0011_2233, 0xaabb_ccdd, 0xeeff);
    let b = k.as_bytes();
    assert_eq!(&b[0..4], &0x0011_2233u32.to_be_bytes());
    assert_eq!(&b[4..8], &0xaabb_ccddu32.to_be_bytes());
    assert_eq!(&b[8..10], &0xeeffu16.to_be_bytes());
    assert_eq!(&b[10..], &[0u8; 6]);
}

#[test]
fn pack_row_key_lex_order_matches_numeric_order() {
    let lo = super::pack_row_key(1, 1, 1);
    let mid = super::pack_row_key(1, 1, 2);
    let hi = super::pack_row_key(1, 2, 0);
    let top = super::pack_row_key(2, 0, 0);
    assert!(lo.as_bytes() < mid.as_bytes());
    assert!(mid.as_bytes() < hi.as_bytes());
    assert!(hi.as_bytes() < top.as_bytes());
}
