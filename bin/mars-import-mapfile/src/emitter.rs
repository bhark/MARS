//! one-pass YAML emitter for the mapfile translator.
//!
//! intentionally string-based: the result is meant to be hand-edited.

use std::collections::HashMap;
use std::fmt::Write as _;

use tracing::warn;

#[derive(Debug, Default)]
pub(crate) struct Skeleton {
    pub(crate) service_name: Option<String>,
    pub(crate) service_title: Option<String>,
    pub(crate) layers: Vec<LayerSkeleton>,
    pub(crate) styles: Vec<StyleDef>,
    /// mapfile-level SYMBOL definitions keyed by name. consumed by STYLE
    /// blocks via STYLE.SYMBOL "<name>"; not emitted into YAML directly -
    /// each STYLE that uses a symbol carries the resolved marker/fill on
    /// its `StyleDef`.
    pub(crate) symbols: HashMap<String, SymbolDef>,
}

#[derive(Debug, Clone)]
pub(crate) enum SymbolDef {
    /// MapServer SYMBOL TYPE ELLIPSE / VECTOR with a circular point list.
    Circle,
    /// SYMBOL TYPE HATCH. ANGLE and SIZE are symbol-level defaults; STYLE
    /// can override via ANGLE/SIZE/WIDTH/COLOR on the referencing STYLE.
    Hatch {
        angle_deg: Option<f32>,
        size: Option<f32>,
    },
    /// VECTOR with a named heuristic (square/triangle/cross/x/pin).
    NamedShape(String),
}

#[derive(Debug, Clone)]
pub(crate) struct StyleDef {
    pub(crate) name: String,
    pub(crate) style_type: String,
    pub(crate) fill: Option<EmitFill>,
    pub(crate) stroke: Option<String>,
    pub(crate) stroke_width: Option<f32>,
    pub(crate) stroke_dasharray: Option<Vec<f32>>,
    pub(crate) marker: Option<EmitMarker>,
    pub(crate) font_family: Option<String>,
    pub(crate) font_size: Option<f32>,
    pub(crate) halo_color: Option<String>,
    pub(crate) halo_width: Option<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum EmitFill {
    /// Bare hex string: emits as `fill: "#rrggbb"`.
    Hex(String),
    /// Tagged hatch map.
    Hatch {
        spacing: f32,
        angle_deg: f32,
        line_width: f32,
        colour: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EmitMarker {
    pub(crate) kind: &'static str,
    pub(crate) size: f32,
}

#[derive(Debug, Default)]
pub(crate) struct LayerSkeleton {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) geom_kind: Option<String>,
    pub(crate) sources: Vec<SourceSkeleton>,
    pub(crate) classes: Vec<ClassSkeleton>,
    pub(crate) label: Option<LabelSkeleton>,
}

#[derive(Debug, Clone)]
pub(crate) struct SourceSkeleton {
    pub(crate) max_denom_exclusive: Option<u64>,
    pub(crate) from: String,
    pub(crate) filter: Option<String>,
    pub(crate) geometry_column: String,
    pub(crate) id_column: Option<String>,
    pub(crate) attributes: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ClassSkeleton {
    pub(crate) name: String,
    pub(crate) title: Option<String>,
    pub(crate) when: Option<String>,
    pub(crate) min_scale_denom: Option<u64>,
    pub(crate) max_scale_denom: Option<u64>,
    pub(crate) style_ref: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LabelSkeleton {
    pub(crate) text: String,
    pub(crate) style_ref: String,
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

/// format a colour triple as #rrggbb.
pub(crate) fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
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
    let title = skel.service_title.as_deref().unwrap_or(name);

    let _ = writeln!(out, "service:");
    let _ = writeln!(out, "  name: {}", yaml_quote(name));
    let _ = writeln!(out, "  title: {}", yaml_quote(title));
    let _ = writeln!(out, "  abstract: \"Imported from mapfile\"");
    let _ = writeln!(out, "  contact_email: ops@example.org");
    let _ = writeln!(out);

    let _ = writeln!(out, "source:");
    let _ = writeln!(out, "  type: postgis");
    let _ = writeln!(out, "  dsn: \"${{PG_DSN}}\"");
    let _ = writeln!(out, "  native_crs: ${{MARS_NATIVE_CRS:-EPSG:25832}}");
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
                if let Some(EmitFill::Hex(ref v)) = st.fill {
                    let _ = writeln!(out, "    fill: {}", yaml_quote(v));
                }
                if let Some(ref c) = st.halo_color {
                    let w = st.halo_width.unwrap_or(1.0);
                    let _ = writeln!(out, "    halo: {{ color: {}, width: {w} }}", yaml_quote(c));
                }
            } else {
                match &st.fill {
                    Some(EmitFill::Hex(v)) => {
                        let _ = writeln!(out, "    fill: {}", yaml_quote(v));
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
                            yaml_quote(colour)
                        );
                    }
                    None => {}
                }
                if let Some(v) = &st.stroke {
                    let _ = writeln!(out, "    stroke: {}", yaml_quote(v));
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
                if let Some(ref m) = st.marker {
                    let _ = writeln!(out, "    marker: {{ kind: {}, size: {} }}", m.kind, m.size);
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
            if let Some(kind) = &layer.geom_kind {
                let _ = writeln!(out, "    type: {kind}");
            }

            let band_tiers = split_layer_into_bands(layer, &windows);
            if !band_tiers.is_empty() {
                let _ = writeln!(out, "    sources:");
                for (band_name, tiers) in &band_tiers {
                    for tier in tiers {
                        let src = tier.src;
                        let mut parts = vec![
                            format!("band: {band_name}"),
                            format!("from: {}", yaml_quote(&src.from)),
                            format!("geometry_column: {}", yaml_quote(&src.geometry_column)),
                        ];
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
                    let _ = writeln!(out, "      - {{ {} }}", parts.join(", "));
                }
            }

            if let Some(ref lbl) = layer.label {
                let _ = writeln!(out, "    label:");
                let _ = writeln!(out, "      text: {}", yaml_quote(&lbl.text));
                let _ = writeln!(
                    out,
                    "      style: {{ type: ref, name: {} }}",
                    yaml_quote(&lbl.style_ref)
                );
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
            from: from.into(),
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
        assert_eq!(detail.1[0].src.from, "t0");
        assert!(detail.1[1].max_denom.is_none());
        assert_eq!(detail.1[1].src.from, "t1");
        // every other band has only t1, single-tier, no max.
        for (name, tiers) in &out {
            if *name == "detail" {
                continue;
            }
            assert_eq!(tiers.len(), 1);
            assert_eq!(tiers[0].src.from, "t1");
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
