//! Safe wrapper around the `proj` C library (or the `proj` Rust crate that
//! itself wraps libproj). The boundary lives here so:
//!
//! - the FFI surface is centralised and reviewed in one place;
//! - the safe surface above it can be tested with mocks;
//! - swapping the underlying impl (e.g. to `proj4rs` pure-Rust) is a one-crate
//!   change.
//!
//! Per-thread `PJ_CONTEXT` handling and transformer caching land in Phase 1.

// no `forbid(unsafe_code)`: this crate exists to encapsulate FFI. It must
// remain the *only* place in the workspace that does so.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use mars_types::{Bbox, CrsCode};

#[derive(Debug, thiserror::Error)]
pub enum ProjError {
    #[error("unknown CRS: {0}")]
    UnknownCrs(String),
    #[error("transformation failed: {0}")]
    Transform(String),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

/// A reusable forward+inverse transformer between two CRSes.
#[derive(Debug)]
pub struct Transformer {
    _from: CrsCode,
    _to: CrsCode,
}

impl Transformer {
    /// Construct a transformer between two CRS authority codes.
    pub fn new(from: &CrsCode, to: &CrsCode) -> Result<Self, ProjError> {
        Ok(Self {
            _from: from.clone(),
            _to: to.clone(),
        })
    }

    /// Forward-transform a single point.
    pub fn transform_point(&self, _x: f64, _y: f64) -> Result<(f64, f64), ProjError> {
        Err(ProjError::NotImplemented {
            what: "mars-proj::Transformer::transform_point",
        })
    }

    /// Forward-transform a bounding box (axis-aligned).
    pub fn transform_bbox(&self, _bbox: Bbox) -> Result<Bbox, ProjError> {
        Err(ProjError::NotImplemented {
            what: "mars-proj::Transformer::transform_bbox",
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn transformer_construction_succeeds_in_stub() {
        // phase 0 sanity: constructing a transformer doesn't require libproj yet.
        let from = CrsCode::new("EPSG:25832");
        let to = CrsCode::new("EPSG:4326");
        Transformer::new(&from, &to).unwrap();
    }
}
