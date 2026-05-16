//! MARS style model. a small fixed vocabulary close to SVG.
//!
//! No rendering happens here; the renderer adapter consumes the compiled form.

#![forbid(unsafe_code)]

mod colour;
mod label;
mod layer;
mod marker;
mod numeric;
mod scaled;
mod stroke;
mod style;
mod stylesheet;

pub use colour::{Colour, FillPaint};
pub use label::{
    AnchorPosition, Halo, LabelStyle, LabelSurvival, LineAngleMode, Placement, PolygonStrategy, ResolvedLabelStyle,
};
pub use layer::{LayerGeomKind, LayerKind, default_placement};
pub use marker::{MarkerShape, MarkerSymbol};
pub use numeric::NumericField;
pub use scaled::ScaledSize;
pub use stroke::{BlendMode, GeomTransform, LineCap, LineJoin, StrokeGap};
pub use style::{ResolvedMarker, ResolvedStyle, Style};
pub use stylesheet::Stylesheet;

#[derive(Debug, thiserror::Error)]
pub enum StyleError {
    #[error("invalid colour: {0}")]
    InvalidColour(String),
    #[error("invalid style: {0}")]
    Invalid(String),
}
