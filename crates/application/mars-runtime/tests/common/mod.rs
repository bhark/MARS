//! shared fixture for mars-runtime integration tests.
//!
//! the actual implementation lives in `mars_runtime::test_fixtures` so tests
//! and benches can share it. this module just re-exports the surface so
//! existing test files keep importing from `common::*` unchanged.

#![allow(dead_code, unreachable_pub, unused_imports)]

pub use mars_runtime::test_fixtures::{
    CapturingRenderer, Fixture, FixtureOptions, MultiLayerFixture, REQUEST_CRS, SleepingStore, StubEncoder,
    build_fixture, build_fixture_with, build_minimal_config, build_minimal_stylesheet, build_multi_layer_config,
    build_multi_layer_fixture, build_multi_layer_stylesheet, default_style,
};
