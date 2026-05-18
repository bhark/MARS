//! wire-shaped mirrors of `mars_style` types used by the emitter.
//!
//! these stay string-friendly so the emitter can keep its one-pass string
//! body without dragging full size-bearing variants from the upstream model
//! into the translator's intermediate state.

use mars_style::Colour;

/// Marker shape recognised by [`mars_style::MarkerSymbol`]. Kept as a small
/// local enum (not the upstream `MarkerSymbol`) so emission can stay
/// string-based without dragging full size-bearing variants into the
/// translator's intermediate model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkerKind {
    Circle,
    Square,
    Triangle,
    Cross,
    X,
    Pin,
}

impl MarkerKind {
    pub(crate) fn from_lowercase(s: &str) -> Option<Self> {
        match s {
            "circle" => Some(Self::Circle),
            "square" => Some(Self::Square),
            "triangle" => Some(Self::Triangle),
            "cross" => Some(Self::Cross),
            "x" => Some(Self::X),
            "pin" => Some(Self::Pin),
            _ => None,
        }
    }

    pub(crate) fn as_wire(self) -> &'static str {
        match self {
            Self::Circle => "circle",
            Self::Square => "square",
            Self::Triangle => "triangle",
            Self::Cross => "cross",
            Self::X => "x",
            Self::Pin => "pin",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SymbolDef {
    /// MapServer SYMBOL TYPE ELLIPSE / VECTOR with a circular point list.
    Circle,
    /// SYMBOL TYPE HATCH. ANGLE and SIZE are symbol-level defaults; STYLE
    /// can override via ANGLE/SIZE/WIDTH/COLOR on the referencing STYLE.
    Hatch { angle_deg: Option<f32>, size: Option<f32> },
    /// VECTOR with a recognised shape name. Unknown shape names are dropped
    /// at SYMBOL parse time so consumers don't have to re-validate strings.
    NamedShape(MarkerKind),
    /// SYMBOL TYPE VECTOR with explicit POINTS x1 y1 x2 y2 ... and optional
    /// FILLED. Maps to `mars_style::MarkerSymbol::VectorShape` at emit time.
    VectorShape {
        points: Vec<(f32, f32)>,
        anchor: Option<(f32, f32)>,
        filled: bool,
    },
    /// SYMBOL TYPE TRUETYPE plus FONT + CHARACTER. Maps to
    /// `mars_style::MarkerSymbol::Glyph` at emit time.
    Glyph { font_family: String, character: String },
    /// SYMBOL TYPE PIXMAP. Resolves at use-site to
    /// `EmitFill::Image { name }` so styles route through the renderer's
    /// image registry. The IMAGE source path (when present in the mapfile)
    /// is captured for diagnostics but not used by the importer; the
    /// operator is responsible for placing the bitmap under
    /// `compiler.images_dir/<name>.<ext>` so the compiler bundles it.
    Pixmap { source_image: Option<String> },
    /// SYMBOL TYPE we recognise as a real mapfile directive but have not yet
    /// implemented (CARTOLINE, future TYPE additions). Held as a typed
    /// signal so the use-site warn carries the actual TYPE string; follows
    /// principle 5 of `docs/EXTENDING.md`.
    NotImplemented { raw_type: String },
}

#[derive(Debug, Clone)]
pub(crate) struct StyleDef {
    pub(crate) name: String,
    pub(crate) style_type: String,
    pub(crate) fill: Option<EmitFill>,
    pub(crate) stroke: Option<Colour>,
    pub(crate) stroke_width: Option<f32>,
    pub(crate) stroke_dasharray: Option<Vec<f32>>,
    pub(crate) stroke_linejoin: Option<&'static str>,
    pub(crate) stroke_linecap: Option<&'static str>,
    pub(crate) marker: Option<EmitMarker>,
    pub(crate) opacity: Option<f32>,
    pub(crate) stroke_offset_px: Option<f32>,
    pub(crate) stroke_gap: Option<EmitStrokeGap>,
    /// `mars_style::GeomTransform` wire value (`"start" | "end" | "vertices"`).
    pub(crate) geom_transform: Option<&'static str>,
    /// `MINFEATURESIZE` threshold in pixels. Per-pass gate dropped at render
    /// time when the feature's pixel-bbox extent falls below the value.
    pub(crate) min_feature_size_px: Option<f32>,
    /// Per-pass blend mode lifted from layer-scope `COMPOSITE { COMPOP }`.
    /// `None` falls back to the renderer default (source-over).
    pub(crate) blend_mode: Option<mars_style::BlendMode>,
    pub(crate) font_family: Option<String>,
    pub(crate) font_size: Option<f32>,
    pub(crate) halo_color: Option<Colour>,
    pub(crate) halo_width: Option<f32>,
    /// Label-style priority lifted from mapserver LABEL PRIORITY 1..10. The
    /// MARS LabelStyle uses `u16` but config validation accepts the same
    /// range; the emitter renders as an integer.
    pub(crate) priority: Option<u16>,
    /// Label-style minimum collision distance, mirroring LABEL MINDISTANCE.
    pub(crate) min_distance: Option<f32>,
    /// Anchor keyword (LABEL POSITION).
    pub(crate) position: Option<mars_style::AnchorPosition>,
    /// Pixel offset (LABEL OFFSET dx dy).
    pub(crate) offset_px: Option<(f32, f32)>,
    /// Static label rotation in degrees (numeric LABEL ANGLE).
    pub(crate) angle_deg: Option<f32>,
    /// `[col]` form on LABEL.ANGLE - resolves rotation from the attribute
    /// at render time. When set the `angle_deg` field is ignored on emit.
    pub(crate) angle_attribute: Option<String>,
    /// `LABEL PARTIALS` - when true, allow labels to extend past the canvas
    /// edge. Default is `false` to match mapserver.
    pub(crate) partials: Option<bool>,
    /// `LABEL FORCE` - skip collision detection.
    pub(crate) force: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EmitFill {
    /// Bare hex string: emits as `fill: "#rrggbb"`.
    Hex(Colour),
    /// Tagged hatch map.
    Hatch {
        spacing: f32,
        angle_deg: f32,
        line_width: f32,
        colour: Colour,
    },
    /// Tagged image-pattern map. `name` references an entry in the
    /// compiler's images_dir; mapfile importer derives it from the SYMBOL
    /// name. Emits as `fill: { kind: image, name: "<n>" }`.
    Image { name: String },
}

/// Authored numeric: a static literal or an attribute reference. Mirrors
/// `mars_style::NumericField`; held as a thin local enum so the emitter
/// stays string-based.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EmitNumeric {
    Static(f32),
    Attribute(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EmitMarker {
    /// Built-in marker shape with a pixel size and optional rotation.
    Builtin {
        kind: MarkerKind,
        size: f32,
        size_attribute: Option<String>,
        angle: Option<EmitNumeric>,
    },
    /// `mars_style::MarkerSymbol::VectorShape`: explicit point list.
    Vector {
        points: Vec<(f32, f32)>,
        anchor: Option<(f32, f32)>,
        filled: bool,
        size: f32,
        size_attribute: Option<String>,
        angle: Option<EmitNumeric>,
    },
    /// `mars_style::MarkerSymbol::Glyph`: TrueType character.
    Glyph {
        font_family: String,
        character: String,
        size: f32,
        size_attribute: Option<String>,
        angle: Option<EmitNumeric>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EmitStrokeGap {
    pub(crate) interval_px: f32,
    pub(crate) initial_px: f32,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct EmitLinePlacement {
    pub(crate) repeat_m: Option<f64>,
    pub(crate) max_angle_delta_deg: Option<f32>,
    /// `auto` (block-rotated at sample tangent) or `follow` (per-glyph
    /// rotation). `None` lets the runtime default kick in (currently
    /// `auto`); explicitly set when the mapfile uses `ANGLE FOLLOW`.
    pub(crate) angle_mode: Option<mars_style::LineAngleMode>,
}
