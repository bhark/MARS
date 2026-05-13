//! pipeline stages.
//!
//! the three orchestrators (snapshot, cycle, rebalance) live as siblings
//! here; cross-pipeline helpers live under [`shared`]. each stage exposes
//! an explicit input + output so stage-to-stage data flow is typed rather
//! than implicit in local bindings. the precedent is
//! `crates/adapters/mars-render/src/ops/mod.rs::dispatch` (EXTENDING.md
//! principle 4 - cohesion follows variant boundaries); the compiler maps
//! that principle to sequence steps via one stage per file rather than one
//! match arm per variant.

pub(crate) mod ctx;
pub(crate) mod shared;
pub(crate) mod snapshot;
