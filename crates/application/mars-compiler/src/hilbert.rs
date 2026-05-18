//! 32-bit hilbert curve. snapshot quantises each feature centroid to the
//! binding's combined bbox, encodes it via [`key_from_xy`], and uses the
//! resulting [`HilbertKey`] to sort rows into spatially coherent pages.
//!
//! curve order is fixed at 32 bits per axis -> 2^64 distinct keys, packed
//! directly into `HilbertKey(u64)`.

use mars_types::{Bbox, HilbertKey};

/// hilbert curve order: 32 bits per axis -> 64-bit key.
const ORDER: u32 = 32;

/// encode an `(x, y)` pair on the u32 grid into a 64-bit hilbert key.
/// classic rotate-and-fold construction; see the wikipedia "Hilbert curve"
/// `xy2d` reference. for `n = 2^32`, the `n - 1 - v` flip in the rotate step
/// becomes `u32::MAX - v` since `n` wraps to 0 in u32.
#[must_use]
pub fn key_from_xy(mut x: u32, mut y: u32) -> HilbertKey {
    let mut d: u64 = 0;
    let mut s: u32 = 1u32 << (ORDER - 1);
    while s > 0 {
        let rx: u32 = u32::from((x & s) > 0);
        let ry: u32 = u32::from((y & s) > 0);
        let s64 = u64::from(s);
        d = d.wrapping_add(s64.wrapping_mul(s64).wrapping_mul(u64::from((3 * rx) ^ ry)));
        rotate(&mut x, &mut y, rx, ry);
        s >>= 1;
    }
    HilbertKey::new(d)
}

#[inline]
fn rotate(x: &mut u32, y: &mut u32, rx: u32, ry: u32) {
    if ry == 0 {
        if rx == 1 {
            *x = u32::MAX - *x;
            *y = u32::MAX - *y;
        }
        core::mem::swap(x, y);
    }
}

/// quantise a real-valued centroid to the u32 grid spanning `bbox`, then
/// hilbert-encode. degenerate bboxes (zero width or height) collapse to
/// origin on that axis -- callers should not be feeding those in, but the
/// path stays panic-free in case a binding has a single-feature stretch.
#[must_use]
pub fn key_from_centroid(cx: f64, cy: f64, bbox: Bbox) -> HilbertKey {
    let nx = quantise(cx, bbox.min_x, bbox.max_x);
    let ny = quantise(cy, bbox.min_y, bbox.max_y);
    key_from_xy(nx, ny)
}

#[inline]
fn quantise(v: f64, lo: f64, hi: f64) -> u32 {
    // guard rejects NaN, infinities, and zero-extent bboxes alike.
    if !(hi.is_finite() && lo.is_finite() && hi > lo) {
        return 0;
    }
    let t = ((v - lo) / (hi - lo)).clamp(0.0, 1.0);
    // map [0, 1] -> [0, u32::MAX]; bias to avoid losing top of range to fp.
    let scaled = t * f64::from(u32::MAX);
    if scaled >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        scaled as u32
    }
}

#[cfg(test)]
mod tests;
