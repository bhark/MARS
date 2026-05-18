//! per-layer WMS / OWS metadata YAML writers.

use std::fmt::Write as _;

use super::skeleton::{IncludeItemsSkeleton, LayerOwsSkeleton, LayerWmsSkeleton};
use super::yaml::yaml_quote;

/// Emit per-layer WMS-only extras under a `wms:` block. Indent assumes a
/// `layers:` list item ("  - ...") - the `wms:` key starts at column 4 to
/// align with siblings like `title:` and `group:`. Fields nest at column 6.
pub(super) fn write_layer_wms_metadata(out: &mut String, wms: &LayerWmsSkeleton) {
    if wms.is_empty() {
        return;
    }
    let _ = writeln!(out, "    wms:");
    if let Some(o) = wms.opaque {
        let _ = writeln!(out, "      opaque: {o}");
    }
    if !wms.advertised_crs.is_empty() {
        let _ = writeln!(out, "      advertised_crs:");
        for crs in &wms.advertised_crs {
            let _ = writeln!(out, "        - {}", yaml_quote(crs));
        }
    }
}

/// Emit per-layer OWS metadata + cross-protocol gating under an `ows:`
/// block. Same indent rules as the `wms:` emitter.
pub(super) fn write_layer_ows_metadata(out: &mut String, ows: &LayerOwsSkeleton) {
    if ows.is_empty() {
        return;
    }
    let _ = writeln!(out, "    ows:");
    if !ows.keywords.is_empty() {
        let _ = writeln!(out, "      keywords:");
        for kw in &ows.keywords {
            let _ = writeln!(out, "        - {}", yaml_quote(kw));
        }
    }
    if !ows.metadata_urls.is_empty() {
        let _ = writeln!(out, "      metadata_urls:");
        for (t, f, h) in &ows.metadata_urls {
            let _ = writeln!(out, "        - type: {}", yaml_quote(t));
            let _ = writeln!(out, "          format: {}", yaml_quote(f));
            let _ = writeln!(out, "          href: {}", yaml_quote(h));
        }
    }
    if !ows.authorities.is_empty() {
        let _ = writeln!(out, "      authorities:");
        for (n, h) in &ows.authorities {
            let _ = writeln!(out, "        - name: {}", yaml_quote(n));
            let _ = writeln!(out, "          href: {}", yaml_quote(h));
        }
    }
    if !ows.identifiers.is_empty() {
        let _ = writeln!(out, "      identifiers:");
        for (a, v) in &ows.identifiers {
            let _ = writeln!(out, "        - authority: {}", yaml_quote(a));
            let _ = writeln!(out, "          value: {}", yaml_quote(v));
        }
    }
    if let Some(a) = &ows.attribution {
        let _ = writeln!(out, "      attribution:");
        if let Some(t) = &a.title {
            let _ = writeln!(out, "        title: {}", yaml_quote(t));
        }
        if let Some(or) = &a.online_resource {
            let _ = writeln!(out, "        online_resource: {}", yaml_quote(or));
        }
        if a.logo_format.is_some() || a.logo_href.is_some() || a.logo_width.is_some() || a.logo_height.is_some() {
            let _ = writeln!(out, "        logo:");
            if let Some(f) = &a.logo_format {
                let _ = writeln!(out, "          format: {}", yaml_quote(f));
            }
            if let Some(h) = &a.logo_href {
                let _ = writeln!(out, "          href: {}", yaml_quote(h));
            }
            if let Some(w) = a.logo_width {
                let _ = writeln!(out, "          width: {w}");
            }
            if let Some(h) = a.logo_height {
                let _ = writeln!(out, "          height: {h}");
            }
        }
    }
    if let Some(items) = &ows.include_items {
        match items {
            IncludeItemsSkeleton::All => {
                let _ = writeln!(out, "      include_items: {{ mode: all }}");
            }
            IncludeItemsSkeleton::None => {
                let _ = writeln!(out, "      include_items: {{ mode: none }}");
            }
            IncludeItemsSkeleton::Explicit(names) => {
                let _ = writeln!(out, "      include_items:");
                let _ = writeln!(out, "        mode: explicit");
                let _ = writeln!(out, "        names:");
                for n in names {
                    let _ = writeln!(out, "          - {}", yaml_quote(n));
                }
            }
        }
    }
    if !ows.request_gating.is_empty() {
        let _ = writeln!(out, "      request_gating:");
        let rg = &ows.request_gating;
        if let Some(b) = rg.get_capabilities {
            let _ = writeln!(out, "        wms_get_capabilities: {b}");
        }
        if let Some(b) = rg.get_map {
            let _ = writeln!(out, "        wms_get_map: {b}");
        }
        if let Some(b) = rg.get_feature_info {
            let _ = writeln!(out, "        wms_get_feature_info: {b}");
        }
        if let Some(b) = rg.get_legend_graphic {
            let _ = writeln!(out, "        wms_get_legend_graphic: {b}");
        }
        if let Some(b) = rg.get_styles {
            let _ = writeln!(out, "        wms_get_styles: {b}");
        }
        if let Some(b) = rg.describe_layer {
            let _ = writeln!(out, "        wms_describe_layer: {b}");
        }
        if let Some(b) = rg.wmts_get_capabilities {
            let _ = writeln!(out, "        wmts_get_capabilities: {b}");
        }
        if let Some(b) = rg.wmts_get_tile {
            let _ = writeln!(out, "        wmts_get_tile: {b}");
        }
        if let Some(b) = rg.wmts_get_feature_info {
            let _ = writeln!(out, "        wmts_get_feature_info: {b}");
        }
    }
}
