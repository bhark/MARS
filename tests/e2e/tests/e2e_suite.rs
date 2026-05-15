//! single test binary. all #[test] fns live in submodules so they share one
//! process; cargo runs tests inside a binary serially under `--test-threads=1`.
//! each test builds its own `kube::Client` because the kube/tower buffer worker
//! is bound to the runtime it was spawned on (see `mars_e2e_kind::cluster`).
//!
//! files live under `tests/e2e_suite/` rather than as siblings so they aren't
//! mistaken for additional test targets even if `autotests` is later turned
//! back on.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "e2e_suite/scenario.rs"]
mod scenario;

#[path = "e2e_suite/a_bootstrap.rs"]
mod a_bootstrap;

#[path = "e2e_suite/b_incremental.rs"]
mod b_incremental;

#[path = "e2e_suite/c_rendering.rs"]
mod c_rendering;

#[path = "e2e_suite/d_wmts.rs"]
mod d_wmts;

#[path = "e2e_suite/e_resilience.rs"]
mod e_resilience;

#[path = "e2e_suite/f_image_pattern.rs"]
mod f_image_pattern;

#[path = "e2e_suite/g_bootstrap.rs"]
mod g_bootstrap;
