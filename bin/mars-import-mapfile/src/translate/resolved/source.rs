//! ResolvedSource + the PostGIS / OGR resolution paths. PostGIS reads
//! `DATA "<col> FROM <table>"` plus SCALETOKEN and folds the layer FILTER
//! into per-tier predicates; OGR lifts the CONNECTION string into an
//! object-store URI and infers VectorFile format from the path extension.

use tracing::warn;

use crate::emitter::{BindingSource, VectorFileBinding};
use crate::expression::parse_mapfile_expression;

use super::super::layer::{guess_id_column, lift_inline_subquery, lifted_to_source, parse_data};

#[derive(Debug)]
pub(crate) struct ResolvedSource {
    pub source: BindingSource,
    pub filter: Option<String>,
    pub geometry_column: String,
    pub id_column: Option<String>,
    pub max_denom_exclusive: Option<u64>,
}

/// Lift a `CONNECTIONTYPE OGR` layer's CONNECTION + PROJECTION into a single
/// vectorfile binding. Failure cases (missing connection / unknown
/// /vsi-prefix / unknown extension / no source CRS) warn and return an
/// empty source list.
pub(super) fn resolve_ogr_source(
    layer_name: &str,
    layer_line: usize,
    connection: Option<&str>,
    layer_projection: Option<&str>,
    map_projection: Option<&str>,
    max_scale_denom: Option<u64>,
    processing_items: Option<&str>,
) -> Vec<ResolvedSource> {
    let Some(raw) = connection else {
        warn!(
            line = layer_line,
            layer = %layer_name,
            "OGR layer has no CONNECTION; skipping"
        );
        return Vec::new();
    };
    let Some(uri) = ogr_connection_to_uri(raw, layer_name, layer_line) else {
        return Vec::new();
    };
    let Some(format) = infer_vectorfile_format(&uri) else {
        warn!(
            line = layer_line,
            layer = %layer_name,
            uri = %uri,
            "OGR connection has unknown extension; cannot infer format - skipping"
        );
        return Vec::new();
    };
    let source_crs = match layer_projection.or(map_projection) {
        Some(s) => s.to_string(),
        None => {
            warn!(
                line = layer_line,
                layer = %layer_name,
                "OGR layer has no PROJECTION (and no MAP PROJECTION fallback); skipping"
            );
            return Vec::new();
        }
    };
    let id_col = processing_items.and_then(guess_id_column);
    vec![ResolvedSource {
        source: BindingSource::VectorFile(VectorFileBinding {
            uri,
            format,
            source_crs,
        }),
        filter: None,
        // vectorfile bindings ignore geometry_column at decode time, but the
        // emitter still writes it for shape parity. Keep it empty to make
        // intent explicit in the YAML.
        geometry_column: String::new(),
        id_column: id_col,
        max_denom_exclusive: max_scale_denom,
    }]
}

/// Translate a MapServer-OGR CONNECTION string into an object-store URI.
/// Returns `None` (after warning) for unrecognised `/vsi*` prefixes; bare
/// paths route to `file://` (canonicalised when relative).
fn ogr_connection_to_uri(raw: &str, layer_name: &str, layer_line: usize) -> Option<String> {
    let trimmed = raw.trim().trim_matches('"');
    if let Some(rest) = trimmed.strip_prefix("/vsis3/") {
        return Some(format!("s3://{rest}"));
    }
    if let Some(rest) = trimmed.strip_prefix("/vsigs/") {
        return Some(format!("gs://{rest}"));
    }
    if let Some(rest) = trimmed.strip_prefix("/vsicurl/") {
        return Some(rest.to_string());
    }
    if trimmed.starts_with("/vsi") {
        warn!(
            line = layer_line,
            layer = %layer_name,
            connection = %trimmed,
            "unsupported /vsi* prefix in OGR CONNECTION; skipping layer"
        );
        return None;
    }
    if trimmed.starts_with("s3://")
        || trimmed.starts_with("gs://")
        || trimmed.starts_with("file://")
        || trimmed.starts_with("http://")
        || trimmed.starts_with("https://")
    {
        return Some(trimmed.to_string());
    }
    // bare path: absolute -> file:///abs; relative -> canonicalise against cwd.
    let pathbuf = std::path::Path::new(trimmed);
    let abs = if pathbuf.is_absolute() {
        pathbuf.to_path_buf()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(pathbuf))
            .unwrap_or_else(|| pathbuf.to_path_buf())
    };
    Some(format!("file://{}", abs.display()))
}

/// Map a file extension on the URI's path to a `VectorFileFormat` wire spelling.
fn infer_vectorfile_format(uri: &str) -> Option<String> {
    // strip query/fragment before extension match
    let path = uri.split(['?', '#']).next().unwrap_or(uri);
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".fgb") {
        Some("flat_geobuf".to_string())
    } else if lower.ends_with(".geojson") || lower.ends_with(".json") {
        Some("geo_json".to_string())
    } else if lower.ends_with(".shp.zip") || lower.ends_with(".shz") {
        // shapefile triple is shipped as a single archive; .shz is the
        // less-common compact spelling.
        Some("shapefile".to_string())
    } else {
        None
    }
}

pub(super) fn resolve_sources(
    data: Option<&str>,
    scale_token_values: &[(u64, String)],
    max_scale_denom: Option<u64>,
    processing_items: Option<&str>,
    layer_filter: Option<&(String, usize)>,
) -> Vec<ResolvedSource> {
    let (geom_col, from_table) = parse_data(data);
    let id_col = processing_items.and_then(guess_id_column);

    // parse the layer-level FILTER body once. Ok = normalised DSL string;
    // Err = the predicate could not be translated; emit `false` so the
    // generated YAML still parses and the class matches no features. the
    // raw mapfile text is surfaced via the stderr `warn!` channel; `--strict`
    // exits 2 if any warnings fired.
    let layer_filter: Option<Result<String, String>> =
        layer_filter.map(|(raw, line)| match parse_mapfile_expression(raw, *line) {
            Ok(expr) => Ok(format!("{expr}")),
            Err(e) => {
                warn!(line = *line, raw = %raw, error = %e, "could not parse layer FILTER");
                Err("false".to_string())
            }
        });

    let combine = |inline: Option<String>| -> Option<String> {
        match (&layer_filter, inline) {
            (Some(Ok(layer)), Some(inline)) => Some(format!("({layer}) AND ({inline})")),
            (Some(Ok(layer)), None) => Some(layer.clone()),
            (Some(Err(todo)), _) => Some(todo.clone()),
            (None, inline) => inline,
        }
    };

    if !scale_token_values.is_empty() {
        let gc = geom_col.unwrap_or_else(|| "geometri".into());
        let n = scale_token_values.len();
        (0..n)
            .map(|idx| {
                let (_min, table) = &scale_token_values[idx];
                let max_denom = if idx + 1 < n {
                    Some(scale_token_values[idx + 1].0)
                } else {
                    max_scale_denom
                };
                let (source, inline_filter) = lifted_to_source(lift_inline_subquery(table));
                ResolvedSource {
                    source,
                    filter: combine(inline_filter),
                    geometry_column: gc.clone(),
                    id_column: id_col.clone(),
                    max_denom_exclusive: max_denom,
                }
            })
            .collect()
    } else if let Some(table) = from_table {
        let (source, inline_filter) = lifted_to_source(lift_inline_subquery(&table));
        vec![ResolvedSource {
            source,
            filter: combine(inline_filter),
            geometry_column: geom_col.unwrap_or_else(|| "geometri".into()),
            id_column: id_col,
            max_denom_exclusive: max_scale_denom,
        }]
    } else {
        Vec::new()
    }
}
