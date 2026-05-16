//! layer kind discriminants + per-kind default placement.

use crate::label::{LineAngleMode, Placement, PolygonStrategy};

/// Layer geometry kind. Mirrors the layer `type:` field in service config
/// for vector layers. Raster layers are discriminated one level up via
/// [`LayerKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerGeomKind {
    Point,
    Line,
    Polygon,
}

impl LayerGeomKind {
    /// Parse the `type:` field of a vector layer. Returns `None` for raster
    /// or unknown values; use [`LayerKind::parse`] when the caller needs to
    /// distinguish vector vs raster.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "point" => Some(Self::Point),
            "line" => Some(Self::Line),
            "polygon" => Some(Self::Polygon),
            _ => None,
        }
    }
}

/// Top-level layer kind: vector (with an inner geometry kind) or raster.
/// Dispatch sites that branch the compiler / runtime pipeline match on this
/// enum; adding a variant breaks compilation at every dispatch hub by design.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    /// Vector layer with a specific geometry kind.
    Vector(LayerGeomKind),
    /// Raster layer. Source binding and render path are not vector-shaped.
    Raster,
}

impl LayerKind {
    /// Parse the `type:` field of a layer config. Accepts the vector kinds
    /// understood by [`LayerGeomKind`] plus `"raster"`. Returns `None` for
    /// unknown values; callers decide whether to fall back or reject.
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(g) = LayerGeomKind::parse(s) {
            return Some(Self::Vector(g));
        }
        match s {
            "raster" => Some(Self::Raster),
            _ => None,
        }
    }
}

/// Default placement for a layer with no explicit `placement:` block.
/// lines repeat at 250 m with a 25° angle gate; everything else
/// gets a single point anchor.
#[must_use]
pub fn default_placement(kind: LayerGeomKind) -> Placement {
    match kind {
        LayerGeomKind::Line => Placement::Line {
            repeat_m: 250.0,
            max_angle_delta_deg: 25.0,
            angle_mode: LineAngleMode::Auto,
        },
        LayerGeomKind::Polygon => Placement::Polygon {
            strategy: PolygonStrategy::Polylabel,
        },
        LayerGeomKind::Point => Placement::Point,
    }
}

#[cfg(test)]
mod tests {
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
}
