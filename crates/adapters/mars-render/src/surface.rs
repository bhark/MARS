//! tiny-skia implementation of the [`mars_render_port::Surface`] trait.
//!
//! `TinySkiaSurface` owns a mutable `tiny_skia::Pixmap` plus the renderer's
//! font and image registries; each method delegates to the existing internal
//! helpers (`fill`, `stroke`, `label`, `pattern`, `raster`) so the
//! tiny-skia-specific code stays in one place while every backend-agnostic
//! caller can talk to the port-level surface.

use std::sync::Arc;

use mars_render_port::{DecodedImage, Path as PortPath, PixelRect, Pixmap, RenderError, Surface};
use mars_style::{ResolvedLabelStyle, ResolvedStyle};
use mars_text::Fonts;
use tiny_skia::Pixmap as SkPixmap;

use crate::canvas::fill_background;
use crate::ops;

/// Concrete `Surface` impl backed by a tiny-skia `Pixmap`. Constructed once
/// per render call; consumed via `finish` which yields the port-level
/// `Pixmap`.
pub(crate) struct TinySkiaSurface {
    pm: SkPixmap,
    fonts: Arc<Fonts>,
    images: Arc<dyn mars_render_port::ImageRegistry>,
}

impl TinySkiaSurface {
    /// Allocate a fresh tiny-skia pixmap of the requested size. Returns an
    /// error if the allocation fails (impossible in practice for sensible
    /// sizes but tiny-skia exposes it so we propagate).
    pub(crate) fn new(
        width: u32,
        height: u32,
        fonts: Arc<Fonts>,
        images: Arc<dyn mars_render_port::ImageRegistry>,
    ) -> Result<Self, RenderError> {
        let pm = SkPixmap::new(width, height)
            .ok_or_else(|| RenderError::Backend(format!("pixmap alloc {width}x{height}")))?;
        Ok(Self { pm, fonts, images })
    }
}

impl Surface for TinySkiaSurface {
    fn fill_background(&mut self, colour: mars_style::Colour) {
        fill_background(&mut self.pm, colour);
    }

    fn draw_path(&mut self, path: &PortPath, style: &ResolvedStyle) -> Result<(), RenderError> {
        ops::path::draw(&mut self.pm, path, style, &self.fonts)
    }

    fn draw_label(
        &mut self,
        anchor: (f32, f32),
        text: &str,
        style: &ResolvedLabelStyle,
        angle_rad: f32,
    ) -> Result<(), RenderError> {
        ops::label::draw(&mut self.pm, anchor, text, style, angle_rad, &self.fonts)
    }

    fn draw_follow_label(
        &mut self,
        polyline_px: &[(f32, f32)],
        start_arc_px: f32,
        text: &str,
        style: &ResolvedLabelStyle,
    ) -> Result<(), RenderError> {
        ops::label::draw_follow(&mut self.pm, polyline_px, start_arc_px, text, style, &self.fonts)
    }

    fn draw_symbol(&mut self, anchor: (f32, f32), rotation_rad: f32, style: &ResolvedStyle) -> Result<(), RenderError> {
        crate::symbol::dispatch(&mut self.pm, anchor, rotation_rad, style, &self.fonts)
    }

    fn draw_pattern(&mut self, path: &PortPath, style: &ResolvedStyle) -> Result<(), RenderError> {
        ops::pattern::draw(&mut self.pm, path, style, self.images.as_ref())
    }

    fn draw_raster(
        &mut self,
        tile: &DecodedImage,
        dst: PixelRect,
        opacity: f32,
        blend_mode: Option<mars_style::BlendMode>,
    ) -> Result<(), RenderError> {
        crate::raster::draw(&mut self.pm, tile, dst, opacity, blend_mode)
    }

    fn finish(self: Box<Self>) -> Pixmap {
        let pm = self.pm;
        let width = pm.width();
        let height = pm.height();
        Pixmap {
            width,
            height,
            premultiplied_rgba: pm.take(),
        }
    }
}
