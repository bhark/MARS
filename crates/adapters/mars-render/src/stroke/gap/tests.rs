#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_render_port::{Path as PortPath, Subpath};
use mars_style::{Colour, FillPaint, MarkerShape, MarkerSymbol, StrokeGap, Style};
use tiny_skia::Pixmap;

use crate::prepare;

fn fonts() -> mars_text::Fonts {
    mars_text::Fonts::with_default()
}

fn alpha_at(pm: &Pixmap, x: u32, y: u32) -> u8 {
    let w = pm.width();
    let off = ((y * w + x) * 4) as usize;
    pm.data()[off + 3]
}

fn band_has_alpha(pm: &Pixmap, x: u32, y: u32, radius: u32) -> bool {
    // search a small box around (x, y) for any non-transparent pixel.
    // antialiased marker edges may not paint the exact sample pixel.
    for dy in 0..=2 * radius {
        for dx in 0..=2 * radius {
            let xi = x.saturating_sub(radius) + dx;
            let yi = y.saturating_sub(radius) + dy;
            if xi >= pm.width() || yi >= pm.height() {
                continue;
            }
            if alpha_at(pm, xi, yi) > 0 {
                return true;
            }
        }
    }
    false
}

fn band_empty(pm: &Pixmap, x: u32, y: u32, radius: u32) -> bool {
    for dy in 0..=2 * radius {
        for dx in 0..=2 * radius {
            let xi = x.saturating_sub(radius) + dx;
            let yi = y.saturating_sub(radius) + dy;
            if xi >= pm.width() || yi >= pm.height() {
                continue;
            }
            if alpha_at(pm, xi, yi) > 0 {
                return false;
            }
        }
    }
    true
}

#[test]
fn stamps_circles_at_fixed_intervals_along_horizontal_line() {
    let mut pm = Pixmap::new(100, 16).unwrap();
    let path = PortPath {
        subpaths: vec![Subpath {
            points: vec![(0.0, 8.0), (100.0, 8.0)],
            closed: false,
        }],
    };
    let style = Style {
        fill: Some(FillPaint::Solid(Colour::rgba(255, 0, 0, 255))),
        stroke: Some(Colour::rgba(0, 0, 0, 255)),
        stroke_width: Some(1.0.into()),
        marker: Some(MarkerSymbol {
            shape: MarkerShape::Circle,
            size: 4.0.into(),
            angle: None,
        }),
        stroke_gap: Some(StrokeGap {
            interval_px: 20.0,
            initial_px: 4.0,
        }),
        ..Default::default()
    }
    .resolve(0);
    let marker = style.marker.clone().expect("marker");
    let gap = prepare::resolve(&style).stroke.expect("stroke").gap.expect("gap");

    super::stamp(&mut pm, &path, &marker, &style, gap, &fonts()).expect("stamp ok");

    // expected sample positions
    for x in [4u32, 24, 44, 64, 84] {
        assert!(band_has_alpha(&pm, x, 8, 3), "no marker near x={x}");
    }
    // between samples (mid points 14, 34, 54, 74) should be clear: the
    // circles have radius 2, so a 3-px band centred 10 px from any
    // stamp must be empty.
    for x in [14u32, 34, 54, 74] {
        assert!(band_empty(&pm, x, 8, 3), "spurious paint near x={x}");
    }
}

#[test]
fn no_stamps_when_initial_exceeds_total_length() {
    let mut pm = Pixmap::new(40, 16).unwrap();
    let path = PortPath {
        subpaths: vec![Subpath {
            points: vec![(0.0, 8.0), (10.0, 8.0)],
            closed: false,
        }],
    };
    let style = Style {
        fill: Some(FillPaint::Solid(Colour::rgba(255, 0, 0, 255))),
        marker: Some(MarkerSymbol {
            shape: MarkerShape::Circle,
            size: 4.0.into(),
            angle: None,
        }),
        ..Default::default()
    }
    .resolve(0);
    let marker = style.marker.clone().expect("marker");
    let gap = crate::prepare::ResolvedStrokeGap {
        interval_px: 5.0,
        initial_px: 20.0,
    };
    super::stamp(&mut pm, &path, &marker, &style, gap, &fonts()).expect("stamp ok");
    let painted = pm.data().chunks_exact(4).filter(|p| p[3] > 0).count();
    assert_eq!(painted, 0, "no stamps should land past arc total");
}

#[test]
fn l_shape_stamps_rotate_with_post_corner_tangent() {
    // horizontal then vertical leg, 20 px each. with initial=2 and
    // interval=10, samples land at arcs 2, 12, 22, 32. arc 22 sits 2 px
    // past the corner on the vertical leg, so a marker placed there
    // must show up below the corner.
    let mut pm = Pixmap::new(40, 40).unwrap();
    let path = PortPath {
        subpaths: vec![Subpath {
            points: vec![(0.0, 8.0), (20.0, 8.0), (20.0, 28.0)],
            closed: false,
        }],
    };
    let style = Style {
        fill: Some(FillPaint::Solid(Colour::rgba(255, 0, 0, 255))),
        marker: Some(MarkerSymbol {
            shape: MarkerShape::Square,
            size: 3.0.into(),
            angle: None,
        }),
        ..Default::default()
    }
    .resolve(0);
    let marker = style.marker.clone().expect("marker");
    let gap = crate::prepare::ResolvedStrokeGap {
        interval_px: 10.0,
        initial_px: 2.0,
    };
    super::stamp(&mut pm, &path, &marker, &style, gap, &fonts()).expect("stamp ok");

    // post-corner sample lands at (20, 10) - assert paint there.
    assert!(band_has_alpha(&pm, 20, 10, 2), "missing post-corner stamp");
}
