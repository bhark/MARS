use super::*;

#[test]
fn default_placement_picks_per_geom_kind() {
    assert!(matches!(default_placement(LayerGeomKind::Point), Placement::Point));
    assert!(matches!(
        default_placement(LayerGeomKind::Line),
        Placement::Line { repeat_m: 250.0, .. }
    ));
    assert!(matches!(
        default_placement(LayerGeomKind::Polygon),
        Placement::Polygon {
            strategy: PolygonStrategy::Polylabel
        }
    ));
}

#[test]
fn layer_kind_parses_vector_and_raster() {
    assert!(matches!(
        LayerKind::parse("point"),
        Some(LayerKind::Vector(LayerGeomKind::Point))
    ));
    assert!(matches!(
        LayerKind::parse("line"),
        Some(LayerKind::Vector(LayerGeomKind::Line))
    ));
    assert!(matches!(
        LayerKind::parse("polygon"),
        Some(LayerKind::Vector(LayerGeomKind::Polygon))
    ));
    assert!(matches!(LayerKind::parse("raster"), Some(LayerKind::Raster)));
    assert_eq!(LayerKind::parse("query"), None);
    assert_eq!(LayerKind::parse(""), None);
}
