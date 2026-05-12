//! shared helpers for image-diff tests. thin re-export of mars-test-support;
//! kept as a `mod common;` so existing integration tests keep their imports.

// per-target subset usage: each integration test only consumes a slice of the
// re-export, so unused_imports fires per crate compile despite `pub use`.
#![allow(dead_code, unreachable_pub, unused_imports)]

#[cfg(feature = "mapserver-diff")]
pub mod perf_report;

pub use mars_test_support::{
    Channels, Decoded, DiffError, DiffReport, assert_within_tolerance, decode, diff_pngs, diff_pngs_with_radius,
};
