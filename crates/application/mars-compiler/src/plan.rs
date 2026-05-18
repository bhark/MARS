//! page enumeration plan for the snapshot compiler.
//!
//! a [`BootstrapPlan`] is the deduplicated set of bindings that the snapshot
//! will materialise. derived from a validated [`mars_config::Config`]: every
//! [`mars_config::SourceBinding`] across every layer collapses to a single
//! [`BindingPlan`] keyed by the resolved [`mars_types::BindingId`] (the
//! `from:` string for postgis table bindings, a content-hash for `sql:` or
//! `uri:` bindings). layers that reference the same source see the same
//! binding, and therefore share page artifacts. divergent shape on the
//! same id raises [`PlanError::ConflictingBinding`].
//!
//! the planner does NOT walk source rows or talk to postgres -- it only
//! decides what set of (binding, level) slices the snapshot has to emit.
//!
//! split into focused submodules:
//! - [`types`] holds the plain plan shapes ([`BindingPlan`], [`LayerPlan`], ...)
//! - [`error`] holds [`PlanError`]
//! - [`binding`] resolves config bindings to (locator, id) and lowers
//!   per-level decimation
//! - [`layer`] parses class `when:`, label `text:`, and resolves label style
//!   refs
//! - [`dedup`] enforces shape consistency across shared bindings / layers
//! - [`build`] is the orchestrator that walks the [`mars_config::Config`]

mod binding;
mod build;
mod dedup;
mod error;
mod layer;
mod types;

pub use build::build_bootstrap_plan;
pub use error::PlanError;
pub use types::{BindingPlan, BootstrapPlan, ClassPlan, LayerLabelPlan, LayerPlan, LevelPlan};

#[cfg(test)]
mod tests;
