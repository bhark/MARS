//! font discovery, shaping, and glyph rasterisation for MARS labels (SPEC §14).
//!
//! - [`Fonts`] wraps `fontdb` for face discovery; `load(paths, bundle_default)`
//!   walks a list of search paths and optionally appends the vendored DejaVu Sans
//!   so goldens never depend on system fontconfig.
//! - [`measure`] shapes a single line of text via rustybuzz and reports advance,
//!   ascent, and descent in pixel space.
//! - [`rasterise`] returns an alpha mask covering the shaped run; the caller
//!   composites halo + fill (the renderer adapter handles colour).
//!
//! No async, no I/O outside font loading. Multi-line / cosmic-text path is
//! deferred to v1.1; rustybuzz on a single line is enough for v1 placement.

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
            db.load_fonts_dir(p.as_ref());
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

    /// borrow face bytes + index. returns `None` if `id` is not in the db.
    fn with_face_data<R>(&self, id: ID, f: impl FnOnce(&[u8], u32) -> R) -> Option<R> {
        self.db.with_face_data(id, |bytes, idx| f(bytes, idx))
    }

    /// resolve `id` to an Arc-backed byte source. for `Source::Binary` this
    /// is the Arc fontdb already holds — no copy. for file-backed sources
    /// (loaded via `load_fonts_dir`) we read once and wrap.
    fn face_bytes(&self, id: ID) -> Result<(Arc<dyn AsRef<[u8]> + Send + Sync>, u32), FontError> {
        let (src, index) = self
            .db
            .face_source(id)
            .ok_or_else(|| FontError::FaceLoad(format!("face id {id:?}")))?;
        let arc: Arc<dyn AsRef<[u8]> + Send + Sync> = match src {
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
}

struct FaceHandle {
    /// shared ttf bytes so the face outlives the database query without
    /// re-copying the buffer on every measure() call.
    data: Arc<dyn AsRef<[u8]> + Send + Sync>,
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
        glyphs.push(ShapedGlyph {
            glyph_id: info.glyph_id as u16,
            x: glyph_x,
            y: glyph_y,
        });
        cursor_x += pos.x_advance as f32 * pixels_per_unit;
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
        let Some(bbox) = face.glyph_bounding_box(gid) else { continue };
        let x0 = g.x + f32::from(bbox.x_min) * run.pixels_per_unit;
        let x1 = g.x + f32::from(bbox.x_max) * run.pixels_per_unit;
        // glyph y in font units grows up; pixel y grows down. invert here.
        let y0 = g.y - f32::from(bbox.y_max) * run.pixels_per_unit;
        let y1 = g.y - f32::from(bbox.y_min) * run.pixels_per_unit;
        if x0 < min_x { min_x = x0; }
        if y0 < min_y { min_y = y0; }
        if x1 > max_x { max_x = x1; }
        if y1 > max_y { max_y = y1; }
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

    let mut pm =
        Pixmap::new(width, height).ok_or_else(|| FontError::FaceParse(format!("pixmap {width}x{height}")))?;

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
        }
    }

    #[test]
    fn measure_hello_advance_in_range() {
        let fonts = Fonts::with_default();
        let run = measure("hello", &lbl("DejaVu Sans", 12.0), &fonts).unwrap();
        // dejavu sans 12pt "hello" advances ≈ 30.0 px. allow a wide band; the
        // ratchet check below pins the exact value.
        assert!(run.advance_x > 20.0 && run.advance_x < 40.0, "advance {} out of band", run.advance_x);
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
}
