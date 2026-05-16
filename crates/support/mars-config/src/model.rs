//! Typed serde model for MARS service YAML.
//!
//! Unit-suffixed scalars (`50GiB`, `4096m`, `5min`) are deserialised as
//! strings here and parsed in [`crate::units`] when accessed; the wire form
//! is preserved verbatim so a config can be round-tripped without loss.

mod artifacts;
mod compiler;
mod config;
mod interfaces;
mod layer;
mod observability;
mod ows;
mod render;
mod reprojection;
mod scales;
mod service;
mod source;
mod style;
mod tile_matrix;
mod wms;

pub use artifacts::*;
pub use compiler::*;
pub use config::*;
pub use interfaces::*;
pub use layer::*;
pub use observability::*;
pub use ows::*;
pub use render::*;
pub use reprojection::*;
pub use scales::*;
pub use service::*;
pub use source::*;
pub use style::*;
pub use tile_matrix::*;
pub use wms::*;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests;
