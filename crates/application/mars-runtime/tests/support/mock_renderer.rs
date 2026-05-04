//! a `Renderer` impl that records draw-op batches and emits a canned PNG-magic
//! byte string instead of rasterising. avoids pulling the tiny-skia adapter
//! into the runtime test surface.

use std::sync::Mutex;

use async_trait::async_trait;
use mars_render_port::{Canvas, DrawOp, ImageFormat, RenderError, Renderer};

pub(crate) const CANNED_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Default)]
pub(crate) struct MockRenderer {
    pub ops: Mutex<Vec<Vec<DrawOp>>>,
}

#[async_trait]
impl Renderer for MockRenderer {
    async fn render(&self, _canvas: Canvas, ops: &[DrawOp], _format: ImageFormat) -> Result<Vec<u8>, RenderError> {
        self.ops.lock().expect("mock renderer lock").push(ops.to_vec());
        Ok(CANNED_BYTES.to_vec())
    }
}
