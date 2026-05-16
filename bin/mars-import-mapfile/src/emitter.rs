//! one-pass YAML emitter for the mapfile translator.
//!
//! intentionally string-based: the result is meant to be hand-edited.

use std::collections::HashMap;
use std::fmt::Write as _;

use mars_style::Colour;
use tracing::warn;

#[derive(Debug, Default)]
pub(crate) struct Skeleton {
    pub(crate) service_name: Option<String>,
    pub(crate) service_title: Option<String>,
    /// MAP-level METADATA harvested from `ows_*` / `wms_*` keys. Populated by
    /// `parse_map_metadata`; consumed by `render` when emitting the service
    /// block. Empty defaults preserve the placeholder-only output for inputs
    /// without metadata.
    pub(crate) service_meta: ServiceMetaSkeleton,
    /// MAP-level `PROJECTION { ... }` capture (e.g. `EPSG:25832`). Falls
    /// back as source_crs for OGR layers that have no layer-scope PROJECTION
    /// block of their own. `None` when the mapfile has no MAP PROJECTION.
    pub(crate) map_projection: Option<String>,
    pub(crate) layers: Vec<LayerSkeleton>,
    pub(crate) styles: Vec<StyleDef>,
    /// mapfile-level SYMBOL definitions keyed by name. consumed by STYLE
    /// blocks via STYLE.SYMBOL "<name>"; not emitted into YAML directly -
    /// each STYLE that uses a symbol carries the resolved marker/fill on
    /// its `StyleDef`.
    pub(crate) symbols: HashMap<String, SymbolDef>,
}

/// MAP-level service metadata harvested from the mapfile METADATA block.
/// Each field carries either a parsed value or the absent state; the emitter
/// is responsible for falling back to placeholders when nothing is set.
#[derive(Debug, Default)]
pub(crate) struct ServiceMetaSkeleton {
    pub(crate) title_override: Option<String>,
    pub(crate) abstract_: Option<String>,
    pub(crate) keywords: Vec<String>,
    pub(crate) online_resource: Option<String>,
    pub(crate) fees: Option<String>,
    pub(crate) access_constraints: Option<String>,
    pub(crate) encoding: Option<String>,
    pub(crate) bbox_extended: Option<bool>,
    pub(crate) sld_enabled: Option<bool>,
    pub(crate) advertised_crs: Vec<String>,
    pub(crate) contact_person: Option<String>,
    pub(crate) contact_position: Option<String>,
    pub(crate) contact_organization: Option<String>,
    pub(crate) contact_phone: Option<String>,
    pub(crate) contact_fax: Option<String>,
    pub(crate) contact_email: Option<String>,
    pub(crate) address_type: Option<String>,
    pub(crate) address_street: Option<String>,
    pub(crate) address_city: Option<String>,
    pub(crate) address_state: Option<String>,
    pub(crate) address_postcode: Option<String>,
    pub(crate) address_country: Option<String>,
    pub(crate) getmap_formats: Vec<String>,
    pub(crate) getfeatureinfo_formats: Vec<String>,
    pub(crate) getlegend_formats: Vec<String>,
    pub(crate) authorities: Vec<(String, String)>,
    pub(crate) identifiers: Vec<(String, String)>,
}

impl ServiceMetaSkeleton {
    /// True when any structured contact sub-field is set. Drives whether the
    /// emitter writes a full `contact:` block or keeps the top-level
    /// `contact_email` shorthand.
    pub(crate) fn has_structured_contact(&self) -> bool {
        self.contact_person.is_some()
            || self.contact_position.is_some()
            || self.contact_organization.is_some()
            || self.contact_phone.is_some()
            || self.contact_fax.is_some()
            || self.address_type.is_some()
            || self.address_street.is_some()
            || self.address_city.is_some()
            || self.address_state.is_some()
            || self.address_postcode.is_some()
            || self.address_country.is_some()
    }
}

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
    pub(crate) marker: Option<EmitMarker>,
    pub(crate) opacity: Option<f32>,
    pub(crate) stroke_offset_px: Option<f32>,
    pub(crate) stroke_gap: Option<EmitStrokeGap>,
    /// `mars_style::GeomTransform` wire value (`"start" | "end" | "vertices"`).
    pub(crate) geom_transform: Option<&'static str>,
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EmitMarker {
    /// Built-in marker shape with a pixel size.
    Builtin { kind: MarkerKind, size: f32 },
    /// `mars_style::MarkerSymbol::VectorShape`: explicit point list.
    Vector {
        points: Vec<(f32, f32)>,
        anchor: Option<(f32, f32)>,
        filled: bool,
        size: f32,
    },
    /// `mars_style::MarkerSymbol::Glyph`: TrueType character.
    Glyph {
        font_family: String,
        character: String,
        size: f32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EmitStrokeGap {
    pub(crate) interval_px: f32,
    pub(crate) initial_px: f32,
}

#[derive(Debug, Default)]
pub(crate) struct LayerSkeleton {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) abstract_: Option<String>,
    pub(crate) geom_kind: Option<String>,
    pub(crate) sources: Vec<SourceSkeleton>,
    pub(crate) classes: Vec<ClassSkeleton>,
    pub(crate) label: Option<LabelSkeleton>,
    /// Slash-prefixed WMS group path (`/A/B/C`). `None` puts the layer at
    /// the service root.
    pub(crate) group: Option<String>,
    /// Per-layer WMS metadata harvested from a `METADATA { ... }` block.
    pub(crate) wms: LayerWmsSkeleton,
}

/// Per-layer WMS metadata in the form that flows from the importer into
/// the emitted YAML's per-layer block. Each field is absent by default and
/// the emitter writes only fields with content.
#[derive(Debug, Default)]
pub(crate) struct LayerWmsSkeleton {
    pub(crate) keywords: Vec<String>,
    pub(crate) metadata_urls: Vec<(String, String, String)>, // (type, format, href)
    pub(crate) authorities: Vec<(String, String)>,
    pub(crate) identifiers: Vec<(String, String)>,
    pub(crate) opaque: Option<bool>,
    pub(crate) advertised_crs: Vec<String>,
    pub(crate) attribution: Option<LayerAttributionSkeleton>,
    pub(crate) include_items: Option<IncludeItemsSkeleton>,
    pub(crate) request_gating: LayerGatingSkeleton,
}

#[derive(Debug, Default)]
pub(crate) struct LayerAttributionSkeleton {
    pub(crate) title: Option<String>,
    pub(crate) online_resource: Option<String>,
    pub(crate) logo_format: Option<String>,
    pub(crate) logo_href: Option<String>,
    pub(crate) logo_width: Option<u32>,
    pub(crate) logo_height: Option<u32>,
}

#[derive(Debug)]
pub(crate) enum IncludeItemsSkeleton {
    All,
    None,
    Explicit(Vec<String>),
}

#[derive(Debug, Default)]
pub(crate) struct LayerGatingSkeleton {
    pub(crate) get_capabilities: Option<bool>,
    pub(crate) get_map: Option<bool>,
    pub(crate) get_feature_info: Option<bool>,
    pub(crate) get_legend_graphic: Option<bool>,
    pub(crate) get_styles: Option<bool>,
    pub(crate) describe_layer: Option<bool>,
}

impl LayerGatingSkeleton {
    pub(crate) fn is_empty(&self) -> bool {
        self.get_capabilities.is_none()
            && self.get_map.is_none()
            && self.get_feature_info.is_none()
            && self.get_legend_graphic.is_none()
            && self.get_styles.is_none()
            && self.describe_layer.is_none()
    }
}

impl LayerWmsSkeleton {
    pub(crate) fn is_empty(&self) -> bool {
        self.keywords.is_empty()
            && self.metadata_urls.is_empty()
            && self.authorities.is_empty()
            && self.identifiers.is_empty()
            && self.opaque.is_none()
            && self.advertised_crs.is_empty()
            && self.attribution.is_none()
            && self.include_items.is_none()
            && self.request_gating.is_empty()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SourceSkeleton {
    pub(crate) max_denom_exclusive: Option<u64>,
    /// Either a table reference (`from:`) or an inline SELECT (`sql:`).
    pub(crate) source: BindingSource,
    pub(crate) filter: Option<String>,
    pub(crate) geometry_column: String,
    pub(crate) id_column: Option<String>,
    pub(crate) attributes: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum BindingSource {
    /// `from: <table>` form.
    Table(String),
    /// `sql: <SELECT ...>` form for inline subqueries the mapfile DATA block
    /// could not be lifted to a plain table reference. Snapshot-only.
    Sql(String),
    /// vectorfile binding lifted from a `CONNECTIONTYPE OGR` + `CONNECTION`
    /// pair. emits `uri: / format: / source_crs:` on the per-binding line.
    VectorFile(VectorFileBinding),
}

/// vectorfile binding fields. format is the snake_case wire spelling
/// (`flat_geobuf` | `geo_json`); source_crs is the layer's PROJECTION (with
/// MAP-level fallback) in `EPSG:NNNN` form.
#[derive(Debug, Clone)]
pub(crate) struct VectorFileBinding {
    pub uri: String,
    pub format: String,
    pub source_crs: String,
}

impl SourceSkeleton {
    /// Diagnostic descriptor for the binding's table reference. Used by
    /// tests that inspect the produced source layout. SQL bindings collapse
    /// to a sentinel so existing assertions read straight through.
    #[cfg(test)]
    pub(crate) fn source_table(&self) -> &str {
        match &self.source {
            BindingSource::Table(t) => t.as_str(),
            BindingSource::Sql(_) => "<sql>",
            BindingSource::VectorFile(_) => "<vectorfile>",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClassSkeleton {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) when: Option<String>,
    pub(crate) min_scale_denom: Option<u64>,
    pub(crate) max_scale_denom: Option<u64>,
    pub(crate) style_ref: String,
    pub(crate) label: Option<LabelSkeleton>,
}

#[derive(Debug, Clone)]
pub(crate) struct LabelSkeleton {
    pub(crate) text: String,
    pub(crate) style_ref: String,
    /// `ANGLE FOLLOW` (mapserver) -> `placement: { kind: line, ... }`.
    pub(crate) placement_line: Option<EmitLinePlacement>,
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

/// slugify a name for YAML identifiers: lowercase, non-alnum → '_'.
pub(crate) fn slugify(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

/// quote a YAML string using simple double-quoting; escapes `"` and `\`.
fn yaml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// quote a `Colour` as a YAML string (`"#rrggbb"` or `"#rrggbbaa"`).
fn quote_colour(c: Colour) -> String {
    yaml_quote(&c.to_string())
}

/// Emit the optional `service.*` fields harvested from MAP-level METADATA.
/// Each field is written only when set. Empty list fields are skipped.
fn write_service_meta(out: &mut String, svc: &ServiceMetaSkeleton) {
    if !svc.keywords.is_empty() {
        let _ = writeln!(out, "  keywords:");
        for kw in &svc.keywords {
            let _ = writeln!(out, "    - {}", yaml_quote(kw));
        }
    }
    if let Some(v) = &svc.online_resource {
        let _ = writeln!(out, "  online_resource: {}", yaml_quote(v));
    }
    if let Some(v) = &svc.fees {
        let _ = writeln!(out, "  fees: {}", yaml_quote(v));
    }
    if let Some(v) = &svc.access_constraints {
        let _ = writeln!(out, "  access_constraints: {}", yaml_quote(v));
    }
    if let Some(v) = &svc.encoding {
        let _ = writeln!(out, "  encoding: {}", yaml_quote(v));
    }
    if let Some(b) = svc.bbox_extended {
        let _ = writeln!(out, "  bbox_extended: {b}");
    }
    if let Some(b) = svc.sld_enabled {
        let _ = writeln!(out, "  sld_enabled: {b}");
    }
    if !svc.advertised_crs.is_empty() {
        let _ = writeln!(out, "  advertised_crs:");
        for crs in &svc.advertised_crs {
            let _ = writeln!(out, "    - {}", yaml_quote(crs));
        }
    }
    if svc.has_structured_contact() || svc.contact_email.is_some() {
        let _ = writeln!(out, "  contact:");
        if let Some(v) = &svc.contact_person {
            let _ = writeln!(out, "    person: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_position {
            let _ = writeln!(out, "    position: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_organization {
            let _ = writeln!(out, "    organization: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_phone {
            let _ = writeln!(out, "    phone: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_fax {
            let _ = writeln!(out, "    fax: {}", yaml_quote(v));
        }
        if let Some(v) = &svc.contact_email {
            let _ = writeln!(out, "    email: {}", yaml_quote(v));
        }
        let any_addr = svc.address_type.is_some()
            || svc.address_street.is_some()
            || svc.address_city.is_some()
            || svc.address_state.is_some()
            || svc.address_postcode.is_some()
            || svc.address_country.is_some();
        if any_addr {
            let _ = writeln!(out, "    address:");
            if let Some(v) = &svc.address_type {
                let _ = writeln!(out, "      type: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_street {
                let _ = writeln!(out, "      street: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_city {
                let _ = writeln!(out, "      city: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_state {
                let _ = writeln!(out, "      state_or_province: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_postcode {
                let _ = writeln!(out, "      postcode: {}", yaml_quote(v));
            }
            if let Some(v) = &svc.address_country {
                let _ = writeln!(out, "      country: {}", yaml_quote(v));
            }
        }
    }
    if !svc.authorities.is_empty() {
        let _ = writeln!(out, "  authorities:");
        for (n, h) in &svc.authorities {
            let _ = writeln!(out, "    - name: {}", yaml_quote(n));
            let _ = writeln!(out, "      href: {}", yaml_quote(h));
        }
    }
    if !svc.identifiers.is_empty() {
        let _ = writeln!(out, "  identifiers:");
        for (a, v) in &svc.identifiers {
            let _ = writeln!(out, "    - authority: {}", yaml_quote(a));
            let _ = writeln!(out, "      value: {}", yaml_quote(v));
        }
    }
    let any_fmt =
        !svc.getmap_formats.is_empty() || !svc.getfeatureinfo_formats.is_empty() || !svc.getlegend_formats.is_empty();
    if any_fmt {
        let _ = writeln!(out, "  formats:");
        if !svc.getmap_formats.is_empty() {
            let _ = writeln!(out, "    get_map:");
            for v in &svc.getmap_formats {
                let _ = writeln!(out, "      - {}", yaml_quote(v));
            }
        }
        if !svc.getfeatureinfo_formats.is_empty() {
            let _ = writeln!(out, "    get_feature_info:");
            for v in &svc.getfeatureinfo_formats {
                let _ = writeln!(out, "      - {}", yaml_quote(v));
            }
        }
        if !svc.getlegend_formats.is_empty() {
            let _ = writeln!(out, "    get_legend_graphic:");
            for v in &svc.getlegend_formats {
                let _ = writeln!(out, "      - {}", yaml_quote(v));
            }
        }
    }
}

/// Emit per-layer WMS metadata fields under the layer body. Indent assumes a
/// `layers:` list item ("  - ...") - each top-level metadata field starts at
/// column 4 to align with siblings like `title:` and `group:`.
fn write_layer_wms_metadata(out: &mut String, wms: &LayerWmsSkeleton) {
    if wms.is_empty() {
        return;
    }
    if !wms.keywords.is_empty() {
        let _ = writeln!(out, "    keywords:");
        for kw in &wms.keywords {
            let _ = writeln!(out, "      - {}", yaml_quote(kw));
        }
    }
    if !wms.metadata_urls.is_empty() {
        let _ = writeln!(out, "    metadata_urls:");
        for (t, f, h) in &wms.metadata_urls {
            let _ = writeln!(out, "      - type: {}", yaml_quote(t));
            let _ = writeln!(out, "        format: {}", yaml_quote(f));
            let _ = writeln!(out, "        href: {}", yaml_quote(h));
        }
    }
    if !wms.authorities.is_empty() {
        let _ = writeln!(out, "    authorities:");
        for (n, h) in &wms.authorities {
            let _ = writeln!(out, "      - name: {}", yaml_quote(n));
            let _ = writeln!(out, "        href: {}", yaml_quote(h));
        }
    }
    if !wms.identifiers.is_empty() {
        let _ = writeln!(out, "    identifiers:");
        for (a, v) in &wms.identifiers {
            let _ = writeln!(out, "      - authority: {}", yaml_quote(a));
            let _ = writeln!(out, "        value: {}", yaml_quote(v));
        }
    }
    if let Some(o) = wms.opaque {
        let _ = writeln!(out, "    opaque: {o}");
    }
    if !wms.advertised_crs.is_empty() {
        let _ = writeln!(out, "    advertised_crs:");
        for crs in &wms.advertised_crs {
            let _ = writeln!(out, "      - {}", yaml_quote(crs));
        }
    }
    if let Some(a) = &wms.attribution {
        let _ = writeln!(out, "    attribution:");
        if let Some(t) = &a.title {
            let _ = writeln!(out, "      title: {}", yaml_quote(t));
        }
        if let Some(or) = &a.online_resource {
            let _ = writeln!(out, "      online_resource: {}", yaml_quote(or));
        }
        if a.logo_format.is_some() || a.logo_href.is_some() || a.logo_width.is_some() || a.logo_height.is_some() {
            let _ = writeln!(out, "      logo:");
            if let Some(f) = &a.logo_format {
                let _ = writeln!(out, "        format: {}", yaml_quote(f));
            }
            if let Some(h) = &a.logo_href {
                let _ = writeln!(out, "        href: {}", yaml_quote(h));
            }
            if let Some(w) = a.logo_width {
                let _ = writeln!(out, "        width: {w}");
            }
            if let Some(h) = a.logo_height {
                let _ = writeln!(out, "        height: {h}");
            }
        }
    }
    if let Some(items) = &wms.include_items {
        match items {
            IncludeItemsSkeleton::All => {
                let _ = writeln!(out, "    include_items: {{ mode: all }}");
            }
            IncludeItemsSkeleton::None => {
                let _ = writeln!(out, "    include_items: {{ mode: none }}");
            }
            IncludeItemsSkeleton::Explicit(names) => {
                let _ = writeln!(out, "    include_items:");
                let _ = writeln!(out, "      mode: explicit");
                let _ = writeln!(out, "      names:");
                for n in names {
                    let _ = writeln!(out, "        - {}", yaml_quote(n));
                }
            }
        }
    }
    if !wms.request_gating.is_empty() {
        let _ = writeln!(out, "    request_gating:");
        let rg = &wms.request_gating;
        if let Some(b) = rg.get_capabilities {
            let _ = writeln!(out, "      get_capabilities: {b}");
        }
        if let Some(b) = rg.get_map {
            let _ = writeln!(out, "      get_map: {b}");
        }
        // get_feature_info=true already surfaces via the legacy enable flag
        // emitted above; here we record only the explicit deny case to avoid
        // double-emission of the same fact.
        if let Some(false) = rg.get_feature_info {
            let _ = writeln!(out, "      get_feature_info: false");
        }
        if let Some(b) = rg.get_legend_graphic {
            let _ = writeln!(out, "      get_legend_graphic: {b}");
        }
        if let Some(b) = rg.get_styles {
            let _ = writeln!(out, "      get_styles: {b}");
        }
        if let Some(b) = rg.describe_layer {
            let _ = writeln!(out, "      describe_layer: {b}");
        }
    }
}

/// YAML wire spelling for `mars_style::AnchorPosition` (snake_case enum).
fn anchor_position_yaml(p: mars_style::AnchorPosition) -> &'static str {
    use mars_style::AnchorPosition;
    match p {
        AnchorPosition::Ul => "ul",
        AnchorPosition::Uc => "uc",
        AnchorPosition::Ur => "ur",
        AnchorPosition::Cl => "cl",
        AnchorPosition::Cc => "cc",
        AnchorPosition::Cr => "cr",
        AnchorPosition::Ll => "ll",
        AnchorPosition::Lc => "lc",
        AnchorPosition::Lr => "lr",
        AnchorPosition::Auto => "auto",
    }
}

/// YAML wire spelling for `mars_style::LineAngleMode`.
fn line_angle_mode_yaml(m: mars_style::LineAngleMode) -> &'static str {
    use mars_style::LineAngleMode;
    match m {
        LineAngleMode::Auto => "auto",
        LineAngleMode::Follow => "follow",
    }
}

/// render a marker into the style block under `    marker:`. flow-mapping
/// for built-ins and glyph (compact), block-mapping for vector shapes since
/// the point list is open-ended.
fn write_marker(out: &mut String, m: &EmitMarker) {
    match m {
        EmitMarker::Builtin { kind, size } => {
            let _ = writeln!(out, "    marker: {{ kind: {}, size: {size} }}", kind.as_wire());
        }
        EmitMarker::Glyph {
            font_family,
            character,
            size,
        } => {
            let _ = writeln!(
                out,
                "    marker: {{ kind: glyph, font_family: {}, character: {}, size: {size} }}",
                yaml_quote(font_family),
                yaml_quote(character)
            );
        }
        EmitMarker::Vector {
            points,
            anchor,
            filled,
            size,
        } => {
            let _ = writeln!(out, "    marker:");
            let _ = writeln!(out, "      kind: vector_shape");
            let pts = points
                .iter()
                .map(|(x, y)| format!("[{x}, {y}]"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(out, "      points: [{pts}]");
            if let Some((ax, ay)) = anchor {
                let _ = writeln!(out, "      anchor: [{ax}, {ay}]");
            }
            let _ = writeln!(out, "      filled: {filled}");
            let _ = writeln!(out, "      size: {size}");
        }
    }
}

/// render one class entry under `    classes:`. compact flow-mapping when the
/// class has no per-class label; expanded block-mapping when it does, so the
/// label sub-tree can be rendered on its own lines.
fn write_class(out: &mut String, cls: &ClassSkeleton) {
    let mut parts = vec![format!("name: {}", yaml_quote(&cls.name))];
    if let Some(title) = &cls.title {
        parts.push(format!("title: {}", yaml_quote(title)));
    }
    if let Some(when) = &cls.when {
        parts.push(format!("when: {}", yaml_quote(when)));
    }
    if cls.min_scale_denom.is_some() || cls.max_scale_denom.is_some() {
        let mut scale_parts: Vec<String> = Vec::new();
        if let Some(m) = cls.min_scale_denom {
            scale_parts.push(format!("min: {m}"));
        }
        if let Some(m) = cls.max_scale_denom {
            scale_parts.push(format!("max: {m}"));
        }
        parts.push(format!("scale: {{ {} }}", scale_parts.join(", ")));
    }
    parts.push(format!("style: {{ type: ref, name: {} }}", yaml_quote(&cls.style_ref)));

    let Some(lbl) = cls.label.as_ref() else {
        let _ = writeln!(out, "      - {{ {} }}", parts.join(", "));
        return;
    };

    // expanded form: scalars + style on their own lines, then label: block.
    let mut iter = parts.into_iter();
    if let Some(first) = iter.next() {
        let _ = writeln!(out, "      - {first}");
    }
    for p in iter {
        let _ = writeln!(out, "        {p}");
    }
    let _ = writeln!(out, "        label:");
    write_label_body(out, lbl, "          ");
}

/// render a [`LabelSkeleton`] under an externally-emitted `label:` key at
/// `indent`. shared between the layer-level and class-level label paths.
fn write_label_body(out: &mut String, lbl: &LabelSkeleton, indent: &str) {
    let _ = writeln!(out, "{indent}text: {}", yaml_quote(&lbl.text));
    let _ = writeln!(
        out,
        "{indent}style: {{ type: ref, name: {} }}",
        yaml_quote(&lbl.style_ref)
    );
    if let Some(p) = lbl.placement_line {
        let mut parts = vec!["kind: line".to_string()];
        if let Some(r) = p.repeat_m {
            parts.push(format!("repeat_m: {r}"));
        }
        if let Some(a) = p.max_angle_delta_deg {
            parts.push(format!("max_angle_delta_deg: {a}"));
        }
        if let Some(m) = p.angle_mode {
            parts.push(format!("angle_mode: {}", line_angle_mode_yaml(m)));
        }
        let _ = writeln!(out, "{indent}placement: {{ {} }}", parts.join(", "));
    }
}

/// default scale-band ladder used when `--bands` is not supplied.
/// caps are denom upper bounds (exclusive). the overview cap is finite
/// (1:10_000_000) - large enough for a country-wide view, small enough to
/// render cleanly in YAML; operators that need a wider ladder pass `--bands`.
pub(crate) fn default_bands() -> Vec<(String, u64)> {
    vec![
        ("detail".into(), 2_500),
        ("hi".into(), 12_500),
        ("mid".into(), 50_000),
        ("lo".into(), 250_000),
        ("overview".into(), 10_000_000),
    ]
}

/// expand an ordered ladder of caps into bands carrying their lower bound too.
/// band i covers `[prev_cap, cap)`; band 0's lower bound is 0.
struct BandWindow<'a> {
    name: &'a str,
    min: u64,
    cap: u64,
}

fn band_windows(bands: &[(String, u64)]) -> Vec<BandWindow<'_>> {
    let mut out = Vec::with_capacity(bands.len());
    let mut prev: u64 = 0;
    for (name, cap) in bands {
        out.push(BandWindow {
            name: name.as_str(),
            min: prev,
            cap: *cap,
        });
        prev = *cap;
    }
    out
}

/// per-tier emission inside a single band for a single layer.
struct EmittedTier<'a> {
    src: &'a SourceSkeleton,
    /// `None` = last tier of this band (no `max_denom_exclusive` rendered).
    max_denom: Option<u64>,
}

/// for each band, compute the tier-set this layer contributes.
/// returns `(band_name, Vec<EmittedTier>)` per band that the layer fully covers.
/// bands the layer only partially covers are dropped with a warn.
fn split_layer_into_bands<'a>(
    layer: &'a LayerSkeleton,
    windows: &[BandWindow<'a>],
) -> Vec<(&'a str, Vec<EmittedTier<'a>>)> {
    if layer.sources.is_empty() {
        return Vec::new();
    }

    // contiguous source intervals within a layer: [prev_max, this_max).
    // first source starts at 0; an open-ended `max_denom_exclusive` is u64::MAX.
    let mut intervals: Vec<(u64, u64, &SourceSkeleton)> = Vec::with_capacity(layer.sources.len());
    let mut prev: u64 = 0;
    for src in &layer.sources {
        let this = src.max_denom_exclusive.unwrap_or(u64::MAX);
        if this <= prev {
            warn!(
                layer = %layer.name,
                prev_max = prev,
                this_max = this,
                "layer sources not in strictly increasing max_denom order; skipping later tier"
            );
            continue;
        }
        intervals.push((prev, this, src));
        prev = this;
    }
    if intervals.is_empty() {
        return Vec::new();
    }
    let layer_min = intervals.first().map(|(m, _, _)| *m).unwrap_or(0);
    let layer_max = intervals.last().map(|(_, m, _)| *m).unwrap_or(0);

    let mut out: Vec<(&str, Vec<EmittedTier>)> = Vec::new();
    for w in windows {
        // skip bands the layer doesn't intersect at all.
        if w.cap <= layer_min || w.min >= layer_max {
            continue;
        }
        // partial coverage: layer doesn't fully span [w.min, w.cap).
        if layer_min > w.min || layer_max < w.cap {
            warn!(
                layer = %layer.name,
                band = %w.name,
                band_min = w.min,
                band_cap = w.cap,
                layer_min,
                layer_max,
                "layer partially overlaps band; dropping (validator requires full band coverage)"
            );
            continue;
        }

        // collect the source intervals that intersect this band.
        let in_band: Vec<&(u64, u64, &SourceSkeleton)> = intervals
            .iter()
            .filter(|(lo, hi, _)| *hi > w.min && *lo < w.cap)
            .collect();

        let n = in_band.len();
        let mut tiers: Vec<EmittedTier> = Vec::with_capacity(n);
        for (idx, (_lo, hi, src)) in in_band.iter().enumerate() {
            let is_last = idx + 1 == n;
            let effective = (*hi).min(w.cap);
            let max_denom = if is_last && effective == w.cap {
                None
            } else {
                Some(effective)
            };
            tiers.push(EmittedTier { src, max_denom });
        }
        out.push((w.name, tiers));
    }
    out
}

pub(crate) fn render(skel: &Skeleton, bands: &[(String, u64)]) -> String {
    let mut out = String::new();
    out.push_str("# Generated by mars-import-mapfile\n");
    out.push_str("# Operator metadata below uses ${VAR:-default} placeholders.\n");
    out.push_str("# Review and replace before production use.\n\n");

    let name = skel.service_name.as_deref().unwrap_or("unnamed");
    let title = skel
        .service_meta
        .title_override
        .as_deref()
        .or(skel.service_title.as_deref())
        .unwrap_or(name);
    let svc = &skel.service_meta;
    let abstract_text = svc.abstract_.as_deref().unwrap_or("Imported from mapfile");

    let _ = writeln!(out, "service:");
    let _ = writeln!(out, "  name: {}", yaml_quote(name));
    let _ = writeln!(out, "  title: {}", yaml_quote(title));
    if svc.abstract_.is_some() {
        let _ = writeln!(out, "  abstract: {}", yaml_quote(abstract_text));
    } else {
        // legacy placeholder; emitted with the same quoted spelling existing
        // fixtures lock down. parsed abstract_ values take precedence via the
        // branch above and route through yaml_quote for safety.
        let _ = writeln!(out, "  abstract: \"Imported from mapfile\"");
    }
    // top-level shorthand only when no structured contact block is emitted.
    // a structured contact (set below) carries the email under `contact.email`
    // and takes precedence at capabilities-emit time.
    if !svc.has_structured_contact() {
        if let Some(email) = svc.contact_email.as_deref() {
            let _ = writeln!(out, "  contact_email: {}", yaml_quote(email));
        } else {
            // legacy placeholder unquoted to match existing fixture goldens
            let _ = writeln!(out, "  contact_email: ops@example.org");
        }
    }
    write_service_meta(&mut out, svc);
    let _ = writeln!(out);

    let has_vectorfile = skel
        .layers
        .iter()
        .flat_map(|l| &l.sources)
        .any(|s| matches!(s.source, BindingSource::VectorFile(_)));
    if has_vectorfile {
        // plural form: postgis + vectorfile coexist. per-binding `source:`
        // fields disambiguate; mars-config folds the wire shape into the
        // typed `sources:` model.
        let _ = writeln!(out, "sources:");
        let _ = writeln!(out, "  - id: pg");
        let _ = writeln!(out, "    type: postgis");
        let _ = writeln!(out, "    dsn: \"${{PG_DSN}}\"");
        let _ = writeln!(out, "    native_crs: ${{MARS_NATIVE_CRS:-EPSG:25832}}");
        let _ = writeln!(out, "  - id: ogr");
        let _ = writeln!(out, "    type: vectorfile");
        let _ = writeln!(out, "    native_crs: ${{MARS_NATIVE_CRS:-EPSG:25832}}");
        let _ = writeln!(out, "    cache_dir: /var/cache/mars/vectorfile");
    } else {
        let _ = writeln!(out, "source:");
        let _ = writeln!(out, "  type: postgis");
        let _ = writeln!(out, "  dsn: \"${{PG_DSN}}\"");
        let _ = writeln!(out, "  native_crs: ${{MARS_NATIVE_CRS:-EPSG:25832}}");
    }
    let _ = writeln!(out);

    let _ = writeln!(out, "artifacts:");
    let _ = writeln!(out, "  store:");
    let _ = writeln!(out, "    type: fs");
    let _ = writeln!(out, "    path: \"${{MARS_STORE_PATH}}\"");
    let _ = writeln!(out, "  cache:");
    let _ = writeln!(out, "    path: \"${{MARS_CACHE_PATH}}\"");
    let _ = writeln!(out, "    max_size: 256MiB");
    let _ = writeln!(out, "    eviction: lru");
    let _ = writeln!(out);

    // scales / cells
    let _ = writeln!(out, "scales:");
    let _ = writeln!(out, "  bands:");
    for (name, cap) in bands {
        let _ = writeln!(out, "    - {{ name: {name}, max_denom_exclusive: {cap} }}");
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "cells:");
    let _ = writeln!(out, "  grid: regular");
    let _ = writeln!(out, "  origin: [0, 0]");
    let _ = writeln!(out, "  size_per_band:");
    for (name, _) in bands {
        let _ = writeln!(out, "    {name}: ${{MARS_CELL_SIZE:-1024m}}");
    }
    let _ = writeln!(out, "  extent:");
    let _ = writeln!(out, "    min_x: ${{MARS_EXTENT_MIN_X:-0}}");
    let _ = writeln!(out, "    min_y: ${{MARS_EXTENT_MIN_Y:-0}}");
    let _ = writeln!(out, "    max_x: ${{MARS_EXTENT_MAX_X:-0}}");
    let _ = writeln!(out, "    max_y: ${{MARS_EXTENT_MAX_Y:-0}}");
    let _ = writeln!(out);

    let _ = writeln!(out, "interfaces:");
    let _ = writeln!(out, "  wms:");
    let _ = writeln!(out, "    enabled: true");
    let _ = writeln!(out, "    versions: [\"1.3.0\"]");
    let _ = writeln!(out, "    formats: [\"image/png\", \"image/jpeg\"]");
    let _ = writeln!(out);

    // styles
    if !skel.styles.is_empty() {
        let _ = writeln!(out, "styles:");
        for st in &skel.styles {
            let _ = writeln!(out, "  {}:", st.name);
            let _ = writeln!(out, "    type: {}", st.style_type);
            if st.style_type == "label" {
                if let Some(ref f) = st.font_family {
                    let _ = writeln!(out, "    font_family: {}", yaml_quote(f));
                }
                if let Some(v) = st.font_size {
                    let _ = writeln!(out, "    font_size: {v}");
                }
                if let Some(EmitFill::Hex(c)) = &st.fill {
                    let _ = writeln!(out, "    fill: {}", quote_colour(*c));
                }
                if let Some(c) = st.halo_color {
                    let w = st.halo_width.unwrap_or(1.0);
                    let _ = writeln!(out, "    halo: {{ color: {}, width: {w} }}", quote_colour(c));
                }
                if let Some(p) = st.priority {
                    let _ = writeln!(out, "    priority: {p}");
                }
                if let Some(d) = st.min_distance {
                    let _ = writeln!(out, "    min_distance: {d}");
                }
                if let Some(pos) = st.position {
                    let _ = writeln!(out, "    position: {}", anchor_position_yaml(pos));
                }
                if let Some((dx, dy)) = st.offset_px {
                    let _ = writeln!(out, "    offset_px: [{dx}, {dy}]");
                }
                if let Some(a) = st.angle_deg {
                    let _ = writeln!(out, "    angle_deg: {a}");
                }
                if let Some(true) = st.partials {
                    let _ = writeln!(out, "    partials: true");
                }
                if let Some(true) = st.force {
                    let _ = writeln!(out, "    force: true");
                }
            } else {
                match &st.fill {
                    Some(EmitFill::Hex(c)) => {
                        let _ = writeln!(out, "    fill: {}", quote_colour(*c));
                    }
                    Some(EmitFill::Hatch {
                        spacing,
                        angle_deg,
                        line_width,
                        colour,
                    }) => {
                        let _ = writeln!(
                            out,
                            "    fill: {{ kind: hatch, spacing: {spacing}, angle_deg: {angle_deg}, line_width: {line_width}, colour: {} }}",
                            quote_colour(*colour)
                        );
                    }
                    Some(EmitFill::Image { name }) => {
                        let _ = writeln!(out, "    fill: {{ kind: image, name: {} }}", yaml_quote(name));
                    }
                    None => {}
                }
                if let Some(c) = st.stroke {
                    let _ = writeln!(out, "    stroke: {}", quote_colour(c));
                }
                if let Some(v) = st.stroke_width {
                    let _ = writeln!(out, "    stroke_width: {v}");
                }
                if let Some(ref arr) = st.stroke_dasharray {
                    let _ = writeln!(
                        out,
                        "    stroke_dasharray: [{}]",
                        arr.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(", ")
                    );
                }
                if let Some(lj) = st.stroke_linejoin {
                    let _ = writeln!(out, "    stroke_linejoin: {lj}");
                }
                if let Some(o) = st.opacity {
                    let _ = writeln!(out, "    opacity: {o}");
                }
                if let Some(off) = st.stroke_offset_px {
                    let _ = writeln!(out, "    stroke_offset_px: {off}");
                }
                if let Some(g) = st.stroke_gap {
                    let _ = writeln!(
                        out,
                        "    stroke_gap: {{ interval_px: {}, initial_px: {} }}",
                        g.interval_px, g.initial_px
                    );
                }
                if let Some(gt) = st.geom_transform {
                    let _ = writeln!(out, "    geom_transform: {gt}");
                }
                if let Some(m) = st.marker.as_ref() {
                    write_marker(&mut out, m);
                }
            }
        }
        let _ = writeln!(out);
    }

    // layers
    let windows = band_windows(bands);
    let _ = writeln!(out, "layers:");
    if skel.layers.is_empty() {
        let _ = writeln!(out, "  []");
    } else {
        for layer in &skel.layers {
            let _ = writeln!(out, "  - name: {}", yaml_quote(&layer.name));
            if let Some(title) = &layer.title {
                let _ = writeln!(out, "    title: {}", yaml_quote(title));
            }
            if let Some(abs) = &layer.abstract_ {
                let _ = writeln!(out, "    abstract: {}", yaml_quote(abs));
            }
            if let Some(kind) = &layer.geom_kind {
                let _ = writeln!(out, "    type: {kind}");
            }
            if let Some(g) = &layer.group {
                let _ = writeln!(out, "    group: {}", yaml_quote(g));
            }
            // explicit GFI opt-in derived from request_gating; the layer block
            // emits `enable_get_feature_info: true` whenever the request_gating
            // explicitly permits the op (back-compat with the binary flag).
            if let Some(true) = layer.wms.request_gating.get_feature_info {
                let _ = writeln!(out, "    enable_get_feature_info: true");
            }
            write_layer_wms_metadata(&mut out, &layer.wms);

            let band_tiers = split_layer_into_bands(layer, &windows);
            if !band_tiers.is_empty() {
                let _ = writeln!(out, "    sources:");
                for (band_name, tiers) in &band_tiers {
                    for tier in tiers {
                        let src = tier.src;
                        let mut parts: Vec<String> = Vec::new();
                        // emit `source: pg|ogr` only in plural mode so the
                        // 17 back-compat fixtures stay byte-stable.
                        if has_vectorfile {
                            let id = match &src.source {
                                BindingSource::VectorFile(_) => "ogr",
                                _ => "pg",
                            };
                            parts.push(format!("source: {id}"));
                        }
                        parts.push(format!("band: {band_name}"));
                        match &src.source {
                            BindingSource::Table(t) => {
                                parts.push(format!("from: {}", yaml_quote(t)));
                                parts.push(format!("geometry_column: {}", yaml_quote(&src.geometry_column)));
                            }
                            BindingSource::Sql(s) => {
                                parts.push(format!("sql: {}", yaml_quote(s)));
                                parts.push(format!("geometry_column: {}", yaml_quote(&src.geometry_column)));
                            }
                            BindingSource::VectorFile(vf) => {
                                parts.push(format!("uri: {}", yaml_quote(&vf.uri)));
                                parts.push(format!("format: {}", vf.format));
                                parts.push(format!("source_crs: {}", yaml_quote(&vf.source_crs)));
                            }
                        }
                        if let Some(ref id) = src.id_column {
                            parts.push(format!("id_column: {}", yaml_quote(id)));
                        }
                        if let Some(d) = tier.max_denom {
                            parts.push(format!("max_denom_exclusive: {d}"));
                        }
                        if let Some(f) = &src.filter {
                            parts.push(format!("filter: {}", yaml_quote(f)));
                        }
                        if !src.attributes.is_empty() {
                            let attrs = src
                                .attributes
                                .iter()
                                .map(|a| yaml_quote(a))
                                .collect::<Vec<_>>()
                                .join(", ");
                            parts.push(format!("attributes: [{attrs}]"));
                        }
                        let _ = writeln!(out, "      - {{ {} }}", parts.join(", "));
                    }
                }
            }

            if !layer.classes.is_empty() {
                let _ = writeln!(out, "    classes:");
                for cls in &layer.classes {
                    write_class(&mut out, cls);
                }
            }

            if let Some(ref lbl) = layer.label {
                let _ = writeln!(out, "    label:");
                write_label_body(&mut out, lbl, "      ");
            }
        }
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "observability:");
    let _ = writeln!(out, "  log_level: info");
    let _ = writeln!(out, "  log_format: text");

    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn src(max: Option<u64>, from: &str) -> SourceSkeleton {
        SourceSkeleton {
            max_denom_exclusive: max,
            source: BindingSource::Table(from.into()),
            filter: None,
            geometry_column: "g".into(),
            id_column: None,
            attributes: vec![],
        }
    }

    fn ladder() -> Vec<(String, u64)> {
        vec![
            ("detail".into(), 2_500),
            ("hi".into(), 12_500),
            ("mid".into(), 50_000),
            ("lo".into(), 250_000),
            ("overview".into(), u64::MAX),
        ]
    }

    #[test]
    fn single_open_source_emits_one_tier_per_band() {
        let layer = LayerSkeleton {
            name: "all".into(),
            sources: vec![src(None, "t")],
            ..Default::default()
        };
        let bands = ladder();
        let windows = band_windows(&bands);
        let out = split_layer_into_bands(&layer, &windows);
        assert_eq!(out.len(), 5);
        for (_, tiers) in &out {
            assert_eq!(tiers.len(), 1);
            assert!(tiers[0].max_denom.is_none(), "single-tier band should omit max");
        }
    }

    #[test]
    fn scaletoken_tiers_split_within_a_band() {
        // SCALETOKEN: [0, 1000) -> t0, [1000, MAX) -> t1.
        let layer = LayerSkeleton {
            name: "buildings".into(),
            sources: vec![src(Some(1_000), "t0"), src(None, "t1")],
            ..Default::default()
        };
        let bands = ladder();
        let windows = band_windows(&bands);
        let out = split_layer_into_bands(&layer, &windows);
        let detail = out.iter().find(|(n, _)| *n == "detail").expect("detail band");
        assert_eq!(detail.1.len(), 2);
        assert_eq!(detail.1[0].max_denom, Some(1_000));
        assert_eq!(detail.1[0].src.source_table(), "t0");
        assert!(detail.1[1].max_denom.is_none());
        assert_eq!(detail.1[1].src.source_table(), "t1");
        // every other band has only t1, single-tier, no max.
        for (name, tiers) in &out {
            if *name == "detail" {
                continue;
            }
            assert_eq!(tiers.len(), 1);
            assert_eq!(tiers[0].src.source_table(), "t1");
            assert!(tiers[0].max_denom.is_none());
        }
    }

    #[test]
    fn partial_band_coverage_is_dropped() {
        // layer caps at 25000 - covers detail and hi fully, mid only partially.
        let layer = LayerSkeleton {
            name: "x".into(),
            sources: vec![src(Some(25_000), "t")],
            ..Default::default()
        };
        let bands = ladder();
        let windows = band_windows(&bands);
        let out = split_layer_into_bands(&layer, &windows);
        let names: Vec<&str> = out.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, vec!["detail", "hi"]);
    }
}
