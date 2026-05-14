//! Parity harness shared helpers. Re-exports the pixel-diff utilities from
//! `mars-test-support` so each parity scenario (`tests/<dataset>.rs`) imports
//! them through one stable path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub use mars_test_support::{
    Channels, Decoded, DiffError, DiffReport, assert_within_tolerance, decode, diff_pngs, diff_pngs_with_radius,
};
