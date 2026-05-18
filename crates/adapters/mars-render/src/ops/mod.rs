//! Per-`DrawOp`-variant tiny-skia draw helpers. The port-level
//! [`mars_render_port::dispatch_ops`] walks `DrawOp`s and routes each
//! through the [`mars_render_port::Surface`] impl in `crate::surface`,
//! which in turn calls into these helpers. Adding a new `DrawOp` variant
//! breaks the build at the Surface impl, which is the canonical seam.

pub(crate) mod label;
pub(crate) mod path;
pub(crate) mod pattern;

#[cfg(test)]
mod tests;
