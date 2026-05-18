#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_render_port::EmptyImageRegistry;
use mars_style::Colour;
use tiny_skia::{PathBuilder, Pixmap as SkPixmap};

use super::*;

fn square_path() -> tiny_skia::Path {
    let mut pb = PathBuilder::new();
    pb.move_to(2.0, 2.0);
    pb.line_to(8.0, 2.0);
    pb.line_to(8.0, 8.0);
    pb.line_to(2.0, 8.0);
    pb.close();
    pb.finish().unwrap()
}

#[test]
fn solid_fill_returns_routing_contract_error() {
    let mut pm = SkPixmap::new(16, 16).unwrap();
    let fill = ResolvedFill {
        paint: FillPaint::Solid(Colour::rgba(255, 0, 0, 255)),
        alpha: 1.0,
    };
    let err = draw(
        &mut pm,
        &square_path(),
        &fill,
        BlendMode::SourceOver,
        &EmptyImageRegistry,
    )
    .expect_err("routing error");
    assert!(matches!(err, RenderError::Backend(msg) if msg.contains("DrawOp::Path")));
}

#[test]
fn image_fill_routes_to_image_dispatch() {
    // EmptyImageRegistry has no entries, so the dispatch routes through to
    // pattern::image::draw which surfaces ImageNotFound.
    let mut pm = SkPixmap::new(16, 16).unwrap();
    let fill = ResolvedFill {
        paint: FillPaint::Image { name: "brick".into() },
        alpha: 1.0,
    };
    let err = draw(
        &mut pm,
        &square_path(),
        &fill,
        BlendMode::SourceOver,
        &EmptyImageRegistry,
    )
    .expect_err("missing image");
    assert!(matches!(err, RenderError::ImageNotFound { ref name } if name == "brick"));
}
