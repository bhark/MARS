#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

fn fg(bbox: [f32; 4]) -> FeatureGeom {
    FeatureGeom {
        user_id: 1,
        bbox,
        geom: GeomKind::Point((0.0, 0.0)),
    }
}

#[test]
fn min_size_zero_keeps_everything() {
    let f = fg([0.0, 0.0, 0.0, 0.0]);
    assert!(passes_min_size(&f, 0.0));
}

#[test]
fn min_size_drops_below_threshold() {
    // diagonal = sqrt(3^2 + 4^2) = 5
    let f = fg([0.0, 0.0, 3.0, 4.0]);
    assert!(passes_min_size(&f, 5.0));
    assert!(!passes_min_size(&f, 5.1));
}

#[test]
fn label_priority_threshold() {
    assert!(passes_label_priority(10, 0));
    assert!(passes_label_priority(10, 10));
    assert!(!passes_label_priority(9, 10));
}

#[test]
fn simplify_zero_tolerance_no_op() {
    let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 0.1), (2.0, 0.0)]);
    let s = simplify_naive(&g, 0.0);
    assert_eq!(s, g);
}

#[test]
fn simplify_collinear_line_drops_middle() {
    let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)]);
    let s = simplify_naive(&g, 0.5);
    match s {
        GeomKind::LineString(out) => assert_eq!(out, vec![(0.0, 0.0), (3.0, 0.0)]),
        _ => panic!("expected LineString"),
    }
}

#[test]
fn simplify_keeps_significant_vertex() {
    // big midpoint deviation must be kept.
    let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 5.0), (2.0, 0.0)]);
    let s = simplify_naive(&g, 1.0);
    match s {
        GeomKind::LineString(out) => assert_eq!(out, vec![(0.0, 0.0), (1.0, 5.0), (2.0, 0.0)]),
        _ => panic!("expected LineString"),
    }
}

#[test]
fn simplify_polygon_per_ring() {
    // square with a redundant midpoint on the top edge.
    let ring = vec![
        (0.0, 0.0),
        (10.0, 0.0),
        (10.0, 10.0),
        (5.0, 10.0),
        (0.0, 10.0),
        (0.0, 0.0),
    ];
    let g = GeomKind::Polygon(vec![ring]);
    let s = simplify_naive(&g, 0.5);
    match s {
        GeomKind::Polygon(rings) => {
            // (5, 10) is collinear with (10, 10) and (0, 10) so it must drop.
            assert_eq!(rings[0].len(), 5);
            assert_eq!(rings[0].first(), rings[0].last());
        }
        _ => panic!("expected Polygon"),
    }
}

#[test]
fn simplify_preserves_points_and_multipoints() {
    let g = GeomKind::Point((1.0, 2.0));
    assert_eq!(simplify_naive(&g, 5.0), g);
    let g = GeomKind::MultiPoint(vec![(1.0, 2.0), (3.0, 4.0)]);
    assert_eq!(simplify_naive(&g, 5.0), g);
}

#[test]
fn simplify_keeps_degenerate_ring() {
    // 4-point ring (triangle + closure); simplification at huge tol would
    // otherwise drop interior vertices, but we preserve to avoid bad output.
    let ring = vec![(0.0, 0.0), (1.0, 0.0), (0.5, 0.5), (0.0, 0.0)];
    let g = GeomKind::Polygon(vec![ring.clone()]);
    let s = simplify_naive(&g, 100.0);
    match s {
        GeomKind::Polygon(rings) => assert_eq!(rings[0], ring),
        _ => panic!("expected Polygon"),
    }
}

#[test]
fn dispatch_naive_matches_naive_directly() {
    let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)]);
    let dispatched = simplify(&g, 0.5, SimplifierKind::Naive);
    let direct = simplify_naive(&g, 0.5);
    assert_eq!(dispatched, direct);
}
