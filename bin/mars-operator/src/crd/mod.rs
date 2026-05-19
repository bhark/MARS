//! MarsService CustomResource definition, split by concern.
//!
//! - `spec` - top-level `MarsServiceSpec` (carries the `CustomResource`
//!   derive), `MarsServiceStatus`, `Condition`, and `print_crd`.
//! - `cluster` - cluster-scoped `MarsServiceCluster` catalog CR.
//! - `compiler` - compiler workload fields.
//! - `runtime` - runtime workload fields (incl. PDB, service, cache).
//! - `definition` - the sibling-key oneOf for resolving a `RenderDefinition`.
//! - `bootstrap` - reusable secret-ref + teardown-policy types shared with the
//!   cluster-side bootstrap reconciler.
//! - `k8s` - shared schema-friendly mirrors of upstream k8s types.
//! - `schema` - `schemars` helpers for opaque preserve-unknown-fields.
//! - `defaults` - defaults shared across more than one submodule.

pub(crate) mod bootstrap;
pub(crate) mod cluster;
pub(crate) mod compiler;
mod defaults;
pub(crate) mod definition;
pub(crate) mod k8s;
pub(crate) mod runtime;
mod schema;
pub(crate) mod spec;
