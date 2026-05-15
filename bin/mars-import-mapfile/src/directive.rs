//! per-block typed directive dispatch for the mapfile translator.
//!
//! Each per-block parser walks a `&[Token]` body slice. Pre-refactor the
//! dispatch was a string-keyed `match kw.as_str() { ... }` repeated in
//! every parser. This module collapses that into per-block enums plus
//! `from_token` constructors so each parser becomes a plain `match` on the
//! enum, and the keyword string match lives in exactly one place.

use crate::scanner::Token;

/// Top-level (MAP-body) directive.
#[derive(Debug)]
pub(crate) enum MapDirective<'a> {
    Name(&'a Token),
    Title(&'a Token),
    Layer(&'a Token),
    Symbol,
    /// `METADATA` block at MAP scope. Carries `ows_*` / `wms_*` keys that
    /// drive WMS / WMTS capabilities metadata (online resource, contact,
    /// keywords, fees, authority refs, etc.).
    Metadata,
    /// Keyword present in the `UNSUPPORTED` list - the parser warns at use
    /// site and skips a matching block range when applicable.
    Unsupported(&'a Token),
    Unknown,
}

impl<'a> MapDirective<'a> {
    pub(crate) fn from_token(t: &'a Token, is_unsupported: impl FnOnce(&str) -> bool) -> Self {
        match t.keyword.to_ascii_uppercase().as_str() {
            "NAME" => Self::Name(t),
            "TITLE" => Self::Title(t),
            "LAYER" => Self::Layer(t),
            "SYMBOL" => Self::Symbol,
            "METADATA" => Self::Metadata,
            other if is_unsupported(other) => Self::Unsupported(t),
            _ => Self::Unknown,
        }
    }
}

/// Directives inside a LAYER block. Block-bodied variants carry the opener
/// token so the parser keeps its line number for warnings; the slice is
/// extracted by the parser via `block_range`.
#[derive(Debug)]
pub(crate) enum LayerDirective<'a> {
    Name(&'a Token),
    Title(&'a Token),
    Type(&'a Token),
    Data(&'a Token),
    Filter(&'a Token),
    ClassItem(&'a Token),
    LabelItem(&'a Token),
    MinScaleDenom(&'a Token),
    MaxScaleDenom(&'a Token),
    Processing(&'a Token),
    ScaleToken,
    Class(&'a Token),
    Label(&'a Token),
    Group(&'a Token),
    Status(&'a Token),
    Metadata(&'a Token),
    Unsupported(&'a Token),
    Unknown,
}

impl<'a> LayerDirective<'a> {
    pub(crate) fn from_token(t: &'a Token, is_unsupported: impl FnOnce(&str) -> bool) -> Self {
        match t.keyword.to_ascii_uppercase().as_str() {
            "NAME" => Self::Name(t),
            "TITLE" => Self::Title(t),
            "TYPE" => Self::Type(t),
            "DATA" => Self::Data(t),
            "FILTER" => Self::Filter(t),
            "CLASSITEM" => Self::ClassItem(t),
            "LABELITEM" => Self::LabelItem(t),
            "MINSCALEDENOM" => Self::MinScaleDenom(t),
            "MAXSCALEDENOM" => Self::MaxScaleDenom(t),
            "PROCESSING" => Self::Processing(t),
            "SCALETOKEN" => Self::ScaleToken,
            "CLASS" => Self::Class(t),
            "LABEL" => Self::Label(t),
            "GROUP" => Self::Group(t),
            "STATUS" => Self::Status(t),
            "METADATA" => Self::Metadata(t),
            other if is_unsupported(other) => Self::Unsupported(t),
            _ => Self::Unknown,
        }
    }
}

/// Directives valid inside a CLASS block.
#[derive(Debug)]
pub(crate) enum ClassDirective<'a> {
    Name(&'a Token),
    MinScaleDenom(&'a Token),
    MaxScaleDenom(&'a Token),
    Expression(&'a Token),
    Style,
    Label(&'a Token),
    Unsupported(&'a Token),
    Unknown,
}

impl<'a> ClassDirective<'a> {
    pub(crate) fn from_token(t: &'a Token, is_unsupported: impl FnOnce(&str) -> bool) -> Self {
        match t.keyword.to_ascii_uppercase().as_str() {
            "NAME" => Self::Name(t),
            "MINSCALEDENOM" => Self::MinScaleDenom(t),
            "MAXSCALEDENOM" => Self::MaxScaleDenom(t),
            "EXPRESSION" => Self::Expression(t),
            "STYLE" => Self::Style,
            "LABEL" => Self::Label(t),
            other if is_unsupported(other) => Self::Unsupported(t),
            _ => Self::Unknown,
        }
    }
}

/// Directives valid inside a SYMBOL block. Block-bodied POINTS is the only
/// sub-block here; the parser still calls `block_range` on the token slice
/// when it sees `Points` to extract the coord list.
#[derive(Debug)]
pub(crate) enum SymbolDirective<'a> {
    Name(&'a Token),
    Type(&'a Token),
    Angle(&'a Token),
    Size(&'a Token),
    Filled(&'a Token),
    Points(&'a Token),
    AnchorPoint(&'a Token),
    Font(&'a Token),
    Character(&'a Token),
    Image(&'a Token),
    Unknown,
}

impl<'a> SymbolDirective<'a> {
    pub(crate) fn from_token(t: &'a Token) -> Self {
        match t.keyword.to_ascii_uppercase().as_str() {
            "NAME" => Self::Name(t),
            "TYPE" => Self::Type(t),
            "ANGLE" => Self::Angle(t),
            "SIZE" => Self::Size(t),
            "FILLED" => Self::Filled(t),
            "POINTS" => Self::Points(t),
            "ANCHORPOINT" => Self::AnchorPoint(t),
            "FONT" => Self::Font(t),
            "CHARACTER" => Self::Character(t),
            "IMAGE" => Self::Image(t),
            _ => Self::Unknown,
        }
    }
}

/// Directives valid inside a STYLE block. All scalar - STYLE has no
/// sub-blocks in the mapfile dialect we translate.
#[derive(Debug)]
pub(crate) enum StyleDirective<'a> {
    Color(&'a Token),
    OutlineColor(&'a Token),
    Width(&'a Token),
    OutlineWidth(&'a Token),
    Pattern(&'a Token),
    Symbol(&'a Token),
    Angle(&'a Token),
    Size(&'a Token),
    Opacity(&'a Token),
    Offset(&'a Token),
    Gap(&'a Token),
    InitialGap(&'a Token),
    LineJoin(&'a Token),
    /// `GEOMTRANSFORM "<variant>"`: vertex-extraction subset (start | end |
    /// vertices). Unknown variants land in the layer-level unimplemented bag.
    GeomTransform(&'a Token),
    /// `MINWIDTH` / `MAXWIDTH`: typed signal that a style attenuation
    /// directive is not yet implemented. The parser warns at use site.
    NotImplementedAttenuation(&'a Token),
    Unknown,
}

impl<'a> StyleDirective<'a> {
    pub(crate) fn from_token(t: &'a Token) -> Self {
        match t.keyword.to_ascii_uppercase().as_str() {
            "COLOR" => Self::Color(t),
            "OUTLINECOLOR" => Self::OutlineColor(t),
            "WIDTH" => Self::Width(t),
            "OUTLINEWIDTH" => Self::OutlineWidth(t),
            "PATTERN" => Self::Pattern(t),
            "SYMBOL" => Self::Symbol(t),
            "ANGLE" => Self::Angle(t),
            "SIZE" => Self::Size(t),
            "OPACITY" => Self::Opacity(t),
            "OFFSET" => Self::Offset(t),
            "GAP" => Self::Gap(t),
            "INITIALGAP" => Self::InitialGap(t),
            "LINEJOIN" => Self::LineJoin(t),
            "GEOMTRANSFORM" => Self::GeomTransform(t),
            "MINWIDTH" | "MAXWIDTH" => Self::NotImplementedAttenuation(t),
            _ => Self::Unknown,
        }
    }
}

/// Directives valid inside a LABEL block.
#[derive(Debug)]
pub(crate) enum LabelDirective<'a> {
    Text(&'a Token),
    Font(&'a Token),
    Size(&'a Token),
    Color(&'a Token),
    OutlineColor(&'a Token),
    OutlineWidth(&'a Token),
    Priority(&'a Token),
    MinDistance(&'a Token),
    RepeatDistance(&'a Token),
    MaxOverlapAngle(&'a Token),
    Angle(&'a Token),
    Position(&'a Token),
    Offset(&'a Token),
    Partials(&'a Token),
    Force(&'a Token),
    /// `TYPE BITMAP`: typed signal for the still-unimplemented bitmap label
    /// path. The parser warns at use site with the specific keyword.
    NotImplemented(&'a Token),
    Unknown,
}

impl<'a> LabelDirective<'a> {
    pub(crate) fn from_token(t: &'a Token) -> Self {
        match t.keyword.to_ascii_uppercase().as_str() {
            "TEXT" => Self::Text(t),
            "FONT" => Self::Font(t),
            "SIZE" => Self::Size(t),
            "COLOR" => Self::Color(t),
            "OUTLINECOLOR" => Self::OutlineColor(t),
            "OUTLINEWIDTH" => Self::OutlineWidth(t),
            "PRIORITY" => Self::Priority(t),
            "MINDISTANCE" => Self::MinDistance(t),
            "REPEATDISTANCE" => Self::RepeatDistance(t),
            "MAXOVERLAPANGLE" => Self::MaxOverlapAngle(t),
            "ANGLE" => Self::Angle(t),
            "POSITION" => Self::Position(t),
            "OFFSET" => Self::Offset(t),
            "PARTIALS" => Self::Partials(t),
            "FORCE" => Self::Force(t),
            "TYPE" => Self::NotImplemented(t),
            _ => Self::Unknown,
        }
    }
}
