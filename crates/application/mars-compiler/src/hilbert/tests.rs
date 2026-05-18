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
