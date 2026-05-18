//! MarsService CustomResource definition, split by concern.
//!
//! - `spec` - top-level `MarsServiceSpec` (carries the `CustomResource`
//!   derive), `MarsServiceStatus`, `Condition`, and `print_crd`.
//! - `compiler` - compiler workload fields.
//! - `runtime` - runtime workload fields (incl. PDB, service, cache).
//! - `bootstrap` - postgres catalog bootstrap fields.
//! - `storage` - artifact-store PVC fields.
//! - `k8s` - shared schema-friendly mirrors of upstream k8s types.
//! - `schema` - `schemars` helpers for opaque preserve-unknown-fields.
//! - `defaults` - defaults shared across more than one submodule.

pub(crate) mod bootstrap;
pub(crate) mod cluster;
pub(crate) mod compiler;
mod defaults;
pub(crate) mod k8s;
pub(crate) mod runtime;
mod schema;
pub(crate) mod spec;
pub(crate) mod storage;
