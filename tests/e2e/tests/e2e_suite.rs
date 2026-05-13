//! single test binary. all #[test] fns live in submodules so they share one
//! process and one `kube::Client` singleton (see `mars_e2e_kind::cluster`).
//! cargo runs tests inside a binary serially under `--test-threads=1`.
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
