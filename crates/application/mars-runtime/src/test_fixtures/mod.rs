//! shared in-memory fixtures for runtime tests and benches.
//!
//! builds a minimal mars service end-to-end (config + manifest + page +
//! sidecars) backed by `mars-store::mem` stand-ins, so callers do not need
//! a real object store, cache, or compiler. all stand-ins are port-level;
//! no concrete adapter crate is referenced and the hexagonal-architecture
//! script stays green.
//!
//! gated on `feature = "test-fixtures"`. integration tests and benches
//! both opt in via `required-features` on their respective `[[test]]` /
//! `[[bench]]` Cargo declarations.

mod config;
mod multi;
mod single;
mod sleeping_store;

pub const REQUEST_CRS: &str = "EPSG:25832";

pub use config::{build_minimal_config, build_minimal_stylesheet, default_style};
pub use mars_test_support::port_fakes::{CapturingRenderer, StubEncoder};
pub use multi::{MultiLayerFixture, build_multi_layer_config, build_multi_layer_fixture, build_multi_layer_stylesheet};
pub use single::{Fixture, FixtureOptions, build_fixture, build_fixture_with};
pub use sleeping_store::SleepingStore;
