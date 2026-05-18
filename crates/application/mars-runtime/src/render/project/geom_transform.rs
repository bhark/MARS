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
mod tests;
