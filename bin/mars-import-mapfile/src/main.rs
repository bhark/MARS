//! mars-import-mapfile: opinionated translator from MapServer mapfile to MARS YAML.
//!
//! coverage is the subset exercised by the parity harness.
//! Synchronous; no tokio.

mod emitter;
mod expression;
mod scanner;

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use clap::Parser;
use tracing::warn;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, Layer};

use crate::emitter::{
    ClassSkeleton, LabelSkeleton, LayerSkeleton, Skeleton, SourceSkeleton, StyleDef, rgb_to_hex, slugify,
};
use crate::scanner::{Token, block_range, is_block_opener, scan, scan_file};

#[derive(Debug, Parser)]
#[command(
    name = "mars-import-mapfile",
    version,
    about = "Translate a MapServer mapfile to a MARS YAML config."
)]
struct Cli {
    /// path to the input mapfile.
    input: PathBuf,
    /// path to write the output YAML to (defaults to stdout).
    #[arg(long)]
    output: Option<PathBuf>,
    /// exit non-zero (code 2) if any warnings were emitted.
    #[arg(long)]
    strict: bool,
    /// include only layers whose NAME matches one of these values (repeatable).
    #[arg(long = "include-layer")]
    include_layers: Vec<String>,
    /// override the default scale-band ladder.
    /// format: `name:cap,name:cap,...` with strictly-increasing caps; the
    /// final cap may be `max` to mean unbounded. example:
    /// `detail:2500,hi:12500,mid:50000,lo:250000,overview:max`.
    #[arg(long = "bands", value_parser = parse_bands_arg)]
    bands: Option<Vec<(String, u64)>>,
}

/// parse the `--bands` CLI value into an ordered ladder.
fn parse_bands_arg(s: &str) -> Result<Vec<(String, u64)>, String> {
    let mut out = Vec::new();
    let mut prev: Option<u64> = None;
    for (idx, part) in s.split(',').enumerate() {
        let entry = part.trim();
        let Some((name, cap_str)) = entry.split_once(':') else {
            return Err(format!("band entry {idx} {entry:?} missing `:`"));
        };
        let name = name.trim();
        let cap_str = cap_str.trim();
        if name.is_empty() {
            return Err(format!("band entry {idx} has empty name"));
        }
        let cap: u64 = if cap_str.eq_ignore_ascii_case("max") {
            u64::MAX
        } else {
            cap_str
                .parse()
                .map_err(|e| format!("band entry {idx} cap {cap_str:?}: {e}"))?
        };
        if let Some(p) = prev
            && cap <= p
        {
            return Err(format!(
                "band caps must strictly increase; entry {idx} cap {cap} <= previous {p}"
            ));
        }
        prev = Some(cap);
        out.push((name.to_string(), cap));
    }
    if out.is_empty() {
        return Err("--bands must declare at least one band".into());
    }
    Ok(out)
}

/// keywords whose presence we don't translate yet. some are block openers,
/// some are scalar directives — `walk` handles both.
const UNSUPPORTED: &[&str] = &[
    "SYMBOL",
    "FONTSET",
    "LEGEND",
    "PROJECTION",
    "METADATA",
    "OUTPUTFORMAT",
    "FEATURE",
    "JOIN",
    "COMPOSITE",
    "CLUSTER",
    "GRID",
    "VALIDATION",
];

fn is_unsupported(kw: &str) -> bool {
    let up = kw.to_ascii_uppercase();
    UNSUPPORTED.iter().any(|b| *b == up)
}

struct WarnCounter {
    counter: Arc<AtomicUsize>,
}

impl<S> Layer<S> for WarnCounter
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.counter.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn install_tracing(counter: Arc<AtomicUsize>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_writer(io::stderr);
    let count_layer = WarnCounter { counter }.with_filter(filter);
    let _ = tracing_subscriber::registry()
        .with(fmt_layer)
        .with(count_layer)
        .try_init();
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let warn_count = Arc::new(AtomicUsize::new(0));
    install_tracing(warn_count.clone());

    let tokens = scan_file(&cli.input).with_context(|| format!("scanning {}", cli.input.display()))?;
    let include_layers: Option<std::collections::HashSet<String>> = if cli.include_layers.is_empty() {
        None
    } else {
        Some(cli.include_layers.into_iter().map(|s| s.to_lowercase()).collect())
    };
    let skeleton = translate_tokens(&tokens, include_layers.as_ref());
    let bands = cli.bands.unwrap_or_else(emitter::default_bands);
    let yaml = emitter::render(&skeleton, &bands);

    match &cli.output {
        Some(p) => fs::write(p, &yaml).with_context(|| format!("writing {}", p.display()))?,
        None => {
            let mut stdout = io::stdout().lock();
            stdout.write_all(yaml.as_bytes())?;
        }
    }

    if cli.strict && warn_count.load(Ordering::Relaxed) > 0 {
        std::process::exit(2);
    }
    Ok(())
}

/// translate a mapfile source into a YAML skeleton, warning on unsupported
/// constructs as a side-effect via `tracing::warn!`.
#[allow(dead_code)]
pub(crate) fn translate(src: &str) -> Skeleton {
    let tokens = scan(src);
    translate_tokens(&tokens, None)
}

fn translate_tokens(tokens: &[Token], include_layers: Option<&std::collections::HashSet<String>>) -> Skeleton {
    let mut skel = Skeleton::default();

    let map_slice: &[Token] = match tokens
        .iter()
        .position(|t| t.keyword.eq_ignore_ascii_case("MAP"))
        .and_then(|i| block_range(tokens, i))
    {
        Some(r) => &tokens[r.start + 1..r.end.saturating_sub(1).max(r.start + 1)],
        None => tokens,
    };

    walk(map_slice, &mut skel, include_layers);
    skel
}

fn walk(tokens: &[Token], skel: &mut Skeleton, include_layers: Option<&std::collections::HashSet<String>>) {
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        let kw = t.keyword.to_ascii_uppercase();

        if kw == "NAME" && skel.service_name.is_none() {
            if let Some(v) = t.args.first() {
                skel.service_name = Some(v.clone());
            }
            i += 1;
            continue;
        }
        if kw == "TITLE" && skel.service_title.is_none() {
            if let Some(v) = t.args.first() {
                skel.service_title = Some(v.clone());
            }
            i += 1;
            continue;
        }

        if kw == "LAYER" {
            let range = block_range(tokens, i).unwrap_or(i..i + 1);
            let body: &[Token] = if range.end > range.start + 1 {
                &tokens[range.start + 1..range.end - 1]
            } else {
                &[]
            };
            handle_layer(body, t.line, skel, include_layers);
            i = range.end;
            continue;
        }

        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "unsupported mapfile construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(tokens, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }
}

fn handle_layer(
    body: &[Token],
    layer_line: usize,
    skel: &mut Skeleton,
    include_layers: Option<&std::collections::HashSet<String>>,
) {
    let mut name: Option<String> = None;
    let mut title: Option<String> = None;
    let mut layer_type: Option<String> = None;
    let mut data: Option<String> = None;
    let mut _min_scale_denom: Option<u64> = None;
    let mut max_scale_denom: Option<u64> = None;
    let mut scale_token_values: Vec<(u64, String)> = Vec::new();
    let mut processing_items: Option<String> = None;
    let mut classes: Vec<ClassSkeleton> = Vec::new();
    let mut label: Option<LabelSkeleton> = None;

    // peek name first for filtering
    for t in body {
        if t.keyword.eq_ignore_ascii_case("NAME") {
            if let Some(n) = t.args.first() {
                name = Some(n.clone());
            }
            break;
        }
    }

    if let Some(set) = include_layers {
        let keep = name.as_ref().is_some_and(|n| set.contains(&n.to_lowercase()));
        if !keep {
            return;
        }
    }

    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "NAME" if name.is_none() => {
                name = t.args.first().cloned();
                i += 1;
                continue;
            }
            "TITLE" if title.is_none() => {
                title = t.args.first().cloned();
                i += 1;
                continue;
            }
            "TYPE" if layer_type.is_none() => {
                layer_type = t.args.first().cloned();
                i += 1;
                continue;
            }
            "DATA" if data.is_none() => {
                data = Some(t.args.join(" "));
                i += 1;
                continue;
            }
            "MINSCALEDENOM" | "MAXSCALEDENOM" => {
                if let Some(arg) = t.args.first() {
                    match arg.parse::<f64>() {
                        Ok(v) if v.is_finite() && v >= 0.0 => {
                            let n = normalize_n_plus_one(v as u64);
                            if kw == "MINSCALEDENOM" {
                                _min_scale_denom = Some(n);
                            } else {
                                max_scale_denom = Some(n);
                            }
                        }
                        _ => warn!(line = t.line, keyword = %kw, value = %arg, "could not parse scale denom"),
                    }
                }
                i += 1;
                continue;
            }
            "PROCESSING" => {
                if let Some(arg) = t.args.first() {
                    let up = arg.to_ascii_uppercase();
                    if let Some(rest) = up.strip_prefix("ITEMS=") {
                        processing_items = Some(rest.to_string());
                    }
                }
                i += 1;
                continue;
            }
            "SCALETOKEN" => {
                if let Some(r) = block_range(body, i) {
                    let st_body = &body[r.start + 1..r.end - 1];
                    let mut j = 0;
                    while j < st_body.len() {
                        let st_t = &st_body[j];
                        if st_t.keyword.eq_ignore_ascii_case("VALUES")
                            && let Some(vr) = block_range(st_body, j)
                        {
                            scale_token_values = parse_scale_token(&st_body[vr.start + 1..vr.end - 1]);
                            j = vr.end;
                            continue;
                        }
                        j += 1;
                    }
                    i = r.end;
                    continue;
                }
            }
            "CLASS" => {
                if let Some(r) = block_range(body, i) {
                    let resolved_name = name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));
                    let geom = layer_type.as_deref().and_then(mapfile_type_to_geom);
                    if let Some(cls) = parse_class(
                        &body[r.start + 1..r.end - 1],
                        t.line,
                        &resolved_name,
                        geom.unwrap_or("polygon"),
                        skel,
                    ) {
                        classes.push(cls);
                    }
                    i = r.end;
                    continue;
                }
            }
            "LABEL" => {
                if let Some(r) = block_range(body, i) {
                    let resolved_name = name.clone().unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));
                    if let Some(lbl) = parse_label(&body[r.start + 1..r.end - 1], t.line, &resolved_name, skel) {
                        label = Some(lbl);
                    }
                    i = r.end;
                    continue;
                }
            }
            _ => {}
        }

        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "unsupported mapfile construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(body, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }

    let resolved_name = name.unwrap_or_else(|| format!("unnamed_layer_l{layer_line}"));

    if let Some(ref t) = layer_type {
        let up = t.to_ascii_uppercase();
        if up == "RASTER" || up == "QUERY" {
            warn!(line = layer_line, layer = %resolved_name, "skipping RASTER/QUERY layer");
            return;
        }
    }

    let geom_kind = layer_type
        .as_ref()
        .and_then(|t| mapfile_type_to_geom(t).map(|s| s.to_string()));

    let (geometry_column, from_table) = parse_data(data.as_deref());

    let mut sources = Vec::new();
    if !scale_token_values.is_empty() {
        let gc = geometry_column.clone().unwrap_or_else(|| "geometri".into());
        let id_col = processing_items.as_deref().and_then(guess_id_column);
        let n = scale_token_values.len();
        for (idx, (_min_denom, table)) in scale_token_values.iter().enumerate() {
            let max_denom = if idx + 1 < n {
                Some(scale_token_values[idx + 1].0)
            } else {
                max_scale_denom
            };
            sources.push(SourceSkeleton {
                max_denom_exclusive: max_denom,
                from: table.clone(),
                geometry_column: gc.clone(),
                id_column: id_col.clone(),
                attributes: Vec::new(),
            });
        }
    } else if let Some(table) = from_table {
        sources.push(SourceSkeleton {
            max_denom_exclusive: max_scale_denom,
            from: table,
            geometry_column: geometry_column.unwrap_or_else(|| "geometri".into()),
            id_column: processing_items.as_deref().and_then(guess_id_column),
            attributes: Vec::new(),
        });
    }

    // collect attributes from class expressions
    let mut all_attrs = BTreeSet::new();
    for cls in &classes {
        if let Some(ref when) = cls.when
            && let Ok(expr) = mars_expr::parse(when)
        {
            mars_expr::collect_idents(&expr, &mut all_attrs);
        }
    }
    let attrs_vec: Vec<String> = all_attrs.into_iter().collect();
    for src in &mut sources {
        src.attributes = attrs_vec.clone();
    }

    skel.layers.push(LayerSkeleton {
        name: resolved_name,
        title,
        geom_kind,
        sources,
        classes,
        label,
    });
}

fn mapfile_type_to_geom(t: &str) -> Option<&str> {
    match t.to_ascii_uppercase().as_str() {
        "POINT" => Some("point"),
        "LINE" | "POLYLINE" => Some("line"),
        "POLYGON" => Some("polygon"),
        _ => None,
    }
}

/// strip a trailing ` USING ...` clause from a mapfile DATA / SCALETOKEN value.
fn strip_using(s: &str) -> &str {
    let upper = s.to_ascii_uppercase();
    if let Some(pos) = upper.find(" USING ") {
        &s[..pos]
    } else {
        s
    }
}

fn parse_data(data: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(d) = data else { return (None, None) };
    let cleaned = strip_using(d);
    let cleaned = cleaned.trim().trim_matches('"');
    let cleaned_upper = cleaned.to_ascii_uppercase();
    if let Some(pos) = cleaned_upper.find(" FROM ") {
        let geom = cleaned[..pos].trim().to_string();
        let table = cleaned[pos + 6..].trim().to_string();
        (Some(geom), Some(table))
    } else {
        (None, Some(cleaned.to_string()))
    }
}

fn guess_id_column(items: &str) -> Option<String> {
    let parts: Vec<&str> = items.split(',').map(|s| s.trim()).collect();
    parts
        .iter()
        .find(|s| s.eq_ignore_ascii_case("ogc_fid"))
        .copied()
        .or_else(|| parts.iter().find(|s| s.eq_ignore_ascii_case("id")).copied())
        .or_else(|| parts.iter().find(|s| s.to_ascii_lowercase().ends_with("_fid")).copied())
        .map(|s| s.to_string())
}

fn parse_scale_token(body: &[Token]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for t in body {
        if t.keyword.eq_ignore_ascii_case("END") {
            break;
        }
        let raw = match t.keyword.parse::<f64>() {
            Ok(v) if v.is_finite() && v >= 0.0 => v as u64,
            _ => continue,
        };
        let min = normalize_n_plus_one(raw);
        if let Some(table) = t.args.first() {
            let cleaned = strip_using(table).trim().trim_matches('"').to_string();
            if !cleaned.is_empty() {
                out.push((min, cleaned));
            }
        }
    }
    out
}

/// canonicalize MapServer's `MINSCALEDENOM = N+1` half-open convention.
/// when `n - 1` lands cleanly on a "round" base (10000, 5000, 1000, 500, 100),
/// snap down. conservative — values not on a round base are left alone.
fn normalize_n_plus_one(n: u64) -> u64 {
    if n <= 1 {
        return n;
    }
    const BASES: &[u64] = &[10_000, 5_000, 1_000, 500, 100];
    for &base in BASES {
        if (n - 1) >= base && (n - 1).is_multiple_of(base) {
            return n - 1;
        }
    }
    n
}

#[derive(Debug, Default)]
struct StyleBlock {
    color: Option<(u8, u8, u8)>,
    outlinecolor: Option<(u8, u8, u8)>,
    width: Option<f32>,
    outlinewidth: Option<f32>,
    pattern: Option<Vec<f32>>,
}

fn parse_class(
    body: &[Token],
    class_line: usize,
    layer_name: &str,
    geom_kind: &str,
    skel: &mut Skeleton,
) -> Option<ClassSkeleton> {
    let mut name: Option<String> = None;
    let mut expression: Option<String> = None;
    let mut styles: Vec<StyleBlock> = Vec::new();

    let mut i = 0;
    while i < body.len() {
        let t = &body[i];
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "NAME" if name.is_none() => {
                name = t.args.first().cloned();
                i += 1;
                continue;
            }
            "EXPRESSION" => {
                let joined = t.args.join(" ");
                match expression::parse_mapfile_expression(&joined, t.line) {
                    Ok(expr) => {
                        expression = Some(format!("{expr}"));
                    }
                    Err(e) => {
                        warn!(line = t.line, error = %e, "could not parse EXPRESSION");
                        expression = Some(format!("# TODO: hand-translate: {joined}"));
                    }
                }
                i += 1;
                continue;
            }
            "STYLE" => {
                if let Some(r) = block_range(body, i) {
                    styles.push(parse_style_block(&body[r.start + 1..r.end - 1]));
                    i = r.end;
                    continue;
                }
            }
            _ => {}
        }
        if is_unsupported(&kw) {
            warn!(line = t.line, keyword = %kw, "unsupported class-level construct");
            if is_block_opener(&kw)
                && let Some(r) = block_range(body, i)
            {
                i = r.end;
                continue;
            }
        }
        i += 1;
    }

    let title = name.clone();
    let class_name = slugify(&name.unwrap_or_else(|| format!("class_l{class_line}")));
    let style_prefix = if geom_kind == "polygon" { "poly" } else { geom_kind };
    let style_name = format!("{}_{}_{}", style_prefix, slugify(layer_name), class_name);

    let (fill, stroke, stroke_width, dasharray) = collapse_styles(&styles, class_line);

    // dedupe identical styles
    let canonical = {
        let mut tmp = String::new();
        let _ = std::fmt::write(&mut tmp, format_args!("kind={geom_kind}"));
        if let Some(ref v) = fill {
            let _ = std::fmt::write(&mut tmp, format_args!(",fill={v}"));
        }
        if let Some(ref v) = stroke {
            let _ = std::fmt::write(&mut tmp, format_args!(",stroke={v}"));
        }
        if let Some(v) = stroke_width {
            let _ = std::fmt::write(&mut tmp, format_args!(",width={v}"));
        }
        if let Some(ref arr) = dasharray {
            let _ = std::fmt::write(&mut tmp, format_args!(",dash={arr:?}"));
        }
        tmp
    };

    let existing = skel.styles.iter().find(|s| {
        let mut tmp = String::new();
        let _ = std::fmt::write(&mut tmp, format_args!("kind={}", s.style_type));
        if let Some(ref v) = s.fill {
            let _ = std::fmt::write(&mut tmp, format_args!(",fill={v}"));
        }
        if let Some(ref v) = s.stroke {
            let _ = std::fmt::write(&mut tmp, format_args!(",stroke={v}"));
        }
        if let Some(v) = s.stroke_width {
            let _ = std::fmt::write(&mut tmp, format_args!(",width={v}"));
        }
        if let Some(ref arr) = s.stroke_dasharray {
            let _ = std::fmt::write(&mut tmp, format_args!(",dash={arr:?}"));
        }
        tmp == canonical
    });

    let style_ref = if let Some(st) = existing {
        st.name.clone()
    } else {
        skel.styles.push(StyleDef {
            name: style_name.clone(),
            style_type: geom_kind.to_string(),
            fill,
            stroke,
            stroke_width,
            stroke_dasharray: dasharray,
            font_family: None,
            font_size: None,
            halo_color: None,
            halo_width: None,
        });
        style_name
    };

    Some(ClassSkeleton {
        name: class_name,
        title,
        when: expression,
        style_ref,
    })
}

fn parse_style_block(body: &[Token]) -> StyleBlock {
    let mut st = StyleBlock::default();
    for t in body {
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "COLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    st.color = Some((r, g, b));
                }
            }
            "OUTLINECOLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    st.outlinecolor = Some((r, g, b));
                }
            }
            "WIDTH" => {
                if let Ok(v) = t.args.first().unwrap_or(&String::new()).parse::<f32>() {
                    st.width = Some(v);
                }
            }
            "OUTLINEWIDTH" => {
                if let Ok(v) = t.args.first().unwrap_or(&String::new()).parse::<f32>() {
                    st.outlinewidth = Some(v);
                }
            }
            "PATTERN" => {
                let nums: Vec<f32> = t.args.iter().filter_map(|a| a.parse().ok()).collect();
                if !nums.is_empty() {
                    st.pattern = Some(nums);
                }
            }
            _ => {}
        }
    }
    st
}

fn collapse_styles(
    styles: &[StyleBlock],
    line: usize,
) -> (Option<String>, Option<String>, Option<f32>, Option<Vec<f32>>) {
    if styles.len() > 1 {
        warn!(
            line = line,
            count = styles.len(),
            "STYLE: collapsed multi-pass stack to single fill+stroke"
        );
    }
    let fill = styles
        .iter()
        .rev()
        .find_map(|s| s.color)
        .map(|(r, g, b)| rgb_to_hex(r, g, b));
    let stroke = styles
        .iter()
        .find_map(|s| s.outlinecolor)
        .map(|(r, g, b)| rgb_to_hex(r, g, b));
    let width = styles.iter().find_map(|s| s.width.or(s.outlinewidth));
    let dasharray = styles.iter().find_map(|s| s.pattern.clone());
    (fill, stroke, width, dasharray)
}

fn parse_label(body: &[Token], _line: usize, layer_name: &str, skel: &mut Skeleton) -> Option<LabelSkeleton> {
    let mut text: Option<String> = None;
    let mut font: Option<String> = None;
    let mut size: Option<f32> = None;
    let mut color: Option<(u8, u8, u8)> = None;
    let mut outlinecolor: Option<(u8, u8, u8)> = None;
    let mut outlinewidth: Option<f32> = None;

    for t in body {
        let kw = t.keyword.to_ascii_uppercase();
        match kw.as_str() {
            "TEXT" if text.is_none() => text = t.args.first().cloned(),
            "FONT" if font.is_none() => font = t.args.first().cloned(),
            "SIZE" => size = t.args.first().and_then(|a| a.parse().ok()),
            "COLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    color = Some((r, g, b));
                }
            }
            "OUTLINECOLOR" if t.args.len() >= 3 => {
                if let (Ok(r), Ok(g), Ok(b)) = (t.args[0].parse(), t.args[1].parse(), t.args[2].parse()) {
                    outlinecolor = Some((r, g, b));
                }
            }
            "OUTLINEWIDTH" => {
                outlinewidth = t.args.first().and_then(|a| a.parse().ok());
            }
            _ => {}
        }
    }

    let text = text?;
    let style_name = format!("label_{}", slugify(layer_name));
    let fill = color
        .map(|(r, g, b)| rgb_to_hex(r, g, b))
        .unwrap_or_else(|| "#000000".into());
    // label styles are not deduped against geometry styles
    skel.styles.push(StyleDef {
        name: style_name.clone(),
        style_type: "label".into(),
        fill: Some(fill),
        stroke: None,
        stroke_width: None,
        stroke_dasharray: None,
        font_family: font.or_else(|| Some("sans-serif".into())),
        font_size: size.or(Some(12.0)),
        halo_color: outlinecolor.map(|(r, g, b)| rgb_to_hex(r, g, b)),
        halo_width: outlinewidth,
    });

    Some(LabelSkeleton {
        text,
        style_ref: style_name,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn translate_extracts_name_title_and_layers() {
        let src = r#"
MAP
  NAME "demo"
  TITLE "Demo Service"
  LAYER
    NAME "roads"
    TYPE LINE
  END
  LAYER
    NAME "buildings"
    TYPE POLYGON
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("demo"));
        assert_eq!(skel.service_title.as_deref(), Some("Demo Service"));
        let names: Vec<&str> = skel.layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, vec!["roads", "buildings"]);
    }

    #[test]
    fn translate_extracts_classes_and_sources() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "roads"
    TYPE LINE
    DATA "geometri FROM roads_table"
    CLASS
      NAME "main"
      EXPRESSION ([type] = 'main')
      STYLE
        COLOR 190 190 190
        WIDTH 1.6
      END
    END
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        let layer = &skel.layers[0];
        assert_eq!(layer.name, "roads");
        assert_eq!(layer.geom_kind.as_deref(), Some("line"));
        assert_eq!(layer.sources.len(), 1);
        assert_eq!(layer.sources[0].from, "roads_table");
        assert_eq!(layer.sources[0].geometry_column, "geometri");
        assert_eq!(layer.classes.len(), 1);
        assert_eq!(layer.classes[0].name, "main");
        assert_eq!(layer.classes[0].when.as_deref(), Some("type = 'main'"));
        assert!(!skel.styles.is_empty());
    }

    #[test]
    fn translate_expands_scaletoken() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "buildings"
    TYPE POLYGON
    DATA "geometri FROM buildings_table"
    SCALETOKEN
      NAME "scale"
      VALUES
        "0" "buildings_0"
        "1000" "buildings_1"
      END
    END
    CLASS
      NAME "default"
      EXPRESSION ("1" = "1")
      STYLE
        COLOR 250 250 250
        OUTLINECOLOR 180 180 180
        WIDTH 0.6
      END
    END
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        let layer = &skel.layers[0];
        assert_eq!(layer.sources.len(), 2);
        assert_eq!(layer.sources[0].from, "buildings_0");
        assert_eq!(layer.sources[0].max_denom_exclusive, Some(1000));
        assert_eq!(layer.sources[1].from, "buildings_1");
        assert_eq!(layer.sources[1].max_denom_exclusive, None);
    }

    #[test]
    fn translate_skips_raster_layer() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "ortho"
    TYPE RASTER
  END
  LAYER
    NAME "roads"
    TYPE LINE
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.layers.len(), 1);
        assert_eq!(skel.layers[0].name, "roads");
    }

    #[test]
    fn unsupported_construct_does_not_break_translation() {
        let src = r#"
MAP
  NAME "x"
  SYMBOL
    NAME "dot"
    TYPE ELLIPSE
    POINTS 1 1 END
    FILLED TRUE
  END
  LAYER
    NAME "l1"
  END
END
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("x"));
        assert_eq!(skel.layers.len(), 1);
        assert_eq!(skel.layers[0].name, "l1");
    }

    #[test]
    fn normalize_n_plus_one_handles_round_bases() {
        assert_eq!(normalize_n_plus_one(0), 0);
        assert_eq!(normalize_n_plus_one(1), 1);
        assert_eq!(normalize_n_plus_one(101), 100);
        assert_eq!(normalize_n_plus_one(2_501), 2_500);
        assert_eq!(normalize_n_plus_one(25_001), 25_000);
        assert_eq!(normalize_n_plus_one(100_001), 100_000);
        // not on a round base — left alone.
        assert_eq!(normalize_n_plus_one(2_502), 2_502);
        assert_eq!(normalize_n_plus_one(123), 123);
    }

    #[test]
    fn parse_scale_token_normalizes_n_plus_one() {
        let src = r#"
MAP
  NAME "demo"
  LAYER
    NAME "buildings"
    TYPE POLYGON
    DATA "geometri FROM buildings_table"
    SCALETOKEN
      NAME "scale"
      VALUES
        "0" "buildings_0"
        "25001" "buildings_1"
      END
    END
  END
END
"#;
        let skel = translate(src);
        let layer = &skel.layers[0];
        assert_eq!(layer.sources[0].max_denom_exclusive, Some(25_000));
    }

    #[test]
    fn parse_bands_arg_validates_strict_increase() {
        assert!(parse_bands_arg("a:100,b:50").is_err());
        let ok = parse_bands_arg("a:100,b:200,c:max").unwrap();
        assert_eq!(
            ok,
            vec![("a".into(), 100u64), ("b".into(), 200u64), ("c".into(), u64::MAX),]
        );
    }

    #[test]
    fn comments_and_case_are_handled() {
        let src = r#"
map # top-level
  name "abc"   # service name
  layer
    name "only"
  end
end
"#;
        let skel = translate(src);
        assert_eq!(skel.service_name.as_deref(), Some("abc"));
        assert_eq!(skel.layers.len(), 1);
        assert_eq!(skel.layers[0].name, "only");
    }
}
