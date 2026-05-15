//! WMS GetCapabilities document builders.
//!
//! Per-version emit lives in [`v130`] and [`v111`]; this module dispatches
//! on the negotiated [`WmsVersion`] and owns the version-agnostic helpers
//! (bbox derivation, format resolution, XML primitives) shared by both
//! emitters.
//!
//! Both versions render a minimal, valid capabilities body; full format
//! conformance (DCPType / OnlineResource trees) is a future concern. The
//! HTTP edge caches one document per version and swaps both atomically
//! whenever the manifest changes.

mod v111;
mod v130;

use std::collections::HashMap;

use mars_config::Config;
use mars_types::{Bbox, ImageFormat, LayerId, Manifest};
use quick_xml::Writer;
use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};

use crate::{WmsError, WmsVersion};

/// Render the GetCapabilities XML for the negotiated version.
pub fn capabilities_xml(cfg: &Config, manifest: &Manifest, version: WmsVersion) -> Result<String, WmsError> {
    match version {
        WmsVersion::V111 => v111::capabilities_xml(cfg, manifest),
        WmsVersion::V130 => v130::capabilities_xml(cfg, manifest),
    }
}

/// INFO_FORMAT MIME strings advertised in the GetFeatureInfo capability
/// block. Mirrors the set [`crate::feature_info::info_format_mime`] accepts
/// on the request path so capabilities and runtime stay in agreement.
pub(super) const INFO_FORMATS: &[&str] = &["text/plain", "text/html", "application/json"];

/// Per-layer bbox derived from config. Manifest-side cell unions return
/// once the v3 page entries surface per-binding bboxes the wms builder
/// can union by binding-to-layer mapping.
pub(super) fn derive_layer_bboxes(cfg: &Config, _manifest: &Manifest) -> HashMap<LayerId, Bbox> {
    let mut out: HashMap<LayerId, Bbox> = HashMap::new();
    for layer in &cfg.layers {
        if let Some(bbox) = layer.bbox {
            out.entry(layer.name.clone()).or_insert(bbox);
        }
    }
    out
}

pub(super) fn union_bbox(a: Bbox, b: Bbox) -> Bbox {
    Bbox::new(
        a.min_x.min(b.min_x),
        a.min_y.min(b.min_y),
        a.max_x.max(b.max_x),
        a.max_y.max(b.max_y),
    )
}

/// Resolve the format set the runtime advertises. Falls back to PNG when
/// `interfaces.wms.formats` is omitted, matching `WmsConfig::from_config`.
pub(super) fn configured_formats(cfg: &Config) -> Vec<ImageFormat> {
    let configured: Vec<ImageFormat> = cfg
        .interfaces
        .wms
        .as_ref()
        .map(|w| {
            w.formats
                .iter()
                .filter_map(|f| ImageFormat::from_mime(f.as_str()))
                .collect()
        })
        .unwrap_or_default();
    if configured.is_empty() {
        vec![ImageFormat::Png]
    } else {
        configured
    }
}

pub(super) fn text_element<W: std::io::Write>(w: &mut Writer<W>, name: &str, text: &str) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new(name))).map_err(xml_err)?;
    w.write_event(Event::Text(BytesText::new(text))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new(name))).map_err(xml_err)?;
    Ok(())
}

pub(super) fn xml_err(e: std::io::Error) -> WmsError {
    WmsError::InvalidParam {
        name: "capabilities",
        reason: e.to_string(),
    }
}

/// Node in the WMS GetCapabilities layer tree. Interior nodes are
/// synthesised from `Layer.group` path segments and carry only a `title`
/// from the segment string; leaf nodes wrap an actual configured layer
/// and emit the full per-layer block (Name, Title, BoundingBox, Style).
///
/// Built with a stable child ordering: synthesised group children first
/// (alphabetical by segment), real layer children second (config order).
/// This keeps GetCapabilities output deterministic across config reloads.
pub(super) struct LayerNode<'a> {
    pub title: String,
    pub leaf: Option<&'a mars_config::Layer>,
    pub group_children: std::collections::BTreeMap<String, LayerNode<'a>>,
    pub layer_children: Vec<LayerNode<'a>>,
}

impl<'a> LayerNode<'a> {
    fn new_group(title: String) -> Self {
        Self {
            title,
            leaf: None,
            group_children: Default::default(),
            layer_children: Vec::new(),
        }
    }

    fn new_leaf(layer: &'a mars_config::Layer) -> Self {
        Self {
            title: String::new(),
            leaf: Some(layer),
            group_children: Default::default(),
            layer_children: Vec::new(),
        }
    }
}

/// Split a `Layer.group` value into normalised path segments.
/// Empty / whitespace segments are dropped; leading and trailing slashes
/// are tolerated. Used to bucket layers into the capabilities tree.
fn split_group_path(path: &str) -> Vec<&str> {
    path.split('/').map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// Build a [`LayerNode`] tree where each [`Layer.group`] path inserts the
/// layer as a leaf at the resolved depth. Layers without a group hang off
/// the root.
pub(super) fn build_layer_tree<'a>(layers: &'a [mars_config::Layer]) -> LayerNode<'a> {
    let mut root = LayerNode::new_group(String::new());
    for layer in layers {
        let segments: Vec<&str> = layer.group.as_deref().map(split_group_path).unwrap_or_default();
        let mut cursor = &mut root;
        for seg in &segments {
            cursor = cursor
                .group_children
                .entry((*seg).to_string())
                .or_insert_with(|| LayerNode::new_group((*seg).to_string()));
        }
        cursor.layer_children.push(LayerNode::new_leaf(layer));
    }
    root
}
