//! tiny-skia rasterisation helpers. SPEC §11.2.

use mars_render_port::Path as PortPath;
use mars_style::{Colour, LineCap as SLineCap, LineJoin as SLineJoin, Style};
use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform};

/// build a tiny-skia path from port rings. each ring is closed after its last vertex.
/// returns None if no ring has at least 2 points (tiny-skia rejects empty paths).
pub(crate) fn build_path(path: &PortPath) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    let mut any = false;
    for ring in &path.rings {
        if ring.len() < 2 {
            continue;
        }
        let (x0, y0) = ring[0];
        pb.move_to(x0, y0);
        for &(x, y) in &ring[1..] {
            pb.line_to(x, y);
        }
        pb.close();
        any = true;
    }
    if !any {
        return None;
    }
    pb.finish()
}

pub(crate) fn colour_to_tsk(c: Colour) -> Color {
    Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn map_cap(c: SLineCap) -> LineCap {
    match c {
        SLineCap::Butt => LineCap::Butt,
        SLineCap::Round => LineCap::Round,
        SLineCap::Square => LineCap::Square,
    }
}

fn map_join(j: SLineJoin) -> LineJoin {
    match j {
        SLineJoin::Miter => LineJoin::Miter,
        SLineJoin::Round => LineJoin::Round,
        SLineJoin::Bevel => LineJoin::Bevel,
    }
}

/// fill the pixmap with a solid colour (used for canvas background).
pub(crate) fn fill_background(pm: &mut Pixmap, c: Colour) {
    pm.fill(colour_to_tsk(c));
}

/// draw a single styled path. uses even-odd fill rule (matches mapserver/qgis
/// expectations for self-intersecting symbol geometry; non-zero would change
/// the visual outcome of holes-as-CCW-rings produced upstream).
pub(crate) fn draw_path(pm: &mut Pixmap, path: &PortPath, style: &Style) {
    let Some(tsk_path) = build_path(path) else {
        return;
    };

    if let Some(fill) = style.fill {
        let mut paint = Paint::default();
        paint.set_color(colour_to_tsk(fill));
        paint.anti_alias = true;
        pm.fill_path(&tsk_path, &paint, FillRule::EvenOdd, Transform::identity(), None);
    }

    if let Some(stroke_col) = style.stroke {
        let width = style.stroke_width.unwrap_or(1.0);
        if width > 0.0 {
            let mut paint = Paint::default();
            paint.set_color(colour_to_tsk(stroke_col));
            paint.anti_alias = true;

            let mut stroke = Stroke {
                width,
                line_cap: style.stroke_linecap.map(map_cap).unwrap_or(LineCap::Butt),
                line_join: style.stroke_linejoin.map(map_join).unwrap_or(LineJoin::Miter),
                ..Stroke::default()
            };
            if let Some(dashes) = style.stroke_dasharray.as_ref()
                && !dashes.is_empty()
            {
                stroke.dash = StrokeDash::new(dashes.clone(), 0.0);
            }
            pm.stroke_path(&tsk_path, &paint, &stroke, Transform::identity(), None);
        }
    }
}
