//! polygon-label anchor algorithms.
//!
//! - [`centroid`] - true area-weighted centroid by shoelace; holes subtracted.
//! - [`pole_of_inaccessibility`] - Mapbox-style polylabel quadtree search;
//!   always lands inside the polygon, robust on L-shapes / donuts / concave
//!   geometry.
//!
//! both take a polygon as `&[Ring]` where the first ring is the outer ring
//! and subsequent rings are holes. for `MultiPolygon` the caller picks the
//! polygon with the largest absolute outer-ring area and passes that polygon
//! whole (outer + holes) - see [`pick_largest_polygon`].

use mars_artifact::{Coord, GeomKind};
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// signed area of a ring via shoelace. positive = CCW, negative = CW.
fn signed_area(ring: &[Coord]) -> f64 {
    if ring.len() < 3 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        acc += ring[i].0 * ring[j].1 - ring[j].0 * ring[i].1;
    }
    acc * 0.5
}

/// area-weighted ring centroid via the standard shoelace centroid formula.
/// returns the unweighted bbox-midpoint for degenerate (zero-area) rings.
fn ring_centroid(ring: &[Coord]) -> (f64, f64, f64) {
    if ring.len() < 3 {
        let (sx, sy, n) = ring
            .iter()
            .fold((0.0, 0.0, 0.0_f64), |(x, y, n), &(rx, ry)| (x + rx, y + ry, n + 1.0));
        if n > 0.0 {
            return (sx / n, sy / n, 0.0);
        }
        return (0.0, 0.0, 0.0);
    }
    let mut cx = 0.0;
    let mut cy = 0.0;
    let mut area = 0.0;
    for i in 0..ring.len() {
        let j = (i + 1) % ring.len();
        let cross = ring[i].0 * ring[j].1 - ring[j].0 * ring[i].1;
        cx += (ring[i].0 + ring[j].0) * cross;
        cy += (ring[i].1 + ring[j].1) * cross;
        area += cross;
    }
    area *= 0.5;
    if area.abs() < f64::EPSILON {
        // collinear ring: fall back to vertex mean.
        let n = ring.len() as f64;
        let (sx, sy) = ring.iter().fold((0.0, 0.0), |(x, y), &(rx, ry)| (x + rx, y + ry));
        return (sx / n, sy / n, 0.0);
    }
    (cx / (6.0 * area), cy / (6.0 * area), area)
}

/// area-weighted centroid of a polygon (first ring outer, rest are holes).
/// holes subtract from the outer ring's contribution.
#[must_use]
pub fn centroid(polygon: &[Vec<Coord>]) -> (f64, f64) {
    let Some(outer) = polygon.first() else {
        return (0.0, 0.0);
    };
    let (mut cx, mut cy, mut area) = ring_centroid(outer);
    cx *= area;
    cy *= area;
    for hole in polygon.iter().skip(1) {
        let (hcx, hcy, harea) = ring_centroid(hole);
        // holes are subtractive regardless of winding.
        let ha = harea.abs();
        cx -= hcx * ha;
        cy -= hcy * ha;
        area -= ha;
    }
    if area.abs() < f64::EPSILON {
        // degenerate polygon: fall back to outer ring's bbox midpoint.
        return ring_bbox_centroid(outer);
    }
    (cx / area, cy / area)
}

fn ring_bbox(ring: &[Coord]) -> (f64, f64, f64, f64) {
    let mut minx = f64::INFINITY;
    let mut miny = f64::INFINITY;
    let mut maxx = f64::NEG_INFINITY;
    let mut maxy = f64::NEG_INFINITY;
    for &(x, y) in ring {
        if x < minx {
            minx = x;
        }
        if y < miny {
            miny = y;
        }
        if x > maxx {
            maxx = x;
        }
        if y > maxy {
            maxy = y;
        }
    }
    (minx, miny, maxx, maxy)
}

fn ring_bbox_centroid(ring: &[Coord]) -> (f64, f64) {
    let (minx, miny, maxx, maxy) = ring_bbox(ring);
    ((minx + maxx) * 0.5, (miny + maxy) * 0.5)
}

/// squared distance from a point to a segment. uses the foot-of-perpendicular
/// projection clamped to [0, 1].
fn dist2_point_to_seg(p: Coord, a: Coord, b: Coord) -> f64 {
    let (px, py) = p;
    let (ax, ay) = a;
    let (bx, by) = b;
    let dx = bx - ax;
    let dy = by - ay;
    let len2 = dx * dx + dy * dy;
    if len2 == 0.0 {
        let dxp = px - ax;
        let dyp = py - ay;
        return dxp * dxp + dyp * dyp;
    }
    let t = (((px - ax) * dx) + ((py - ay) * dy)) / len2;
    let t = t.clamp(0.0, 1.0);
    let qx = ax + t * dx;
    let qy = ay + t * dy;
    let dxp = px - qx;
    let dyp = py - qy;
    dxp * dxp + dyp * dyp
}

/// even-odd point-in-polygon over a single ring.
fn point_in_ring(p: Coord, ring: &[Coord]) -> bool {
    if ring.len() < 3 {
        return false;
    }
    let (px, py) = p;
    let mut inside = false;
    let mut j = ring.len() - 1;
    for i in 0..ring.len() {
        let (xi, yi) = ring[i];
        let (xj, yj) = ring[j];
        let intersect = ((yi > py) != (yj > py))
            && (px < (xj - xi) * (py - yi) / (yj - yi).max(f64::MIN_POSITIVE).copysign(yj - yi) + xi);
        if intersect {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// signed distance from a point to the polygon boundary. positive when inside,
/// negative when outside. boundary distance is the minimum unsigned distance
/// to any segment across all rings (outer + holes).
fn signed_dist_to_polygon(p: Coord, polygon: &[Vec<Coord>]) -> f64 {
    let Some(outer) = polygon.first() else {
        return f64::NEG_INFINITY;
    };
    let mut inside = point_in_ring(p, outer);
    // hole containment flips inside-ness.
    for hole in polygon.iter().skip(1) {
        if point_in_ring(p, hole) {
            inside = !inside;
            break;
        }
    }
    let mut min_d2 = f64::INFINITY;
    for ring in polygon {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..ring.len() {
            let a = ring[i];
            let b = ring[(i + 1) % ring.len()];
            let d2 = dist2_point_to_seg(p, a, b);
            if d2 < min_d2 {
                min_d2 = d2;
            }
        }
    }
    let d = min_d2.sqrt();
    if inside { d } else { -d }
}

/// priority-queue cell for polylabel. `d` is signed distance from cell centre
/// to polygon boundary; `max_d` is the upper bound on `d` reachable inside
/// this cell (d + h / sqrt(2)).
#[derive(Debug)]
struct Cell {
    x: f64,
    y: f64,
    half: f64,
    d: f64,
    max_d: f64,
}

impl Cell {
    fn new(x: f64, y: f64, half: f64, polygon: &[Vec<Coord>]) -> Self {
        let d = signed_dist_to_polygon((x, y), polygon);
        let max_d = d + half * std::f64::consts::SQRT_2;
        Self { x, y, half, d, max_d }
    }
}

impl PartialEq for Cell {
    fn eq(&self, other: &Self) -> bool {
        self.max_d == other.max_d
    }
}
impl Eq for Cell {}
impl PartialOrd for Cell {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Cell {
    fn cmp(&self, other: &Self) -> Ordering {
        // BinaryHeap is a max-heap; ordering by max_d gives best-first.
        self.max_d
            .partial_cmp(&other.max_d)
            .unwrap_or(Ordering::Equal)
    }
}

/// Hard cap on the seed-cell tiling. thin/elongated bboxes (very small
/// width.min(height)) would otherwise generate `(w*h)/cell_size^2` cells,
/// quadratic in the aspect ratio. when the natural tiling exceeds this we
/// upscale cell_size until the count fits.
const MAX_SEED_CELLS: usize = 1024;

/// Hard cap on quadtree growth. each pop expands into 4 children; with
/// pathological precision tuning the heap could grow without bound. when
/// we hit this many pushed cells we stop refining and return the best
/// candidate so far - it is still strictly better than the centroid seed.
const MAX_HEAP_CELLS: usize = 100_000;

/// Mapbox-style pole of inaccessibility. iterative quadtree refinement keyed
/// by an upper-bound interior distance. precision is the tolerance in CRS
/// units; smaller = more iterations.
#[must_use]
pub fn pole_of_inaccessibility(polygon: &[Vec<Coord>], precision: f64) -> (f64, f64) {
    let Some(outer) = polygon.first() else {
        return (0.0, 0.0);
    };
    if outer.is_empty() {
        return (0.0, 0.0);
    }
    let (minx, miny, maxx, maxy) = ring_bbox(outer);
    let width = maxx - minx;
    let height = maxy - miny;
    if width <= 0.0 || height <= 0.0 {
        return ((minx + maxx) * 0.5, (miny + maxy) * 0.5);
    }
    // upscale cell_size until the seed-cell count fits the cap. avoids
    // quadratic blow-up on very elongated polygons (10000x1 bbox would
    // otherwise seed ~10k cells).
    let mut cell_size = width.min(height);
    let nx = (width / cell_size).ceil() as usize;
    let ny = (height / cell_size).ceil() as usize;
    if nx.saturating_mul(ny) > MAX_SEED_CELLS {
        let scale = ((nx * ny) as f64 / MAX_SEED_CELLS as f64).sqrt();
        cell_size *= scale;
    }
    let mut h = cell_size / 2.0;

    let mut heap: BinaryHeap<Cell> = BinaryHeap::new();
    let mut x = minx;
    while x < maxx {
        let mut y = miny;
        while y < maxy {
            heap.push(Cell::new(x + h, y + h, h, polygon));
            y += cell_size;
        }
        x += cell_size;
    }

    let (ccx, ccy) = centroid(polygon);
    let mut best = Cell::new(ccx, ccy, 0.0, polygon);
    let centre = Cell::new((minx + maxx) * 0.5, (miny + maxy) * 0.5, 0.0, polygon);
    if centre.d > best.d {
        best = centre;
    }

    let mut pushed: usize = heap.len();
    while let Some(cell) = heap.pop() {
        if cell.d > best.d {
            best = Cell {
                x: cell.x,
                y: cell.y,
                half: cell.half,
                d: cell.d,
                max_d: cell.max_d,
            };
        }
        if cell.max_d - best.d <= precision {
            continue;
        }
        if pushed >= MAX_HEAP_CELLS {
            // pathological precision/polygon combination; stop refining.
            // returns the best candidate found so far.
            break;
        }
        h = cell.half / 2.0;
        heap.push(Cell::new(cell.x - h, cell.y - h, h, polygon));
        heap.push(Cell::new(cell.x + h, cell.y - h, h, polygon));
        heap.push(Cell::new(cell.x - h, cell.y + h, h, polygon));
        heap.push(Cell::new(cell.x + h, cell.y + h, h, polygon));
        pushed += 4;
    }
    (best.x, best.y)
}

/// pick the polygon whose outer ring has the largest absolute area, then
/// return the whole polygon (outer + holes). for `Polygon` returns a borrow
/// over the input; for `MultiPolygon` returns the slice for the chosen
/// polygon. yields `None` for non-polygonal geometry.
#[must_use]
pub fn pick_largest_polygon(g: &GeomKind) -> Option<&[Vec<Coord>]> {
    match g {
        GeomKind::Polygon(rings) => Some(rings.as_slice()),
        GeomKind::MultiPolygon(polys) => polys
            .iter()
            .max_by(|a, b| {
                let aa = a.first().map(|r| signed_area(r).abs()).unwrap_or(0.0);
                let ba = b.first().map(|r| signed_area(r).abs()).unwrap_or(0.0);
                aa.partial_cmp(&ba).unwrap_or(Ordering::Equal)
            })
            .map(|v| v.as_slice()),
        _ => None,
    }
}

/// default precision: scale the polygon's bbox to ~1/200 of its shortest side.
/// good cost/accuracy balance for tile-scale labels.
#[must_use]
pub fn default_precision(polygon: &[Vec<Coord>]) -> f64 {
    let Some(outer) = polygon.first() else {
        return 1.0;
    };
    let (minx, miny, maxx, maxy) = ring_bbox(outer);
    let w = (maxx - minx).abs();
    let h = (maxy - miny).abs();
    let m = w.min(h);
    if m <= 0.0 { 1.0 } else { m / 200.0 }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn rect(minx: f64, miny: f64, maxx: f64, maxy: f64) -> Vec<Coord> {
        vec![
            (minx, miny),
            (maxx, miny),
            (maxx, maxy),
            (minx, maxy),
            (minx, miny),
        ]
    }

    #[test]
    fn centroid_of_square_is_centre() {
        let poly = vec![rect(0.0, 0.0, 10.0, 10.0)];
        let (cx, cy) = centroid(&poly);
        assert!((cx - 5.0).abs() < 1e-9, "cx={cx}");
        assert!((cy - 5.0).abs() < 1e-9, "cy={cy}");
    }

    #[test]
    fn centroid_of_donut_balances_around_outer_centre() {
        // outer 0..10, hole centred on (5,5) -> centroid still at (5, 5).
        let poly = vec![rect(0.0, 0.0, 10.0, 10.0), rect(4.0, 4.0, 6.0, 6.0)];
        let (cx, cy) = centroid(&poly);
        assert!((cx - 5.0).abs() < 1e-9, "cx={cx}");
        assert!((cy - 5.0).abs() < 1e-9, "cy={cy}");
    }

    #[test]
    fn polylabel_of_square_is_centre() {
        let poly = vec![rect(0.0, 0.0, 10.0, 10.0)];
        let prec = default_precision(&poly);
        let (cx, cy) = pole_of_inaccessibility(&poly, prec);
        assert!((cx - 5.0).abs() < 0.1, "cx={cx}");
        assert!((cy - 5.0).abs() < 0.1, "cy={cy}");
    }

    #[test]
    fn polylabel_l_shape_lands_inside_arm() {
        // L: (0,0)-(10,0)-(10,4)-(4,4)-(4,10)-(0,10).
        // bbox-centroid (5,5) is *outside* the L; polylabel must land inside.
        let outer = vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 4.0),
            (4.0, 4.0),
            (4.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ];
        let poly = vec![outer];
        let prec = default_precision(&poly);
        let (cx, cy) = pole_of_inaccessibility(&poly, prec);
        // bbox-centroid would be (5,5), which is in the gap -> NOT inside.
        assert!(point_in_ring((cx, cy), &poly[0]), "polylabel anchor outside L: ({cx},{cy})");
    }

    #[test]
    fn polylabel_donut_avoids_hole() {
        // outer square 0..10, hole 4..6 in the middle. bbox-centroid (5,5) is
        // inside the hole -> bad. polylabel must place outside the hole.
        let poly = vec![rect(0.0, 0.0, 10.0, 10.0), rect(4.0, 4.0, 6.0, 6.0)];
        let prec = default_precision(&poly);
        let (cx, cy) = pole_of_inaccessibility(&poly, prec);
        let in_outer = point_in_ring((cx, cy), &poly[0]);
        let in_hole = point_in_ring((cx, cy), &poly[1]);
        assert!(in_outer, "anchor outside outer: ({cx},{cy})");
        assert!(!in_hole, "anchor landed inside hole: ({cx},{cy})");
    }

    #[test]
    fn polylabel_concave_u_lands_inside() {
        // U-shape: (0,0)-(10,0)-(10,10)-(7,10)-(7,3)-(3,3)-(3,10)-(0,10)-(0,0).
        let outer = vec![
            (0.0, 0.0),
            (10.0, 0.0),
            (10.0, 10.0),
            (7.0, 10.0),
            (7.0, 3.0),
            (3.0, 3.0),
            (3.0, 10.0),
            (0.0, 10.0),
            (0.0, 0.0),
        ];
        let poly = vec![outer];
        let prec = default_precision(&poly);
        let (cx, cy) = pole_of_inaccessibility(&poly, prec);
        assert!(point_in_ring((cx, cy), &poly[0]), "anchor outside U: ({cx},{cy})");
    }

    #[test]
    fn polylabel_thin_polygon_terminates_quickly() {
        // pathological case: 10_000 x 1 bbox. without the seed-cell cap the
        // natural tiling would seed ~10k cells. result must still land inside.
        let poly = vec![rect(0.0, 0.0, 10_000.0, 1.0)];
        let (cx, cy) = pole_of_inaccessibility(&poly, default_precision(&poly));
        assert!(point_in_ring((cx, cy), &poly[0]), "thin-polygon anchor not inside: ({cx},{cy})");
    }

    #[test]
    fn polylabel_extreme_precision_terminates_at_heap_cap() {
        // 100x100 polygon with an absurdly small precision; the heap-size
        // guard must stop refinement before it grows without bound.
        let poly = vec![rect(0.0, 0.0, 100.0, 100.0)];
        let (cx, cy) = pole_of_inaccessibility(&poly, 1e-9);
        // even with the guard tripping early, the answer is still strictly
        // better than (or equal to) the centroid - here, (50, 50).
        assert!((cx - 50.0).abs() < 5.0);
        assert!((cy - 50.0).abs() < 5.0);
    }

    #[test]
    fn pick_largest_polygon_picks_by_outer_area() {
        let small = vec![rect(0.0, 0.0, 1.0, 1.0)];
        let big = vec![rect(0.0, 0.0, 100.0, 100.0)];
        let mp = GeomKind::MultiPolygon(vec![small.clone(), big.clone()]);
        let picked = pick_largest_polygon(&mp).unwrap();
        // big polygon's outer ring is 100x100.
        let outer = &picked[0];
        let (minx, miny, maxx, maxy) = ring_bbox(outer);
        assert!((maxx - minx).abs() > 50.0, "{minx} {maxx}");
        assert!((maxy - miny).abs() > 50.0, "{miny} {maxy}");
    }
}
