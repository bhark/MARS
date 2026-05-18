#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use tiny_skia::{PathBuilder, Pixmap as SkPixmap};

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
fn image_fill_returns_routing_contract_error() {
    let mut pm = SkPixmap::new(16, 16).unwrap();
    let fill = ResolvedFill {
        paint: FillPaint::Image { name: "brick".into() },
        alpha: 1.0,
    };
    let err = draw(&mut pm, &square_path(), &fill, BlendMode::SourceOver).expect_err("routing error");
    assert!(matches!(err, RenderError::Backend(msg) if msg.contains("DrawOp::Pattern")));
}
