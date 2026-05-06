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

use std::ffi::{CStr, CString};
use std::sync::Mutex;

use mars_types::{Bbox, CrsCode};
use proj_sys::{PJ, PJ_CONTEXT};

/// default number of segments per bbox edge used by densified bbox transforms.
/// 10 segments => 11 sample points per edge; matches GDAL's typical default.
pub const DEFAULT_DENSIFY_SEGMENTS: usize = 10;

#[derive(Debug, thiserror::Error)]
pub enum ProjError {
    #[error("unknown CRS: {0}")]
    UnknownCrs(String),
    #[error("transformation failed: {0}")]
    Transform(String),
    #[error("not implemented: {what}")]
    NotImplemented { what: &'static str },
}

thread_local! {
    // per-thread context used for one-shot lifecycle ops (is_projected) and as
    // the construction context for per-Transformer PJ handles.
    static PROJ_CTX: ContextHandle = ContextHandle::new();
}

struct ContextHandle(*mut PJ_CONTEXT);

impl ContextHandle {
    fn new() -> Self {
        // SAFETY: proj_context_create is the documented PROJ entry point and
        // returns null on failure; callers check the pointer before use.
        let ctx = unsafe { proj_sys::proj_context_create() };
        Self(ctx)
    }

    fn as_ptr(&self) -> *mut PJ_CONTEXT {
        self.0
    }
}

impl Drop for ContextHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: 0.0 was created by proj_context_create on this thread and is not used after this point.
            unsafe { proj_sys::proj_context_destroy(self.0) };
        }
    }
}

/// Returns `true` when `code` is a projected (metric) CRS.
///
/// Uses PROJ introspection so any authority code known to the local PROJ
/// database is accepted; no hard-coded allowlist is required.
pub fn is_projected(code: &CrsCode) -> Result<bool, ProjError> {
    let definition =
        CString::new(code.as_str()).map_err(|e| ProjError::UnknownCrs(format!("invalid CRS string: {e}")))?;

    PROJ_CTX.with(|ctx| {
        let ctx_ptr = ctx.as_ptr();
        if ctx_ptr.is_null() {
            return Err(ProjError::Transform("failed to create PROJ context".into()));
        }
        // SAFETY: ctx_ptr is a live PROJ context owned by this thread; definition
        // is a valid NUL-terminated C string. proj_create returns null on
        // failure and a heap-owned PJ on success which we destroy below.
        unsafe {
            let pj = proj_sys::proj_create(ctx_ptr, definition.as_ptr());
            if pj.is_null() {
                return Err(ProjError::UnknownCrs(code.to_string()));
            }
            let ty = proj_sys::proj_get_type(pj);
            proj_sys::proj_destroy(pj);
            Ok(ty == proj_sys::PJ_TYPE_PJ_TYPE_PROJECTED_CRS)
        }
    })
}

/// Construction-time options for `Transformer`.
#[derive(Debug, Clone, Copy)]
pub struct TransformerOptions {
    /// Segments per edge used when densifying bbox transforms. Must be >= 1.
    pub densify_segments: usize,
}

impl Default for TransformerOptions {
    fn default() -> Self {
        Self {
            densify_segments: DEFAULT_DENSIFY_SEGMENTS,
        }
    }
}

/// A reusable forward transformer between two CRSes.
///
/// The underlying `*mut PJ` is bound to the thread-local `PJ_CONTEXT` that was
/// active at construction time. PROJ docs are explicit that a PJ must only be
/// used with its construction context, so transforms are serialised through a
/// `Mutex` for v1. Render workers are tile-parallel above this layer, so
/// per-Transformer contention is acceptable; a per-thread pool can land later
/// if profiling justifies it.
#[derive(Debug)]
pub struct Transformer {
    inner: Mutex<TransformerInner>,
    densify_segments: usize,
}

#[derive(Debug)]
struct TransformerInner {
    pj: *mut PJ,
}

// no Send/Sync: the inner *mut PJ is bound to its construction-time
// thread_local context. v1 keeps Transformer single-thread-bound; cross-thread
// reuse would require lifting to a per-thread pool.
//
// compile-time guard: `*mut PJ` is `!Send + !Sync` automatically, but a future
// derive or unsafe impl could relax this and segfault via the thread-local
// PROJ_CTX. the trait disambiguation trick below errors with `multiple
// applicable items in scope` if Transformer ever gains Send or Sync.
const _: fn() = || {
    struct Invalid;
    trait AmbiguousIfSend<A> {
        fn _check() {}
    }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}
    <Transformer as AmbiguousIfSend<_>>::_check();

    trait AmbiguousIfSync<A> {
        fn _check() {}
    }
    impl<T: ?Sized> AmbiguousIfSync<()> for T {}
    impl<T: ?Sized + Sync> AmbiguousIfSync<Invalid> for T {}
    <Transformer as AmbiguousIfSync<_>>::_check();
};

impl Drop for TransformerInner {
    fn drop(&mut self) {
        if !self.pj.is_null() {
            // SAFETY: pj was created by proj_create_crs_to_crs (or normalize) and
            // is not used after this point; Mutex guarantees no concurrent use.
            unsafe { proj_sys::proj_destroy(self.pj) };
        }
    }
}

impl Transformer {
    /// Construct a transformer between two CRS authority codes with defaults.
    pub fn new(from: &CrsCode, to: &CrsCode) -> Result<Self, ProjError> {
        Self::with_options(from, to, TransformerOptions::default())
    }

    /// Construct a transformer with explicit options.
    pub fn with_options(from: &CrsCode, to: &CrsCode, opts: TransformerOptions) -> Result<Self, ProjError> {
        if opts.densify_segments == 0 {
            return Err(ProjError::Transform("densify_segments must be >= 1".into()));
        }
        let from_c = CString::new(from.as_str())
            .map_err(|e| ProjError::UnknownCrs(format!("invalid source CRS string: {e}")))?;
        let to_c =
            CString::new(to.as_str()).map_err(|e| ProjError::UnknownCrs(format!("invalid target CRS string: {e}")))?;

        PROJ_CTX.with(|ctx| {
            let ctx_ptr = ctx.as_ptr();
            if ctx_ptr.is_null() {
                return Err(ProjError::Transform("failed to create PROJ context".into()));
            }
            // SAFETY: ctx_ptr is a live thread-local context; from_c/to_c are
            // valid NUL-terminated C strings. proj_create_crs_to_crs returns
            // null on failure. The returned PJ is then normalized for
            // visualization (lon/lat axis order independent of CRS metadata),
            // matching what every WMS-style caller in this codebase expects.
            unsafe {
                let raw =
                    proj_sys::proj_create_crs_to_crs(ctx_ptr, from_c.as_ptr(), to_c.as_ptr(), std::ptr::null_mut());
                if raw.is_null() {
                    return Err(ProjError::UnknownCrs(format!("{from} -> {to}")));
                }
                let normalized = proj_sys::proj_normalize_for_visualization(ctx_ptr, raw);
                proj_sys::proj_destroy(raw);
                if normalized.is_null() {
                    return Err(ProjError::Transform(format!(
                        "proj_normalize_for_visualization failed for {from} -> {to}"
                    )));
                }
                Ok(Self {
                    inner: Mutex::new(TransformerInner { pj: normalized }),
                    densify_segments: opts.densify_segments,
                })
            }
        })
    }

    /// Forward-transform a single point.
    pub fn transform_point(&self, x: f64, y: f64) -> Result<(f64, f64), ProjError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| ProjError::Transform("transformer mutex poisoned".into()))?;
        transform_one(guard.pj, x, y)
    }

    /// Forward-transform a bounding box, densifying each edge before computing
    /// the axis-aligned bounding box of the transformed samples.
    pub fn transform_bbox(&self, bbox: Bbox) -> Result<Bbox, ProjError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| ProjError::Transform("transformer mutex poisoned".into()))?;
        densified_bbox(guard.pj, bbox, self.densify_segments)
    }
}

fn transform_one(pj: *mut PJ, x: f64, y: f64) -> Result<(f64, f64), ProjError> {
    // SAFETY: pj is a non-null PJ handle held under Mutex; proj_coord/proj_trans
    // are pure value-in/value-out C entry points. We re-check errno to surface
    // transform failures (PROJ returns HUGE_VAL on error rather than panicking).
    unsafe {
        let input = proj_sys::proj_coord(x, y, 0.0, 0.0);
        let out = proj_sys::proj_trans(pj, proj_sys::PJ_DIRECTION_PJ_FWD, input);
        let err = proj_sys::proj_errno(pj);
        if err != 0 {
            let msg_ptr = proj_sys::proj_errno_string(err);
            let msg = if msg_ptr.is_null() {
                format!("PROJ errno {err}")
            } else {
                CStr::from_ptr(msg_ptr).to_string_lossy().into_owned()
            };
            // reset errno on the handle so subsequent transforms aren't
            // contaminated by this failure.
            proj_sys::proj_errno_reset(pj);
            return Err(ProjError::Transform(msg));
        }
        let xy = out.xy;
        if !xy.x.is_finite() || !xy.y.is_finite() {
            return Err(ProjError::Transform("transform produced non-finite output".into()));
        }
        Ok((xy.x, xy.y))
    }
}

fn densified_bbox(pj: *mut PJ, bbox: Bbox, segments: usize) -> Result<Bbox, ProjError> {
    debug_assert!(segments >= 1);
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;

    let n = segments;
    let dx = (bbox.max_x - bbox.min_x) / n as f64;
    let dy = (bbox.max_y - bbox.min_y) / n as f64;

    let mut visit = |x: f64, y: f64| -> Result<(), ProjError> {
        let (tx, ty) = transform_one(pj, x, y)?;
        if tx < min_x {
            min_x = tx;
        }
        if ty < min_y {
            min_y = ty;
        }
        if tx > max_x {
            max_x = tx;
        }
        if ty > max_y {
            max_y = ty;
        }
        Ok(())
    };

    // bottom + top edges (full width, including corners)
    for i in 0..=n {
        let x = bbox.min_x + dx * i as f64;
        visit(x, bbox.min_y)?;
        visit(x, bbox.max_y)?;
    }
    // left + right edges (interior y only, corners already covered)
    for j in 1..n {
        let y = bbox.min_y + dy * j as f64;
        visit(bbox.min_x, y)?;
        visit(bbox.max_x, y)?;
    }

    Ok(Bbox::new(min_x, min_y, max_x, max_y))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn transformer_construction_succeeds() {
        let from = CrsCode::new("EPSG:25832");
        let to = CrsCode::new("EPSG:4326");
        Transformer::new(&from, &to).unwrap();
    }

    #[test]
    fn transform_point_3857_to_4326_known_value() {
        let t = Transformer::new(&CrsCode::new("EPSG:3857"), &CrsCode::new("EPSG:4326")).unwrap();
        let (lon, lat) = t.transform_point(0.0, 0.0).unwrap();
        assert!(lon.abs() < 1e-9, "lon = {lon}");
        assert!(lat.abs() < 1e-9, "lat = {lat}");
    }

    #[test]
    fn transform_point_25832_to_4326_known_value() {
        // utm 32n (725386, 6177286) -> wgs84 near copenhagen, ~ (12.586, 55.676).
        // tolerance is loose because the input easting/northing is rounded.
        let t = Transformer::new(&CrsCode::new("EPSG:25832"), &CrsCode::new("EPSG:4326")).unwrap();
        let (lon, lat) = t.transform_point(725_386.0, 6_177_286.0).unwrap();
        // round-trip back to verify, then check rough lat/lon range is plausible.
        let inv = Transformer::new(&CrsCode::new("EPSG:4326"), &CrsCode::new("EPSG:25832")).unwrap();
        let (e, n) = inv.transform_point(lon, lat).unwrap();
        assert!((e - 725_386.0).abs() < 1e-3, "round-trip easting = {e}");
        assert!((n - 6_177_286.0).abs() < 1e-3, "round-trip northing = {n}");
        // sanity: somewhere over denmark
        assert!((10.0..=15.0).contains(&lon), "lon = {lon}");
        assert!((54.0..=58.0).contains(&lat), "lat = {lat}");
    }

    #[test]
    fn transform_bbox_densified_25832_to_4326_aabb_widens() {
        // wide utm bbox covering most of denmark; meridian convergence and
        // false-easting curvature mean densified edges bulge outward.
        let from = CrsCode::new("EPSG:25832");
        let to = CrsCode::new("EPSG:4326");
        let bbox = Bbox::new(440_000.0, 6_050_000.0, 900_000.0, 6_400_000.0);

        let dense = Transformer::with_options(&from, &to, TransformerOptions { densify_segments: 32 })
            .unwrap()
            .transform_bbox(bbox)
            .unwrap();

        // four-corner-only AABB
        let t = Transformer::with_options(&from, &to, TransformerOptions { densify_segments: 1 }).unwrap();
        let corners_only = t.transform_bbox(bbox).unwrap();

        // densified must contain the corners-only AABB...
        assert!(dense.min_x <= corners_only.min_x, "{dense:?} vs {corners_only:?}");
        assert!(dense.min_y <= corners_only.min_y, "{dense:?} vs {corners_only:?}");
        assert!(dense.max_x >= corners_only.max_x, "{dense:?} vs {corners_only:?}");
        assert!(dense.max_y >= corners_only.max_y, "{dense:?} vs {corners_only:?}");
        // ...and bulge strictly on at least one edge (otherwise densification
        // is a no-op and we haven't proven the codepath matters).
        let bulges = dense.min_x < corners_only.min_x
            || dense.min_y < corners_only.min_y
            || dense.max_x > corners_only.max_x
            || dense.max_y > corners_only.max_y;
        assert!(bulges, "densification produced no bulge: {dense:?} vs {corners_only:?}");
    }

    #[test]
    fn transform_bbox_densified_4326_to_3857_finite() {
        let t = Transformer::new(&CrsCode::new("EPSG:4326"), &CrsCode::new("EPSG:3857")).unwrap();
        let out = t.transform_bbox(Bbox::new(-10.0, 40.0, 30.0, 60.0)).unwrap();
        for v in [out.min_x, out.min_y, out.max_x, out.max_y] {
            assert!(v.is_finite(), "non-finite component: {v}");
        }
        assert!(out.min_x < out.max_x);
        assert!(out.min_y < out.max_y);
    }

    #[test]
    fn unknown_crs_returns_unknown_crs_error() {
        let err = Transformer::new(&CrsCode::new("EPSG:9999999"), &CrsCode::new("EPSG:4326")).unwrap_err();
        assert!(matches!(err, ProjError::UnknownCrs(_)), "got {err:?}");
    }
}
