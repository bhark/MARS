//! tiled image pattern fill. Sampling against a renderer-side image
//! registry is not yet implemented; the dispatch arm lands first per
//! EXTENDING.md principle 5 (typed `NotImplemented` is a valid landing
//! state) so the vocabulary + routing are in place ahead of the
//! sampling commit.

use mars_render_port::RenderError;
use tiny_skia::Pixmap;

pub(crate) fn draw(pm: &mut Pixmap, path: &tiny_skia::Path, name: &str, alpha: f32) -> Result<(), RenderError> {
    let _ = pm;
    let _ = path;
    let _ = name;
    let _ = alpha;
    Err(RenderError::NotImplemented {
        what: "FillPaint::Image",
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use tiny_skia::{PathBuilder, Pixmap as SkPixmap};

    #[test]
    fn returns_typed_not_implemented_until_sampling_lands() {
        let mut pm = SkPixmap::new(16, 16).unwrap();
        let mut pb = PathBuilder::new();
        pb.move_to(0.0, 0.0);
        pb.line_to(4.0, 0.0);
        pb.line_to(4.0, 4.0);
        pb.close();
        let path = pb.finish().unwrap();
        let err = draw(&mut pm, &path, "brick", 1.0).expect_err("not implemented yet");
        assert!(matches!(err, RenderError::NotImplemented { what } if what == "FillPaint::Image"));
    }
}
