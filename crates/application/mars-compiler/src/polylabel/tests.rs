#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn rect(minx: f64, miny: f64, maxx: f64, maxy: f64) -> Vec<Coord> {
    vec![(minx, miny), (maxx, miny), (maxx, maxy), (minx, maxy), (minx, miny)]
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
    assert!(
        point_in_ring((cx, cy), &poly[0]),
        "polylabel anchor outside L: ({cx},{cy})"
    );
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
    assert!(
        point_in_ring((cx, cy), &poly[0]),
        "thin-polygon anchor not inside: ({cx},{cy})"
    );
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
