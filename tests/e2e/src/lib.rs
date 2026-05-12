//! kind-based e2e harness for MARS. drives an already-running kind cluster
//! (lifecycle owned by `scripts/run-e2e.sh`) and asserts production-equivalent
//! behaviour against the deployed operator chart + MarsService.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod cluster;
pub mod deploy;
pub mod fixtures;
pub mod garage;
pub mod http;
pub mod metrics;
pub mod namespace;
pub mod wait;

pub use mars_test_support as diff;
