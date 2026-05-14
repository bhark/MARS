//! raster-tile compositor. scaffold today: returns a typed `NotImplemented`
//! so the surface flows end to end without any pixels painted. concrete
//! impl will sample the decoded tile into the destination rect using
//! tiny-skia (or a successor adapter when the scope grows).

use std::sync::Arc;

use mars_render_port::{DecodedImage, PixelRect, RenderError};
use tiny_skia::Pixmap;

pub(crate) fn draw(
    _pm: &mut Pixmap,
    _tile: &Arc<DecodedImage>,
    _dst: PixelRect,
    _opacity: f32,
) -> Result<(), RenderError> {
    Err(RenderError::NotImplemented { what: "DrawOp::Raster" })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn raster_dispatch_is_not_implemented() {
        let mut pm = Pixmap::new(8, 8).unwrap();
        let tile = Arc::new(DecodedImage {
            width: 2,
            height: 2,
            rgba: Arc::new(vec![0; 16]),
        });
        let err = draw(
            &mut pm,
            &tile,
            PixelRect {
                x: 0.0,
                y: 0.0,
                w: 8.0,
                h: 8.0,
            },
            1.0,
        )
        .expect_err("must be NotImplemented");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "DrawOp::Raster"));
    }
}
