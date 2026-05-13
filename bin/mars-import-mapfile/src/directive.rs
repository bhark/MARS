//! per-block typed directive dispatch for the mapfile translator.
//!
//! Each per-block parser walks a `&[Token]` body slice. Pre-refactor the
//! dispatch was a string-keyed `match kw.as_str() { ... }` repeated in
//! every parser. This module collapses that into per-block enums plus
//! `from_token` constructors so each parser becomes a plain `match` on the
//! enum, and the keyword string match lives in exactly one place.

use crate::scanner::Token;

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
    /// `POSITION`, `PARTIALS`, `OFFSET`, `TYPE BITMAP`: typed signal that a
    /// label directive is recognised but not yet implemented. The parser
    /// warns at use site with the specific keyword.
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
            "POSITION" | "PARTIALS" | "OFFSET" | "TYPE" => Self::NotImplemented(t),
            _ => Self::Unknown,
        }
    }
}
