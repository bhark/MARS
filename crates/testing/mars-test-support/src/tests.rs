use super::*;

fn rgb_png(w: u32, h: u32, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut wr = enc.write_header().unwrap();
    wr.write_image_data(pixels).unwrap();
    drop(wr);
    out
}

/// 5x1 strip with a single black pixel; jittered by one column. radius=1
/// must forgive entirely; radius=0 must count both endpoints as differing.
#[test]
fn neighborhood_forgives_one_pixel_jitter() {
    let mut a = vec![255u8; 5 * 3];
    let mut g = vec![255u8; 5 * 3];
    a[3..6].copy_from_slice(&[0, 0, 0]); // black at x=1
    g[6..9].copy_from_slice(&[0, 0, 0]); // black at x=2

    let pa = rgb_png(5, 1, &a);
    let pg = rgb_png(5, 1, &g);

    let r1 = diff_pngs_with_radius(&pa, &pg, 0, 1).unwrap();
    assert_eq!(r1.strict_differing, 2);
    assert_eq!(r1.differing_pixels, 0, "neighborhood forgives both");

    let r0 = diff_pngs_with_radius(&pa, &pg, 0, 0).unwrap();
    assert_eq!(r0.differing_pixels, 2, "no neighborhood => count both");
}

/// when the actual paints something not present anywhere nearby in golden,
/// the neighborhood pass still flags it.
#[test]
fn neighborhood_keeps_real_divergence() {
    let mut a = vec![255u8; 5 * 3];
    let g = vec![255u8; 5 * 3];
    a[6..9].copy_from_slice(&[0, 0, 0]);

    let pa = rgb_png(5, 1, &a);
    let pg = rgb_png(5, 1, &g);

    let r = diff_pngs_with_radius(&pa, &pg, 0, 1).unwrap();
    assert_eq!(r.strict_differing, 1);
    assert_eq!(r.differing_pixels, 1, "no match for black anywhere in golden");
}
