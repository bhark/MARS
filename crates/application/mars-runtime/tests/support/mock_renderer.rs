//! a `Renderer` + `Encoder` pair that record draw-op batches and emit a canned
//! PNG-magic byte string instead of rasterising. avoids pulling the tiny-skia
//! adapter into the runtime test surface.

use std::sync::Mutex;

use mars_render_port::{Canvas, DrawOp, EncodeError, Encoder, ImageFormat, Pixmap, RenderError, Renderer};

pub(crate) const CANNED_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n";

#[derive(Default)]
pub(crate) struct MockRenderer {
    pub ops: Mutex<Vec<Vec<DrawOp>>>,
}

impl Renderer for MockRenderer {
    fn render(&self, canvas: Canvas, ops: &[DrawOp]) -> Result<Pixmap, RenderError> {
        self.ops.lock().expect("mock renderer lock").push(ops.to_vec());
        Ok(Pixmap {
            width: canvas.width,
            height: canvas.height,
            premultiplied_rgba: vec![0u8; (canvas.width * canvas.height * 4) as usize],
        })
    }
}

/// encoder that ignores input and yields the canned PNG-magic byte string,
/// letting render-pipeline tests assert on a stable byte equality.
#[derive(Default)]
pub(crate) struct CannedEncoder;

impl Encoder for CannedEncoder {
    fn encode(&self, _pixmap: &Pixmap, _format: ImageFormat) -> Result<Vec<u8>, EncodeError> {
        Ok(CANNED_BYTES.to_vec())
    }
}
