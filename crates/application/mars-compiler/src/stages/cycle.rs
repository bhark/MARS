//! cycle pipeline stages.
//!
//! during the staged migration this module re-exports each stage as it
//! lands; the orchestrator and `CycleCtx` arrive together in the final
//! commit. for now, `apply_cycle` in `lib.rs` still drives the sequence
//! inline and calls each extracted stage one by one.

pub(crate) mod reconcile_cadence;
