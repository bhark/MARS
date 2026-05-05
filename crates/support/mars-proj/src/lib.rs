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

use std::ffi::CString;

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

/// Returns `true` when `code` is a projected (metric) CRS.
///
/// Uses PROJ introspection so any authority code known to the local PROJ
/// database is accepted; no hard-coded allowlist is required.
pub fn is_projected(code: &CrsCode) -> Result<bool, ProjError> {
    let definition =
        CString::new(code.as_str()).map_err(|e| ProjError::UnknownCrs(format!("invalid CRS string: {e}")))?;

    // SAFETY: proj_context_create / proj_create / proj_get_type / proj_destroy
    // / proj_context_destroy are the standard PROJ C lifecycle. `definition`
    // is a valid NUL-terminated string allocated on the Rust heap.
    unsafe {
        let ctx = proj_sys::proj_context_create();
        if ctx.is_null() {
            return Err(ProjError::Transform("failed to create PROJ context".into()));
        }
        let pj = proj_sys::proj_create(ctx, definition.as_ptr());
        let result = if pj.is_null() {
            Err(ProjError::UnknownCrs(code.to_string()))
        } else {
            let ty = proj_sys::proj_get_type(pj);
            proj_sys::proj_destroy(pj);
            Ok(ty == proj_sys::PJ_TYPE_PJ_TYPE_PROJECTED_CRS)
        };
        proj_sys::proj_context_destroy(ctx);
        result
    }
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
