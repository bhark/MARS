#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn fold_postgis_dsns_empty_is_placeholder() {
    let out: LiftedSourceDsn<'_> = fold_postgis_dsns(std::iter::empty::<&str>());
    assert_eq!(out, LiftedSourceDsn::Placeholder);
}

#[test]
fn fold_postgis_dsns_agreement_lifts() {
    let dsns = ["host=a dbname=x", "host=a dbname=x"];
    assert_eq!(
        fold_postgis_dsns(dsns.iter().copied()),
        LiftedSourceDsn::Lifted("host=a dbname=x")
    );
}

#[test]
fn fold_postgis_dsns_disagreement_is_mixed() {
    let dsns = ["host=a dbname=x", "host=b dbname=y"];
    assert_eq!(fold_postgis_dsns(dsns.iter().copied()), LiftedSourceDsn::Mixed);
}
