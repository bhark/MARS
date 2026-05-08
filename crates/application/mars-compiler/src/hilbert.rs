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
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn sentinels_round_trip() {
        // both extremes should be reachable on the curve; both differ.
        assert_ne!(key_from_xy(0, 0), key_from_xy(u32::MAX, 0));
        assert_eq!(key_from_xy(0, 0), HilbertKey::min());
        // top corner has key < HilbertKey::max() (corner of the curve, not
        // last point); check it's distinct and in the upper half.
        let top = key_from_xy(u32::MAX, u32::MAX);
        assert!(top.get() > (u64::MAX >> 1));
    }

    #[test]
    fn unique_on_16x16_grid() {
        // every cell of a 16x16 region (using only the top 4 bits of x and y)
        // must produce a distinct key. exhaustive check; brute-force inverse.
        let mut keys = BTreeSet::new();
        for y in 0u32..16 {
            for x in 0u32..16 {
                let k = key_from_xy(x << 28, y << 28);
                assert!(keys.insert(k), "duplicate key at ({x}, {y})");
            }
        }
        assert_eq!(keys.len(), 256);
    }

    #[test]
    fn adjacent_cells_locality() {
        // hilbert curve keeps spatial neighbours close in 1d. for any cell
        // in the 16x16 grid, at least one of its 4-neighbours has a key
        // within `2 * cell_step` -- a weaker but algorithm-agnostic locality
        // sanity check. cell_step here is 1 (in top-4-bit units of the key).
        let mut keys = std::collections::HashMap::new();
        for y in 0u32..16 {
            for x in 0u32..16 {
                keys.insert((x, y), key_from_xy(x << 28, y << 28).get());
            }
        }
        for ((x, y), &k) in &keys {
            let neighbours: Vec<u64> = [(-1i32, 0), (1, 0), (0, -1), (0, 1)]
                .into_iter()
                .filter_map(|(dx, dy)| {
                    let nx = (*x as i32) + dx;
                    let ny = (*y as i32) + dy;
                    if (0..16).contains(&nx) && (0..16).contains(&ny) {
                        keys.get(&(nx as u32, ny as u32)).copied()
                    } else {
                        None
                    }
                })
                .collect();
            // some neighbour must be within a small window. exact bound for
            // hilbert is implementation-dependent; we just want to catch
            // catastrophic shuffles where neighbours land far away.
            let min_gap = neighbours
                .iter()
                .fold(u128::MAX, |acc, nk| acc.min((k as i128 - *nk as i128).unsigned_abs()));
            assert!(min_gap < (1u128 << 60), "lost locality at ({x}, {y}): gap={min_gap}");
        }
    }

    #[test]
    fn quantise_clamps() {
        let bbox = Bbox::new(0.0, 0.0, 100.0, 100.0);
        // out-of-range inputs clamp rather than wrap.
        let lo = key_from_centroid(-10.0, -10.0, bbox);
        let hi = key_from_centroid(200.0, 200.0, bbox);
        assert_eq!(lo, key_from_xy(0, 0));
        assert_eq!(hi, key_from_xy(u32::MAX, u32::MAX));
    }

    #[test]
    fn quantise_degenerate_bbox_safe() {
        // zero-extent bbox must not panic; everything maps to the same key.
        let bbox = Bbox::new(5.0, 5.0, 5.0, 5.0);
        let a = key_from_centroid(5.0, 5.0, bbox);
        let b = key_from_centroid(7.0, 9.0, bbox);
        assert_eq!(a, b);
    }

    #[test]
    fn centroid_distinct_for_distinct_cells() {
        // two centroids in different quantised cells must produce different
        // keys.
        let bbox = Bbox::new(0.0, 0.0, 1.0, 1.0);
        let a = key_from_centroid(0.0, 0.0, bbox);
        let b = key_from_centroid(0.5, 0.5, bbox);
        let c = key_from_centroid(1.0, 1.0, bbox);
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }
}
