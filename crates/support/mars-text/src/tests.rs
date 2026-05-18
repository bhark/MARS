#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mars_style::Colour;

use super::*;

fn lbl(family: &str, size: f32) -> ResolvedLabelStyle {
    ResolvedLabelStyle {
        font_family: family.into(),
        font_size: size,
        fill: Colour::rgba(0, 0, 0, 0xff),
        halo: None,
        priority: 0,
        min_distance: 0.0,
        position: mars_style::AnchorPosition::default(),
        offset_px: (0.0, 0.0),
        angle_deg: None,
        partials: false,
        force: false,
    }
}

#[test]
fn measure_hello_advance_in_range() {
    let fonts = Fonts::with_default();
    let run = measure("hello", &lbl("DejaVu Sans", 12.0), &fonts).unwrap();
    // dejavu sans 12pt "hello" advances ≈ 30.0 px. allow a wide band; the
    // ratchet check below pins the exact value.
    assert!(
        run.advance_x > 20.0 && run.advance_x < 40.0,
        "advance {} out of band",
        run.advance_x
    );
    assert!(run.ascent > 0.0);
    assert!(run.descent > 0.0);
    assert_eq!(run.glyphs.len(), 5);
}

#[test]
fn measure_advance_is_stable() {
    // exact pixel advance for DejaVu Sans 12pt "hello". CI-stable because
    // the font is vendored. update with care.
    let fonts = Fonts::with_default();
    let run = measure("hello", &lbl("DejaVu Sans", 12.0), &fonts).unwrap();
    let expected = 28.998047_f32;
    assert!(
        (run.advance_x - expected).abs() < 0.5,
        "advance {} drifted from baked value {}",
        run.advance_x,
        expected
    );
}

#[test]
fn rasterise_produces_nonempty_mask() {
    let fonts = Fonts::with_default();
    let run = measure("Hi", &lbl("DejaVu Sans", 16.0), &fonts).unwrap();
    let mask = rasterise(&run).unwrap();
    assert!(mask.width > 0 && mask.height > 0);
    let lit = mask.coverage.iter().filter(|&&a| a > 0).count();
    assert!(lit > 0, "expected some lit pixels");
}

#[test]
fn unknown_family_falls_back_to_dejavu() {
    let fonts = Fonts::with_default();
    // unrecognised family should still resolve via the fontdb fallback chain.
    let run = measure("x", &lbl("Definitely-Not-A-Font", 10.0), &fonts).unwrap();
    assert!(run.advance_x > 0.0);
}

#[test]
fn empty_text_yields_zero_advance() {
    let fonts = Fonts::with_default();
    let run = measure("", &lbl("DejaVu Sans", 12.0), &fonts).unwrap();
    assert_eq!(run.glyphs.len(), 0);
    assert_eq!(run.advance_x, 0.0);
}

#[test]
fn glyphs_iter_advances_monotonically_and_sums_to_run_advance() {
    let fonts = Fonts::with_default();
    let run = measure("Hello", &lbl("DejaVu Sans", 16.0), &fonts).unwrap();
    assert!(run.glyph_count() >= 5, "got {}", run.glyph_count());
    let layouts: Vec<_> = run.glyphs().collect();
    // x positions are monotonic non-decreasing.
    for w in layouts.windows(2) {
        assert!(w[1].x >= w[0].x, "non-monotonic: {} then {}", w[0].x, w[1].x);
    }
    // advances are positive for visible glyphs.
    for g in &layouts {
        assert!(g.advance_x >= 0.0);
    }
    // sum of per-glyph advances ≈ total run advance.
    let sum: f32 = layouts.iter().map(|g| g.advance_x).sum();
    assert!((sum - run.advance_x).abs() < 1e-3, "sum {sum} vs run {}", run.advance_x);
}

#[test]
fn rasterise_glyph_paints_a_visible_letter() {
    let fonts = Fonts::with_default();
    let run = measure("A", &lbl("DejaVu Sans", 24.0), &fonts).unwrap();
    let mask = rasterise_glyph(&run, 0).unwrap();
    assert!(mask.width > 0 && mask.height > 0);
    let lit = mask.coverage.iter().filter(|&&a| a > 0).count();
    assert!(lit > 0, "expected some lit pixels for 'A'");
}

#[test]
fn rasterise_glyph_out_of_bounds_returns_empty_mask() {
    let fonts = Fonts::with_default();
    let run = measure("x", &lbl("DejaVu Sans", 16.0), &fonts).unwrap();
    let mask = rasterise_glyph(&run, 99).unwrap();
    assert_eq!(mask.width, 0);
    assert_eq!(mask.height, 0);
    assert!(mask.coverage.is_empty());
}
