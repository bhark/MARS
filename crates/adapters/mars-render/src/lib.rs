//! CPU rasteriser. SPEC §11.2 — tiny-skia + rustybuzz + cosmic-text. The
//! actual implementation lands in Phase 0/1; this crate keeps the boundary
//! clean today so `mars-runtime` can already wire to a `Renderer` impl.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use mars_render_port::{Canvas, DrawOp, ImageFormat, RenderError, Renderer};

#[derive(Debug, Default)]
pub struct StubRenderer;

#[async_trait]
impl Renderer for StubRenderer {
    async fn render(&self, _canvas: Canvas, _ops: &[DrawOp], _format: ImageFormat) -> Result<Vec<u8>, RenderError> {
        // todo(SPEC §11.2) wire tiny-skia rasteriser
        Err(RenderError::NotImplemented {
            what: "mars-render::StubRenderer::render",
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_returns_not_implemented() {
        let r = StubRenderer;
        let c = Canvas {
            width: 1,
            height: 1,
            background: None,
        };
        assert!(matches!(
            r.render(c, &[], ImageFormat::Png).await,
            Err(RenderError::NotImplemented { .. })
        ));
    }
}
