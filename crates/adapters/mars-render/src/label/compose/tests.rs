#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use mars_text::GlyphMask;

fn mask3x3() -> GlyphMask {
    // a 3x3 mask with a non-trivial coverage pattern; origin (0,0).
    GlyphMask {
        width: 3,
        height: 3,
        origin_x: 0,
        origin_y: 0,
        coverage: vec![10, 20, 30, 40, 50, 60, 70, 80, 90],
    }
}

#[test]
fn axis_sampler_parity_with_raw_mask_read() {
    let mask = mask3x3();
    let s = AxisSampler {
        mask: &mask,
        dst_x0: 5,
        dst_y0: 7,
    };
    for my in 0..3i32 {
        for mx in 0..3i32 {
            let raw = mask.coverage[(my * 3 + mx) as usize];
            let got = s.sample(5 + mx, 7 + my).expect("inside");
            assert_eq!(got, raw, "axis sampler mismatch at ({mx},{my})");
        }
    }
}

#[test]
fn rotated_sampler_zero_degrees_matches_axis() {
    let mask = mask3x3();
    let axis = AxisSampler {
        mask: &mask,
        dst_x0: 4,
        dst_y0: 9,
    };
    let rot = RotatedSampler {
        mask: &mask,
        anchor: (4.0, 9.0),
        origin: (0.0, 0.0),
        cos: 1.0,
        sin: 0.0,
    };
    for my in 0..3i32 {
        for mx in 0..3i32 {
            let dx = 4 + mx;
            let dy = 9 + my;
            let a = axis.sample(dx, dy).expect("axis inside");
            let r = rot.sample(dx, dy).expect("rotated inside");
            assert_eq!(a, r, "0-deg rotated must match axis at ({mx},{my})");
        }
    }
}

#[test]
fn rotated_sampler_ninety_degrees_round_trip() {
    // 90 ccw rotation around anchor at (0,0). canvas (dx, dy) maps via
    // inverse rotation to mask coords. for cos=0, sin=1: lx = sin*dy, ly = -sin*dx
    // wait: lx = cos*rx + sin*ry = 0*rx + 1*ry = ry = dy; ly = -sin*rx + cos*ry = -rx = -dx
    // so canvas (dx, dy) -> mask (dy, -dx). pick a pixel and verify.
    let mask = mask3x3();
    let rot = RotatedSampler {
        mask: &mask,
        anchor: (0.0, 0.0),
        origin: (0.0, 0.0),
        cos: 0.0,
        sin: 1.0,
    };
    // canvas (dy=2, dx=0) -> mask (2, 0) = coverage[2] = 30
    assert_eq!(rot.sample(0, 2), Some(30));
    // canvas (dx=0, dy=0) -> mask (0, 0) = coverage[0] = 10
    assert_eq!(rot.sample(0, 0), Some(10));
    // out of mask
    assert_eq!(rot.sample(5, 5), None);
}
