//! font discovery, shaping, and glyph rasterisation for MARS labels.
//!
//! - [`Fonts`] wraps `fontdb` for face discovery; `load(paths, bundle_default)`
//!   walks a list of search paths and optionally appends the vendored DejaVu Sans
//!   so goldens never depend on system fontconfig.
//! - [`measure`] shapes a single line of text via rustybuzz and reports advance,
//!   ascent, and descent in pixel space.
//! - [`rasterise`] returns an alpha mask covering the shaped run; the caller
//!   composites halo + fill (the renderer adapter handles colour).
//!
//! No async, no I/O outside font loading. Multi-line shaping is deferred
//! to v1.1; rustybuzz on a single line is enough for v1 placement.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

use std::path::Path;
use std::sync::Arc;

use fontdb::{Database, Family, ID, Query, Source, Stretch, Style as FontStyle, Weight};
use mars_style::LabelStyle;
use thiserror::Error;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Transform};

/// vendored fallback font; ensures CI golden stability across hosts where
/// system fontconfig may or may not surface DejaVu Sans.
const BUNDLED_DEJAVU: &[u8] = include_bytes!("../test_fonts/DejaVuSans.ttf");

/// shared font bytes; matches fontdb's `Source::Binary` Arc shape so we can
/// hold the same allocation without copying.
type FaceBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// errors produced by `mars-text`.
#[derive(Debug, Error)]
pub enum FontError {
    /// no face matched the requested family.
    #[error("font family not found: {0}")]
    FamilyNotFound(String),
    /// face data could not be loaded from the database.
    #[error("font face load failed: {0}")]
    FaceLoad(String),
    /// shaping or outline parse error.
    #[error("font face parse error: {0}")]
    FaceParse(String),
    /// I/O error while loading fonts.
    #[error("font I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// in-memory font registry. cheap to clone behind `Arc`.
#[derive(Debug, Default)]
pub struct Fonts {
    db: Database,
}

impl Fonts {
    /// load fonts from `paths` (recursively walked by fontdb). when
    /// `bundle_default` is set, the vendored DejaVu Sans is registered last so
    /// it acts as a fallback without overriding deliberate user installs.
    pub fn load<P: AsRef<Path>>(paths: &[P], bundle_default: bool) -> Result<Self, FontError> {
        let mut db = Database::new();
        for p in paths {
            let path = p.as_ref();
            // fontdb::load_fonts_dir silently swallows i/o errors; surface the
            // most common operator mistake (path missing or unreadable) before
            // labels go quietly blank.
            match std::fs::metadata(path) {
                Ok(_) => db.load_fonts_dir(path),
                Err(e) => tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "font path skipped",
                ),
            }
        }
        if bundle_default {
            db.load_font_data(BUNDLED_DEJAVU.to_vec());
        }
        Ok(Self { db })
    }

    /// construct with only the vendored DejaVu Sans loaded. handy for tests.
    #[must_use]
    pub fn with_default() -> Self {
        let mut db = Database::new();
        db.load_font_data(BUNDLED_DEJAVU.to_vec());
        Self { db }
    }

    /// resolve `family` to a face id. checks the family verbatim and falls
    /// back to the first DejaVu-Sans face when nothing matches.
    fn select(&self, family: &str) -> Result<ID, FontError> {
        let q = Query {
            families: &[Family::Name(family), Family::Name("DejaVu Sans"), Family::SansSerif],
            weight: Weight::NORMAL,
            stretch: Stretch::Normal,
            style: FontStyle::Normal,
        };
        self.db
            .query(&q)
            .ok_or_else(|| FontError::FamilyNotFound(family.to_owned()))
    }

    /// resolve `id` to an Arc-backed byte source. for `Source::Binary` this
    /// is the Arc fontdb already holds - no copy. for file-backed sources
    /// (loaded via `load_fonts_dir`) read once and wrapped.
    fn face_bytes(&self, id: ID) -> Result<(FaceBytes, u32), FontError> {
        let (src, index) = self
            .db
            .face_source(id)
            .ok_or_else(|| FontError::FaceLoad(format!("face id {id:?}")))?;
        let arc: FaceBytes = match src {
            Source::Binary(a) => a,
            Source::File(path) => {
                let data = std::fs::read(&path)?;
                Arc::new(data)
            }
        };
        Ok((arc, index))
    }
}

/// shaped, single-line glyph run in pixel space relative to a baseline anchor.
#[derive(Debug, Clone)]
pub struct ShapedRun {
    glyphs: Vec<ShapedGlyph>,
    /// total horizontal advance of the run, pixels.
    pub advance_x: f32,
    /// font ascent (positive, pixels above baseline).
    pub ascent: f32,
    /// font descent (positive, pixels below baseline).
    pub descent: f32,
    face: Arc<FaceHandle>,
    pixels_per_unit: f32,
}

#[derive(Debug, Clone, Copy)]
struct ShapedGlyph {
    glyph_id: u16,
    /// pixel offset from run origin to glyph origin.
    x: f32,
    y: f32,
    /// horizontal advance of this glyph, pixels.
    advance_x: f32,
}

/// per-glyph layout exposed to callers walking a [`ShapedRun`] (e.g. a
/// FOLLOW renderer that places each glyph along a curve). values are in
/// the same pixel-space coordinate system as the run's metrics.
#[derive(Debug, Clone, Copy)]
pub struct GlyphLayout {
    /// glyph id within the resolved font face.
    pub glyph_id: u16,
    /// pixel offset from the run origin to this glyph's origin.
    pub x: f32,
    /// pixel offset from the run origin to this glyph's origin.
    pub y: f32,
    /// horizontal advance of this glyph, pixels.
    pub advance_x: f32,
}

struct FaceHandle {
    /// shared ttf bytes so the face outlives the database query without
    /// re-copying the buffer on every measure() call.
    data: FaceBytes,
    index: u32,
}

impl std::fmt::Debug for FaceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FaceHandle")
            .field("len", &self.data.as_ref().as_ref().len())
            .field("index", &self.index)
            .finish()
    }
}

/// shape a single line of text. returns a [`ShapedRun`] in pixel space.
///
/// rustybuzz is invoked once with default features. any returned cluster /
/// glyph offsets are converted from font units to pixels using
/// `style.font_size / units_per_em`.
pub fn measure(text: &str, style: &LabelStyle, fonts: &Fonts) -> Result<ShapedRun, FontError> {
    let id = fonts.select(&style.font_family)?;
    let (data, index) = fonts.face_bytes(id)?;

    let face_for_shape = rustybuzz::Face::from_slice(data.as_ref().as_ref(), index)
        .ok_or_else(|| FontError::FaceParse(style.font_family.clone()))?;
    let upem = face_for_shape.units_per_em() as f32;
    let pixels_per_unit = style.font_size / upem;
    let ascent = f32::from(face_for_shape.ascender()) * pixels_per_unit;
    let descent = -f32::from(face_for_shape.descender()) * pixels_per_unit;

    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(text);
    buffer.guess_segment_properties();
    let glyph_buffer = rustybuzz::shape(&face_for_shape, &[], buffer);

    let infos = glyph_buffer.glyph_infos();
    let positions = glyph_buffer.glyph_positions();
    let mut cursor_x = 0.0f32;
    let mut cursor_y = 0.0f32;
    let mut glyphs = Vec::with_capacity(infos.len());
    for (info, pos) in infos.iter().zip(positions.iter()) {
        let glyph_x = cursor_x + pos.x_offset as f32 * pixels_per_unit;
        let glyph_y = cursor_y + pos.y_offset as f32 * pixels_per_unit;
        let advance_x = pos.x_advance as f32 * pixels_per_unit;
        glyphs.push(ShapedGlyph {
            glyph_id: info.glyph_id as u16,
            x: glyph_x,
            y: glyph_y,
            advance_x,
        });
        cursor_x += advance_x;
        cursor_y += pos.y_advance as f32 * pixels_per_unit;
    }

    Ok(ShapedRun {
        glyphs,
        advance_x: cursor_x,
        ascent,
        descent,
        face: Arc::new(FaceHandle { data, index }),
        pixels_per_unit,
    })
}

impl ShapedRun {
    /// Borrow the per-glyph layout produced by shaping. one entry per
    /// post-shaping glyph (clusters may differ in count from the source
    /// `&str` for ligatures / complex scripts).
    pub fn glyphs(&self) -> impl Iterator<Item = GlyphLayout> + '_ {
        self.glyphs.iter().map(|g| GlyphLayout {
            glyph_id: g.glyph_id,
            x: g.x,
            y: g.y,
            advance_x: g.advance_x,
        })
    }

    /// number of shaped glyphs.
    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.glyphs.len()
    }
}

/// rasterised glyph mask covering an entire shaped run.
///
/// `coverage` is a row-major u8 alpha buffer. `(origin_x, origin_y)` is the
/// pixel offset from the run's baseline anchor to the mask's `(0, 0)` cell;
/// most runs have negative `origin_y` (mask top is above baseline) and a
/// small positive `origin_x` from glyph side bearings.
#[derive(Debug, Clone)]
pub struct GlyphMask {
    /// mask width in pixels.
    pub width: u32,
    /// mask height in pixels.
    pub height: u32,
    /// x offset from baseline anchor to mask top-left.
    pub origin_x: i32,
    /// y offset from baseline anchor to mask top-left.
    pub origin_y: i32,
    /// row-major u8 coverage.
    pub coverage: Vec<u8>,
}

/// rasterise a shaped run into a single coverage mask. caller composites the
/// fill / halo colours and pastes into the target pixmap.
///
/// uses tiny-skia internally as a software outline rasteriser. the resulting
/// mask is tightly cropped around the union of glyph bounding boxes.
pub fn rasterise(run: &ShapedRun) -> Result<GlyphMask, FontError> {
    let face = ttf_parser::Face::parse(run.face.data.as_ref().as_ref(), run.face.index)
        .map_err(|e| FontError::FaceParse(format!("{e:?}")))?;

    // first pass: union of glyph bounding boxes in pixel space.
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    let mut have_box = false;
    for g in &run.glyphs {
        let gid = ttf_parser::GlyphId(g.glyph_id);
        let Some(bbox) = face.glyph_bounding_box(gid) else {
            continue;
        };
        let x0 = g.x + f32::from(bbox.x_min) * run.pixels_per_unit;
        let x1 = g.x + f32::from(bbox.x_max) * run.pixels_per_unit;
        // glyph y in font units grows up; pixel y grows down. invert here.
        let y0 = g.y - f32::from(bbox.y_max) * run.pixels_per_unit;
        let y1 = g.y - f32::from(bbox.y_min) * run.pixels_per_unit;
        if x0 < min_x {
            min_x = x0;
        }
        if y0 < min_y {
            min_y = y0;
        }
        if x1 > max_x {
            max_x = x1;
        }
        if y1 > max_y {
            max_y = y1;
        }
        have_box = true;
    }
    if !have_box {
        return Ok(GlyphMask {
            width: 0,
            height: 0,
            origin_x: 0,
            origin_y: 0,
            coverage: Vec::new(),
        });
    }

    // pad by 1 pixel on each side for AA tails.
    let pad = 1.0f32;
    let mask_x0 = (min_x - pad).floor() as i32;
    let mask_y0 = (min_y - pad).floor() as i32;
    let mask_x1 = (max_x + pad).ceil() as i32;
    let mask_y1 = (max_y + pad).ceil() as i32;
    let width = (mask_x1 - mask_x0).max(1) as u32;
    let height = (mask_y1 - mask_y0).max(1) as u32;

    let mut pm = Pixmap::new(width, height).ok_or_else(|| FontError::FaceParse(format!("pixmap {width}x{height}")))?;

    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 255);
    paint.anti_alias = true;

    for g in &run.glyphs {
        let gid = ttf_parser::GlyphId(g.glyph_id);
        let mut builder = PathBuilderAdapter {
            inner: PathBuilder::new(),
            scale: run.pixels_per_unit,
            // glyph (0,0) in pixel space, before mask offset.
            x: g.x - mask_x0 as f32,
            y: g.y - mask_y0 as f32,
        };
        if face.outline_glyph(gid, &mut builder).is_none() {
            continue;
        }
        let Some(path) = builder.inner.finish() else { continue };
        pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }

    // extract alpha channel only.
    let coverage: Vec<u8> = pm.data().chunks_exact(4).map(|p| p[3]).collect();
    Ok(GlyphMask {
        width,
        height,
        origin_x: mask_x0,
        origin_y: mask_y0,
        coverage,
    })
}

/// rasterise a single glyph from a shaped run. the returned mask is tightly
/// cropped to the glyph's bounding box, with `origin_x` / `origin_y` giving
/// the pixel offset from the glyph's own origin (not the run origin) to the
/// mask's top-left.
///
/// FOLLOW labels use this to stamp glyphs individually along a curve, each
/// rotated to its own local tangent.
pub fn rasterise_glyph(run: &ShapedRun, glyph_idx: usize) -> Result<GlyphMask, FontError> {
    let g = match run.glyphs.get(glyph_idx) {
        Some(g) => *g,
        None => {
            return Ok(GlyphMask {
                width: 0,
                height: 0,
                origin_x: 0,
                origin_y: 0,
                coverage: Vec::new(),
            });
        }
    };
    let face = ttf_parser::Face::parse(run.face.data.as_ref().as_ref(), run.face.index)
        .map_err(|e| FontError::FaceParse(format!("{e:?}")))?;
    let gid = ttf_parser::GlyphId(g.glyph_id);
    let Some(bbox) = face.glyph_bounding_box(gid) else {
        return Ok(GlyphMask {
            width: 0,
            height: 0,
            origin_x: 0,
            origin_y: 0,
            coverage: Vec::new(),
        });
    };

    // glyph-local pixel-space bbox; pad by 1 pixel for AA tails.
    let pad = 1.0f32;
    let x0_p = f32::from(bbox.x_min) * run.pixels_per_unit;
    let x1_p = f32::from(bbox.x_max) * run.pixels_per_unit;
    let y0_p = -f32::from(bbox.y_max) * run.pixels_per_unit;
    let y1_p = -f32::from(bbox.y_min) * run.pixels_per_unit;
    let mask_x0 = (x0_p - pad).floor() as i32;
    let mask_y0 = (y0_p - pad).floor() as i32;
    let mask_x1 = (x1_p + pad).ceil() as i32;
    let mask_y1 = (y1_p + pad).ceil() as i32;
    let width = (mask_x1 - mask_x0).max(1) as u32;
    let height = (mask_y1 - mask_y0).max(1) as u32;

    let mut pm = Pixmap::new(width, height).ok_or_else(|| FontError::FaceParse(format!("pixmap {width}x{height}")))?;
    let mut paint = Paint::default();
    paint.set_color_rgba8(255, 255, 255, 255);
    paint.anti_alias = true;

    let mut builder = PathBuilderAdapter {
        inner: PathBuilder::new(),
        scale: run.pixels_per_unit,
        x: -mask_x0 as f32,
        y: -mask_y0 as f32,
    };
    if face.outline_glyph(gid, &mut builder).is_none() {
        return Ok(GlyphMask {
            width: 0,
            height: 0,
            origin_x: 0,
            origin_y: 0,
            coverage: Vec::new(),
        });
    }
    let Some(path) = builder.inner.finish() else {
        return Ok(GlyphMask {
            width: 0,
            height: 0,
            origin_x: 0,
            origin_y: 0,
            coverage: Vec::new(),
        });
    };
    pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    let coverage: Vec<u8> = pm.data().chunks_exact(4).map(|p| p[3]).collect();
    Ok(GlyphMask {
        width,
        height,
        origin_x: mask_x0,
        origin_y: mask_y0,
        coverage,
    })
}

struct PathBuilderAdapter {
    inner: PathBuilder,
    scale: f32,
    x: f32,
    y: f32,
}

impl ttf_parser::OutlineBuilder for PathBuilderAdapter {
    fn move_to(&mut self, x: f32, y: f32) {
        self.inner.move_to(self.x + x * self.scale, self.y - y * self.scale);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.inner.line_to(self.x + x * self.scale, self.y - y * self.scale);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        self.inner.quad_to(
            self.x + x1 * self.scale,
            self.y - y1 * self.scale,
            self.x + x * self.scale,
            self.y - y * self.scale,
        );
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        self.inner.cubic_to(
            self.x + x1 * self.scale,
            self.y - y1 * self.scale,
            self.x + x2 * self.scale,
            self.y - y2 * self.scale,
            self.x + x * self.scale,
            self.y - y * self.scale,
        );
    }
    fn close(&mut self) {
        self.inner.close();
    }
}

// re-export rustybuzz's bundled ttf-parser so the renderer adapter can
// share a single semver line.
pub use rustybuzz::ttf_parser;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use mars_style::Colour;

    use super::*;

    fn lbl(family: &str, size: f32) -> LabelStyle {
        LabelStyle {
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
        assert!(
            (sum - run.advance_x).abs() < 1e-3,
            "sum {sum} vs run {}",
            run.advance_x
        );
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
}
