//! OWS Common substrate for OGC interface adapters (WMS, WMTS, future
//! WFS/WCS). Carries the protocol-agnostic machinery the interface crates
//! were copying instead of sharing: KVP parsing (case-insensitive keys,
//! percent-decoding, optional-typed accessors), an [`OwsParseError`] trait
//! the per-interface error types implement to slot into shared helpers,
//! and (in later modules) XML emit primitives and format negotiation.

#![forbid(unsafe_code)]

pub mod parse;

pub use parse::{Kvp, OwsParseError, nonempty, parse_kvp, parse_optional_u32, pct_decode, require};
