//! load font faces from `service.fonts` paths and the vendored fallback.

use std::sync::Arc;

use anyhow::{Context, Result};
use mars_config::Config;
use mars_text::Fonts;

/// build the renderer's font registry from the `service.fonts` block.
pub fn load_fonts(cfg: &Config) -> Result<Arc<Fonts>> {
    let f = &cfg.service.fonts;
    let fonts = Fonts::load(&f.paths, f.bundle_default).context("load fonts")?;
    Ok(Arc::new(fonts))
}
