//! Thin re-export of the KVP helpers lifted into `mars-ows-common`. Kept as
//! a local module so existing `super::common::*` imports stay valid; new
//! callers can reach for `mars_ows_common` directly.

pub(super) use mars_ows_common::{Kvp, nonempty, parse_kvp, parse_optional_u32, require};

#[cfg(test)]
mod tests;
