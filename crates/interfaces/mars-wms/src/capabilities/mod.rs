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

use mars_config::{Config, ContactInfo};
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

/// Emit a `<KeywordList>` block with one `<Keyword>` child per entry. No-op
/// when the slice is empty so callers can pass `&cfg.service.keywords`
/// unconditionally.
pub(super) fn write_keyword_list<W: std::io::Write>(w: &mut Writer<W>, keywords: &[String]) -> Result<(), WmsError> {
    if keywords.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("KeywordList")))
        .map_err(xml_err)?;
    for kw in keywords {
        text_element(w, "Keyword", kw)?;
    }
    w.write_event(Event::End(BytesEnd::new("KeywordList")))
        .map_err(xml_err)?;
    Ok(())
}

/// Emit an empty `<OnlineResource xlink:type="simple" xlink:href="..."/>`.
/// xmlns:xlink is declared per-element since the root WMS element does not
/// pre-declare it.
pub(super) fn write_online_resource<W: std::io::Write>(w: &mut Writer<W>, href: &str) -> Result<(), WmsError> {
    let mut or = BytesStart::new("OnlineResource");
    or.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    or.push_attribute(("xlink:type", "simple"));
    or.push_attribute(("xlink:href", href));
    w.write_event(Event::Empty(or)).map_err(xml_err)
}

/// Emit `<DCPType><HTTP><Get><OnlineResource .../></Get></HTTP></DCPType>`.
/// Both WMS 1.1.1 and 1.3.0 use the same DCPType shape - only the parent
/// operation element differs.
pub(super) fn write_dcp_type<W: std::io::Write>(w: &mut Writer<W>, href: &str) -> Result<(), WmsError> {
    w.write_event(Event::Start(BytesStart::new("DCPType")))
        .map_err(xml_err)?;
    w.write_event(Event::Start(BytesStart::new("HTTP"))).map_err(xml_err)?;
    w.write_event(Event::Start(BytesStart::new("Get"))).map_err(xml_err)?;
    write_online_resource(w, href)?;
    w.write_event(Event::End(BytesEnd::new("Get"))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("HTTP"))).map_err(xml_err)?;
    w.write_event(Event::End(BytesEnd::new("DCPType")))
        .map_err(xml_err)?;
    Ok(())
}

/// Emit `<ContactInformation>` with whichever sub-elements have content.
/// Returns early without writing anything when nothing is to be emitted, so
/// callers can call unconditionally. `fallback_email` is consulted when
/// `contact.email` is empty - this preserves the legacy top-level
/// `service.contact_email` shorthand.
pub(super) fn write_contact_information<W: std::io::Write>(
    w: &mut Writer<W>,
    contact: &ContactInfo,
    fallback_email: &str,
) -> Result<(), WmsError> {
    let email = if !contact.email.is_empty() {
        contact.email.as_str()
    } else {
        fallback_email
    };
    if contact.is_empty() && email.is_empty() {
        return Ok(());
    }
    w.write_event(Event::Start(BytesStart::new("ContactInformation")))
        .map_err(xml_err)?;
    if !contact.person.is_empty() || !contact.organization.is_empty() {
        w.write_event(Event::Start(BytesStart::new("ContactPersonPrimary")))
            .map_err(xml_err)?;
        if !contact.person.is_empty() {
            text_element(w, "ContactPerson", &contact.person)?;
        }
        if !contact.organization.is_empty() {
            text_element(w, "ContactOrganization", &contact.organization)?;
        }
        w.write_event(Event::End(BytesEnd::new("ContactPersonPrimary")))
            .map_err(xml_err)?;
    }
    if !contact.position.is_empty() {
        text_element(w, "ContactPosition", &contact.position)?;
    }
    if !contact.address.is_empty() {
        let a = &contact.address;
        w.write_event(Event::Start(BytesStart::new("ContactAddress")))
            .map_err(xml_err)?;
        // AddressType defaults to "postal" when omitted - MapServer's behavior
        // and the most common WMS deployment convention.
        let addr_type = if a.type_.is_empty() { "postal" } else { a.type_.as_str() };
        text_element(w, "AddressType", addr_type)?;
        if !a.street.is_empty() {
            text_element(w, "Address", &a.street)?;
        }
        if !a.city.is_empty() {
            text_element(w, "City", &a.city)?;
        }
        if !a.state_or_province.is_empty() {
            text_element(w, "StateOrProvince", &a.state_or_province)?;
        }
        if !a.postcode.is_empty() {
            text_element(w, "PostCode", &a.postcode)?;
        }
        if !a.country.is_empty() {
            text_element(w, "Country", &a.country)?;
        }
        w.write_event(Event::End(BytesEnd::new("ContactAddress")))
            .map_err(xml_err)?;
    }
    if !contact.phone.is_empty() {
        text_element(w, "ContactVoiceTelephone", &contact.phone)?;
    }
    if !contact.fax.is_empty() {
        text_element(w, "ContactFacsimileTelephone", &contact.fax)?;
    }
    if !email.is_empty() {
        text_element(w, "ContactElectronicMailAddress", email)?;
    }
    w.write_event(Event::End(BytesEnd::new("ContactInformation")))
        .map_err(xml_err)?;
    Ok(())
}

/// Resolve the per-operation advertised format list. When the service-level
/// override is set, return its strings verbatim. Otherwise fall back to the
/// renderable formats resolved by [`configured_formats`], mapped to their
/// MIME strings.
pub(super) fn resolved_request_formats(override_list: &[String], fallback: &[ImageFormat]) -> Vec<String> {
    if !override_list.is_empty() {
        override_list.to_vec()
    } else {
        fallback.iter().map(|f| f.mime().to_string()).collect()
    }
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
