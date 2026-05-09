//! informational image cross-check.
//!
//! rasterises the full polygon set (originals and simplified) at a fitted
//! canvas, then writes side-by-side PNGs plus a pixel diff. NOT the gate —
//! seam-preservation is the gate. this is for eyeballing sliver gaps and
//! large-scale shape divergence at coarse decimation tolerances; the diff
//! pixel count is reported alongside but does not influence pass/fail.

// canvas_px / paths on DiffReport are reported by the gate write-up landing
// in the next commit; allow until then so the report stays stable.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use mars_artifact::{Coord, FeatureGeom, GeomKind};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

#[derive(Debug, Clone, Default)]
pub struct DiffReport {
    pub canvas_px: u32,
    pub differing_pixels: u64,
    pub total_pixels: u64,
    pub original_path: PathBuf,
    pub simplified_path: PathBuf,
    pub diff_path: PathBuf,
}

pub fn render_and_diff(
    originals: &[FeatureGeom],
    simplified: &[FeatureGeom],
    canvas_px: u32,
    out_dir: &Path,
    level_label: &str,
) -> anyhow::Result<DiffReport> {
    std::fs::create_dir_all(out_dir)?;
    let bbox = combined_bbox(originals).ok_or_else(|| anyhow::anyhow!("empty geometry set"))?;
    let (w, h, transform) = fit_to_canvas(bbox, canvas_px);

    let pm_orig = rasterise(originals, w, h, transform)?;
    let pm_simp = rasterise(simplified, w, h, transform)?;

    let original_path = out_dir.join(format!("level{level_label}_original.png"));
    let simplified_path = out_dir.join(format!("level{level_label}_simplified.png"));
    let diff_path = out_dir.join(format!("level{level_label}_diff.png"));
    pm_orig.save_png(&original_path)?;
    pm_simp.save_png(&simplified_path)?;

    let (diff_pm, diff_count) = pixel_diff(&pm_orig, &pm_simp)?;
    diff_pm.save_png(&diff_path)?;

    Ok(DiffReport {
        canvas_px,
        differing_pixels: diff_count,
        total_pixels: u64::from(w) * u64::from(h),
        original_path,
        simplified_path,
        diff_path,
    })
}

fn combined_bbox(geoms: &[FeatureGeom]) -> Option<(f64, f64, f64, f64)> {
    let mut minx = f64::INFINITY;
    let mut miny = f64::INFINITY;
    let mut maxx = f64::NEG_INFINITY;
    let mut maxy = f64::NEG_INFINITY;
    let mut seen = false;
    for f in geoms {
        for_each_coord(&f.geom, |x, y| {
            seen = true;
            if x < minx {
                minx = x;
            }
            if y < miny {
                miny = y;
            }
            if x > maxx {
                maxx = x;
            }
            if y > maxy {
                maxy = y;
            }
        });
    }
    if !seen {
        return None;
    }
    Some((minx, miny, maxx, maxy))
}

fn fit_to_canvas(bbox: (f64, f64, f64, f64), canvas_px: u32) -> (u32, u32, Transform) {
    let (minx, miny, maxx, maxy) = bbox;
    let dx = (maxx - minx).max(1e-9);
    let dy = (maxy - miny).max(1e-9);
    let aspect = dx / dy;
    let (w, h) = if aspect >= 1.0 {
        (canvas_px, ((canvas_px as f64) / aspect).max(1.0) as u32)
    } else {
        (((canvas_px as f64) * aspect).max(1.0) as u32, canvas_px)
    };
    let sx = (w as f64) / dx;
    let sy = (h as f64) / dy;
    let s = sx.min(sy);
    // y flip: data y grows up, canvas y grows down.
    let tx = -minx * s;
    let ty = (h as f64) + miny * s;
    let transform = Transform::from_row(s as f32, 0.0, 0.0, -s as f32, tx as f32, ty as f32);
    (w.max(1), h.max(1), transform)
}

fn rasterise(geoms: &[FeatureGeom], w: u32, h: u32, transform: Transform) -> anyhow::Result<Pixmap> {
    let mut pm = Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("pixmap allocation failed"))?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, 255);
    paint.anti_alias = false;
    for f in geoms {
        match &f.geom {
            GeomKind::Polygon(rings) => fill_polygon(&mut pm, rings, &paint, transform),
            GeomKind::MultiPolygon(parts) => {
                for rings in parts {
                    fill_polygon(&mut pm, rings, &paint, transform);
                }
            }
            _ => {}
        }
    }
    Ok(pm)
}

fn fill_polygon(pm: &mut Pixmap, rings: &[Vec<Coord>], paint: &Paint, transform: Transform) {
    let mut pb = PathBuilder::new();
    for ring in rings {
        if ring.len() < 3 {
            continue;
        }
        let (x0, y0) = ring[0];
        pb.move_to(x0 as f32, y0 as f32);
        for &(x, y) in &ring[1..] {
            pb.line_to(x as f32, y as f32);
        }
        pb.close();
    }
    if let Some(path) = pb.finish() {
        pm.fill_path(&path, paint, FillRule::EvenOdd, transform, None);
    }
}

fn pixel_diff(a: &Pixmap, b: &Pixmap) -> anyhow::Result<(Pixmap, u64)> {
    let w = a.width();
    let h = a.height();
    let mut out = Pixmap::new(w, h).ok_or_else(|| anyhow::anyhow!("pixmap allocation failed"))?;
    let pa = a.data();
    let pb = b.data();
    let po = out.data_mut();
    let mut differing = 0u64;
    for i in (0..pa.len()).step_by(4) {
        let alpha_a = pa[i + 3];
        let alpha_b = pb[i + 3];
        let differ = alpha_a != alpha_b;
        if differ {
            differing += 1;
            // red where original was filled but simplified isn't (gap/sliver),
            // green where simplified covers extra ground (overshoot).
            if alpha_a > alpha_b {
                po[i] = 255;
                po[i + 1] = 0;
                po[i + 2] = 0;
                po[i + 3] = 255;
            } else {
                po[i] = 0;
                po[i + 1] = 255;
                po[i + 2] = 0;
                po[i + 3] = 255;
            }
        }
    }
    Ok((out, differing))
}

fn for_each_coord(g: &GeomKind, mut f: impl FnMut(f64, f64)) {
    match g {
        GeomKind::Polygon(rings) => {
            for r in rings {
                for &(x, y) in r {
                    f(x, y);
                }
            }
        }
        GeomKind::MultiPolygon(parts) => {
            for p in parts {
                for r in p {
                    for &(x, y) in r {
                        f(x, y);
                    }
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn poly(id: u64, ring: Vec<Coord>) -> FeatureGeom {
        FeatureGeom {
            user_id: id,
            bbox: [0.0, 0.0, 0.0, 0.0],
            geom: GeomKind::Polygon(vec![ring]),
        }
    }

    fn tempdir(tag: &str) -> PathBuf {
        let base = std::env::var_os("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("topo-imagediff-{}-{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identical_geoms_have_zero_diff() {
        let p = poly(1, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let geoms = vec![p];
        let dir = tempdir("identical");
        let report = render_and_diff(&geoms, &geoms, 256, &dir, "1").unwrap();
        assert_eq!(report.differing_pixels, 0);
        assert!(report.original_path.exists());
        assert!(report.simplified_path.exists());
        assert!(report.diff_path.exists());
    }

    #[test]
    fn shifted_geoms_have_nonzero_diff() {
        let a = poly(1, vec![(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 0.0)]);
        let b = poly(1, vec![(2.0, 0.0), (12.0, 0.0), (12.0, 10.0), (2.0, 10.0), (2.0, 0.0)]);
        let dir = tempdir("shifted");
        let report = render_and_diff(&[a], &[b], 256, &dir, "1").unwrap();
        assert!(report.differing_pixels > 0);
    }
}
