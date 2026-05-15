//! derive a synthetic point set from a feature geometry when a style sets
//! `geom_transform`. mirrors mapserver's GEOMTRANSFORM start | end | vertices.
//! the render dispatch routes the derived coords through the multipoint
//! subpath builder, so the existing marker pipeline stamps each point.

use mars_artifact::GeomKind;
use mars_style::GeomTransform;

pub(super) fn derived_points(g: &GeomKind, t: GeomTransform) -> Vec<(f64, f64)> {
    match g {
        GeomKind::Point(c) => vec![*c],
        GeomKind::MultiPoint(coords) => coords.clone(),
        GeomKind::LineString(coords) => sample_ring(coords, t),
        GeomKind::Polygon(rings) => rings.iter().flat_map(|r| sample_ring(r, t)).collect(),
        GeomKind::MultiLineString(parts) => parts.iter().flat_map(|r| sample_ring(r, t)).collect(),
        GeomKind::MultiPolygon(polys) => polys
            .iter()
            .flat_map(|poly| poly.iter().flat_map(|r| sample_ring(r, t)))
            .collect(),
    }
}

fn sample_ring(coords: &[(f64, f64)], t: GeomTransform) -> Vec<(f64, f64)> {
    match t {
        GeomTransform::Start => coords.first().copied().into_iter().collect(),
        GeomTransform::End => coords.last().copied().into_iter().collect(),
        GeomTransform::Vertices => coords.to_vec(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn linestring_start_takes_first_vertex() {
        let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        assert_eq!(derived_points(&g, GeomTransform::Start), vec![(0.0, 0.0)]);
    }

    #[test]
    fn linestring_end_takes_last_vertex() {
        let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        assert_eq!(derived_points(&g, GeomTransform::End), vec![(2.0, 2.0)]);
    }

    #[test]
    fn linestring_vertices_returns_all() {
        let g = GeomKind::LineString(vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)]);
        assert_eq!(
            derived_points(&g, GeomTransform::Vertices),
            vec![(0.0, 0.0), (1.0, 1.0), (2.0, 2.0)],
        );
    }

    #[test]
    fn polygon_start_yields_one_per_ring() {
        let g = GeomKind::Polygon(vec![
            vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 0.0)],
            vec![(2.0, 2.0), (3.0, 2.0), (3.0, 3.0), (2.0, 2.0)],
        ]);
        assert_eq!(derived_points(&g, GeomTransform::Start), vec![(0.0, 0.0), (2.0, 2.0)],);
    }

    #[test]
    fn polygon_end_matches_start_on_closed_rings() {
        let g = GeomKind::Polygon(vec![vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 0.0)]]);
        assert_eq!(derived_points(&g, GeomTransform::End), vec![(0.0, 0.0)]);
    }

    #[test]
    fn multilinestring_vertices_flattens_across_parts() {
        let g = GeomKind::MultiLineString(vec![vec![(0.0, 0.0), (1.0, 1.0)], vec![(5.0, 5.0), (6.0, 6.0)]]);
        assert_eq!(
            derived_points(&g, GeomTransform::Vertices),
            vec![(0.0, 0.0), (1.0, 1.0), (5.0, 5.0), (6.0, 6.0)],
        );
    }

    #[test]
    fn multipolygon_start_yields_one_per_ring_across_polys() {
        let g = GeomKind::MultiPolygon(vec![
            vec![vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 0.0)]],
            vec![
                vec![(10.0, 10.0), (11.0, 10.0), (11.0, 11.0), (10.0, 10.0)],
                vec![(10.2, 10.2), (10.8, 10.2), (10.8, 10.8), (10.2, 10.2)],
            ],
        ]);
        assert_eq!(
            derived_points(&g, GeomTransform::Start),
            vec![(0.0, 0.0), (10.0, 10.0), (10.2, 10.2)],
        );
    }

    #[test]
    fn point_passes_through_for_any_transform() {
        let g = GeomKind::Point((3.0, 4.0));
        for t in [GeomTransform::Start, GeomTransform::End, GeomTransform::Vertices] {
            assert_eq!(derived_points(&g, t), vec![(3.0, 4.0)]);
        }
    }

    #[test]
    fn multipoint_passes_through_for_any_transform() {
        let g = GeomKind::MultiPoint(vec![(1.0, 1.0), (2.0, 2.0)]);
        for t in [GeomTransform::Start, GeomTransform::End, GeomTransform::Vertices] {
            assert_eq!(derived_points(&g, t), vec![(1.0, 1.0), (2.0, 2.0)]);
        }
    }

    #[test]
    fn empty_linestring_yields_no_points() {
        let g = GeomKind::LineString(vec![]);
        assert!(derived_points(&g, GeomTransform::Start).is_empty());
        assert!(derived_points(&g, GeomTransform::End).is_empty());
        assert!(derived_points(&g, GeomTransform::Vertices).is_empty());
    }
}
