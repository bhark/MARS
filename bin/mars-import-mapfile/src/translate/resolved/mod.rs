//! parse -> resolve normalisation: collapses Option-heavy `ParsedX` into
//! non-Option `ResolvedX` with every default unwrapped exactly once.
//!
//! mirrors the role of `mars-render/src/prepare.rs::resolve`. callers in
//! [`super::emit`] read from `ResolvedX` and never call `.unwrap_or(default)`
//! for anything resolvable here. when a new mapfile default lands (e.g.
//! `"geometri"`, `"polygon"`, `"sans-serif"`), it lives here exactly once.
//!
//! Layout mirrors the parse side under `translate/`: one module per
//! resolved entity (layer / source / class / label / symbol). `layer.rs`
//! is the entry point; the rest are reached through it.

mod class;
mod label;
mod layer;
mod source;
mod symbol;

pub(crate) use class::ResolvedClass;
pub(crate) use label::ResolvedLabel;
pub(crate) use layer::{ResolvedLayer, resolve_layer};
pub(crate) use symbol::{ResolvedSymbol, resolve_symbol};
