//! Safe wrapper around the `proj` C library (or the `proj` Rust crate that
//! itself wraps libproj). The boundary lives here so:
//!
//! - the FFI surface is centralised and reviewed in one place;
//! - the safe surface above it can be tested with mocks;
//! - swapping the underlying impl (e.g. to `proj4rs` pure-Rust) is a one-crate
//!   change.
//!
//! Per-thread `PJ_CONTEXT` plus a per-thread `(from,to) -> Transformer` cache
//! to amortise PROJ context+normalize cost across requests on the same worker.

// no `forbid(unsafe_code)`: this crate exists to encapsulate FFI. It must
// remain the *only* place in the workspace that does so.
#![allow(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::rc::Rc;

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

    // per-thread (from,to) -> Transformer cache. Rc (not Arc) keeps the
    // single-thread invariant at the type level; Transformer is !Send because
    // its PJ binds to PROJ_CTX above. CrsCode is Arc<str> with content-based
    // Hash/Eq, so cache hits are zero-alloc and inserts only bump refcounts.
    static TRANSFORMER_CACHE: RefCell<HashMap<(CrsCode, CrsCode), Rc<Transformer>>> = RefCell::new(HashMap::new());
}

/// Look up a cached `(from, to)` transformer for the calling thread,
/// constructing one on the first miss. Returned `Rc` is `!Send`, keeping the
/// PJ-context invariant enforced at the type level.
pub fn cached_transformer(from: &CrsCode, to: &CrsCode) -> Result<Rc<Transformer>, ProjError> {
    TRANSFORMER_CACHE.with(|c| {
        let key = (from.clone(), to.clone());
        if let Some(existing) = c.borrow().get(&key) {
            return Ok(existing.clone());
        }
        let built = Rc::new(Transformer::new(from, to)?);
        c.borrow_mut().insert(key, built.clone());
        Ok(built)
    })
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

/// Wire axis order of a CRS. Used by interfaces (WMS 1.3.0) to decide whether
/// a `BBOX=` slice is `minx,miny,maxx,maxy` (east/north) or
/// `miny,minx,maxy,maxx` (north/east).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AxisOrder {
    /// First axis east (x), second axis north (y). The "natural" order most
    /// CRSes use; matches the WMS 1.1.1 wire format.
    EastNorth,
    /// First axis north (lat), second axis east (lon). EPSG geographic 2D
    /// CRSes (4326, 4258, ...) advertise this and WMS 1.3.0 honours it.
    NorthEast,
}

/// Resolve the wire axis order for `code` via PROJ introspection.
///
/// Reads the first axis direction (`north` / `east` / etc.) off the CRS's
/// coordinate system and maps it to [`AxisOrder`]. Backed by the same
/// per-thread `PJ_CONTEXT` as the rest of this crate; no PROJ state escapes.
///
/// `CRS:84` (OGC) is defined as WGS 84 with longitude/latitude axis order,
/// so it short-circuits to [`AxisOrder::EastNorth`] without consulting PROJ.
pub fn axis_order(code: &CrsCode) -> Result<AxisOrder, ProjError> {
    // ogc crs:84 is wgs84 with explicit lon/lat axis order. proj treats it as
    // a synonym of epsg:4326 in some database revisions and returns north/east,
    // which would lie about the wire order. pin it here.
    if code.as_str().eq_ignore_ascii_case("CRS:84") {
        return Ok(AxisOrder::EastNorth);
    }

    let definition =
        CString::new(code.as_str()).map_err(|e| ProjError::UnknownCrs(format!("invalid CRS string: {e}")))?;

    PROJ_CTX.with(|ctx| {
        let ctx_ptr = ctx.as_ptr();
        if ctx_ptr.is_null() {
            return Err(ProjError::Transform("failed to create PROJ context".into()));
        }
        // SAFETY: ctx_ptr is a live thread-local context; definition is a valid
        // NUL-terminated C string. proj_create returns null on failure. The
        // intermediate PJ handles (crs, cs) are destroyed before return.
        unsafe {
            let crs = proj_sys::proj_create(ctx_ptr, definition.as_ptr());
            if crs.is_null() {
                return Err(ProjError::UnknownCrs(format!("{code}: {}", proj_ctx_error(ctx_ptr))));
            }
            let cs = proj_sys::proj_crs_get_coordinate_system(ctx_ptr, crs);
            proj_sys::proj_destroy(crs);
            if cs.is_null() {
                return Err(ProjError::Transform(format!(
                    "no coordinate system for {code}: {}",
                    proj_ctx_error(ctx_ptr)
                )));
            }
            let mut direction: *const std::os::raw::c_char = std::ptr::null();
            let ok = proj_sys::proj_cs_get_axis_info(
                ctx_ptr,
                cs,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut direction,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            proj_sys::proj_destroy(cs);
            if ok == 0 || direction.is_null() {
                return Err(ProjError::Transform(format!(
                    "axis info unavailable for {code}: {}",
                    proj_ctx_error(ctx_ptr)
                )));
            }
            let dir = CStr::from_ptr(direction).to_string_lossy().to_ascii_lowercase();
            match dir.as_str() {
                "north" | "south" => Ok(AxisOrder::NorthEast),
                "east" | "west" => Ok(AxisOrder::EastNorth),
                other => Err(ProjError::Transform(format!(
                    "unexpected first-axis direction `{other}` for {code}"
                ))),
            }
        }
    })
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
                return Err(ProjError::UnknownCrs(format!("{code}: {}", proj_ctx_error(ctx_ptr))));
            }
            let ty = proj_sys::proj_get_type(pj);
            proj_sys::proj_destroy(pj);
            Ok(ty == proj_sys::PJ_TYPE_PJ_TYPE_PROJECTED_CRS)
        }
    })
}

/// Read the last error registered on `ctx_ptr` as a human-readable string.
/// PROJ overwrites errno per-call on the same thread, so this must be called
/// immediately after the failing API.
///
/// # Safety
///
/// `ctx_ptr` must be a valid `PJ_CONTEXT*` (the thread-local context here is).
unsafe fn proj_ctx_error(ctx_ptr: *mut proj_sys::PJ_CONTEXT) -> String {
    unsafe {
        let errno = proj_sys::proj_context_errno(ctx_ptr);
        if errno == 0 {
            return "unknown".to_string();
        }
        let msg_ptr = proj_sys::proj_context_errno_string(ctx_ptr, errno);
        if msg_ptr.is_null() {
            return format!("errno {errno}");
        }
        match std::ffi::CStr::from_ptr(msg_ptr).to_str() {
            Ok(s) => s.to_string(),
            Err(_) => format!("errno {errno}"),
        }
    }
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
/// used with its construction context, so the type is statically `!Send` and
/// `!Sync` (compile-time-guarded below); a `RefCell` is enough to gate the
/// `&mut PJ` access required by the C API without paying for a `Mutex` whose
/// guard would never contend.
#[derive(Debug)]
pub struct Transformer {
    inner: RefCell<TransformerInner>,
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
            // is not used after this point; the !Send / !Sync invariant on
            // Transformer guarantees no concurrent use.
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
                    return Err(ProjError::UnknownCrs(format!(
                        "{from} -> {to}: {}",
                        proj_ctx_error(ctx_ptr)
                    )));
                }
                let normalized = proj_sys::proj_normalize_for_visualization(ctx_ptr, raw);
                proj_sys::proj_destroy(raw);
                if normalized.is_null() {
                    return Err(ProjError::Transform(format!(
                        "proj_normalize_for_visualization failed for {from} -> {to}: {}",
                        proj_ctx_error(ctx_ptr)
                    )));
                }
                Ok(Self {
                    inner: RefCell::new(TransformerInner { pj: normalized }),
                    densify_segments: opts.densify_segments,
                })
            }
        })
    }

    /// Forward-transform a single point.
    pub fn transform_point(&self, x: f64, y: f64) -> Result<(f64, f64), ProjError> {
        let guard = self.inner.borrow();
        transform_one(guard.pj, x, y)
    }

    /// Forward-transform a bounding box, densifying each edge before computing
    /// the axis-aligned bounding box of the transformed samples.
    pub fn transform_bbox(&self, bbox: Bbox) -> Result<Bbox, ProjError> {
        let guard = self.inner.borrow();
        densified_bbox(guard.pj, bbox, self.densify_segments)
    }

    /// Forward-transform an in-place array of `[x, y]` pairs in a single FFI
    /// call. For a 1000-vertex ring this collapses 1000 FFI hops into one,
    /// which is where most of the per-feature reproject cost lives.
    pub fn transform_points(&self, points: &mut [[f64; 2]]) -> Result<(), ProjError> {
        if points.is_empty() {
            return Ok(());
        }
        let guard = self.inner.borrow();
        let n = points.len();
        // [f64; 2] has well-defined layout: sizeof == 2*sizeof::<f64>(), no padding.
        let stride = std::mem::size_of::<[f64; 2]>();
        // SAFETY: `points` is &mut so we have exclusive access for the call.
        // x_ptr / y_ptr cover the same allocation; PROJ reads/writes each lane
        // with the given stride, which the layout guarantee makes well-defined.
        unsafe {
            let base = points.as_mut_ptr().cast::<f64>();
            let x_ptr = base;
            let y_ptr = base.add(1);
            let count = proj_sys::proj_trans_generic(
                guard.pj,
                proj_sys::PJ_DIRECTION_PJ_FWD,
                x_ptr,
                stride,
                n,
                y_ptr,
                stride,
                n,
                std::ptr::null_mut(),
                0,
                0,
                std::ptr::null_mut(),
                0,
                0,
            );
            if count != n {
                let err = proj_sys::proj_errno(guard.pj);
                let msg = if err != 0 {
                    let p = proj_sys::proj_errno_string(err);
                    if p.is_null() {
                        format!("PROJ errno {err}")
                    } else {
                        CStr::from_ptr(p).to_string_lossy().into_owned()
                    }
                } else {
                    format!("proj_trans_generic transformed {count}/{n} points")
                };
                proj_sys::proj_errno_reset(guard.pj);
                return Err(ProjError::Transform(msg));
            }
        }
        // PROJ writes HUGE_VAL on per-point failure without setting errno;
        // a final scan surfaces those rather than poisoning downstream math.
        for [x, y] in points.iter() {
            if !x.is_finite() || !y.is_finite() {
                return Err(ProjError::Transform("transform produced non-finite output".into()));
            }
        }
        Ok(())
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
mod tests;
