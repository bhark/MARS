//! image-diff helpers shared by the integration suite (`bin/mars/tests/*`) and
//! the kind-based e2e suite (`tests/e2e/`). neighborhood-tolerant counting
//! forgives sub-pixel anti-alias jitter on thin features without hiding real
//! divergence.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt;

#[derive(Debug, Clone)]
pub struct DiffReport {
    pub width: u32,
    pub height: u32,
    pub total_pixels: u32,
    /// strict per-pixel-position differing count (no neighborhood relaxation).
    /// kept for visibility into the raw signal.
    pub strict_differing: u32,
    /// neighborhood-tolerant count: a strict-differing pixel only counts here
    /// if neither side has a within-tolerance match in the other image's
    /// `radius` window. forgives sub-pixel anti-alias jitter on thin features.
    pub differing_pixels: u32,
    pub max_channel_delta: u8,
    pub radius: u32,
}

impl DiffReport {
    pub fn diff_ratio(&self) -> f32 {
        if self.total_pixels == 0 {
            0.0
        } else {
            self.differing_pixels as f32 / self.total_pixels as f32
        }
    }

    pub fn strict_diff_ratio(&self) -> f32 {
        if self.total_pixels == 0 {
            0.0
        } else {
            self.strict_differing as f32 / self.total_pixels as f32
        }
    }
}

impl fmt::Display for DiffReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DiffReport {{ {}x{} px, differing={}/{} ({:.4}%), strict={} ({:.4}%), max_channel_delta={}, r={} }}",
            self.width,
            self.height,
            self.differing_pixels,
            self.total_pixels,
            self.diff_ratio() * 100.0,
            self.strict_differing,
            self.strict_diff_ratio() * 100.0,
            self.max_channel_delta,
            self.radius,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channels {
    Rgb,
    Rgba,
}

impl Channels {
    pub fn stride(self) -> usize {
        match self {
            Self::Rgb => 3,
            Self::Rgba => 4,
        }
    }
}

impl fmt::Display for Channels {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rgb => f.write_str("rgb8"),
            Self::Rgba => f.write_str("rgba8"),
        }
    }
}

#[derive(Debug)]
pub enum DiffError {
    DecodeActual(String),
    DecodeGolden(String),
    DimensionMismatch { aw: u32, ah: u32, gw: u32, gh: u32 },
    FormatMismatch { a: Channels, g: Channels },
    UnsupportedColor(String),
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecodeActual(e) => write!(f, "decode actual: {e}"),
            Self::DecodeGolden(e) => write!(f, "decode golden: {e}"),
            Self::DimensionMismatch { aw, ah, gw, gh } => {
                write!(f, "dimension mismatch: actual {aw}x{ah}, golden {gw}x{gh}")
            }
            Self::FormatMismatch { a, g } => {
                write!(f, "color/format mismatch: actual {a}, golden {g}")
            }
            Self::UnsupportedColor(c) => write!(f, "unsupported color type: {c}"),
        }
    }
}

impl std::error::Error for DiffError {}

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub channels: Channels,
    pub pixels: Vec<u8>,
}

/// detect image format from magic bytes. capture writes server bytes verbatim,
/// so a `.png` filename may actually be jpeg.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpeg")
    } else {
        None
    }
}

pub fn decode(bytes: &[u8]) -> Result<Decoded, DiffError> {
    decode_inner(bytes).map_err(DiffError::DecodeActual)
}

fn decode_inner(bytes: &[u8]) -> Result<Decoded, String> {
    match sniff(bytes) {
        Some("png") => decode_png(bytes),
        Some("jpeg") => decode_jpeg(bytes),
        _ => Err("unrecognized image format (not png or jpeg)".into()),
    }
}

fn decode_png(bytes: &[u8]) -> Result<Decoded, String> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().map_err(|e| format!("png header: {e}"))?;
    let info = reader.info().clone();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader.next_frame(&mut buf).map_err(|e| format!("png frame: {e}"))?;
    buf.truncate(frame.buffer_size());
    if info.bit_depth as u8 != 8 {
        return Err(format!("unsupported png bit depth: {:?}", info.bit_depth));
    }
    let channels = match info.color_type {
        png::ColorType::Rgb => Channels::Rgb,
        png::ColorType::Rgba => Channels::Rgba,
        other => return Err(format!("unsupported png color type: {other:?}")),
    };
    Ok(Decoded {
        width: info.width,
        height: info.height,
        channels,
        pixels: buf,
    })
}

fn decode_jpeg(bytes: &[u8]) -> Result<Decoded, String> {
    let mut dec = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(bytes));
    let pixels = dec.decode().map_err(|e| format!("jpeg decode: {e}"))?;
    let info = dec.info().ok_or_else(|| "jpeg decode produced no info".to_string())?;
    let cs = dec.output_colorspace();
    let channels = match cs {
        Some(zune_jpeg::zune_core::colorspace::ColorSpace::RGB) => Channels::Rgb,
        Some(zune_jpeg::zune_core::colorspace::ColorSpace::RGBA) => Channels::Rgba,
        other => return Err(format!("unsupported jpeg colorspace: {other:?}")),
    };
    Ok(Decoded {
        width: info.width as u32,
        height: info.height as u32,
        channels,
        pixels,
    })
}

/// per-channel delta beyond which a pixel position is counted as strictly
/// differing. neighborhood relaxation (radius `r`, default 1, override via
/// `MARS_DIFF_RADIUS`) then forgives any strict differ that has a within-
/// tolerance match in the other image's (2r+1)x(2r+1) window - kills sub-
/// pixel anti-alias jitter on thin features without hiding real divergence.
pub fn diff_pngs(actual: &[u8], golden: &[u8], tolerance: u8) -> Result<DiffReport, DiffError> {
    let radius = std::env::var("MARS_DIFF_RADIUS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(1);
    diff_pngs_with_radius(actual, golden, tolerance, radius)
}

pub fn diff_pngs_with_radius(
    actual: &[u8],
    golden: &[u8],
    tolerance: u8,
    radius: u32,
) -> Result<DiffReport, DiffError> {
    let a = decode_inner(actual).map_err(DiffError::DecodeActual)?;
    let g = decode_inner(golden).map_err(DiffError::DecodeGolden)?;

    if a.width != g.width || a.height != g.height {
        return Err(DiffError::DimensionMismatch {
            aw: a.width,
            ah: a.height,
            gw: g.width,
            gh: g.height,
        });
    }
    if a.channels != g.channels {
        return Err(DiffError::FormatMismatch {
            a: a.channels,
            g: g.channels,
        });
    }

    let stride = a.channels.stride();
    let width = a.width;
    let height = a.height;
    let total_pixels = width.saturating_mul(height);

    // strict pass
    let mut strict_mask = vec![false; (width as usize) * (height as usize)];
    let mut strict_count: u32 = 0;
    let mut max_delta: u8 = 0;
    for (i, (pa, pg)) in a
        .pixels
        .chunks_exact(stride)
        .zip(g.pixels.chunks_exact(stride))
        .enumerate()
    {
        let pm = pixel_max_delta(pa, pg);
        if pm > max_delta {
            max_delta = pm;
        }
        if pm > tolerance {
            strict_mask[i] = true;
            strict_count = strict_count.saturating_add(1);
        }
    }

    // neighborhood pass: forgive any strict differ that has a tolerance-match
    // in the other image within `radius`. symmetric in both directions so
    // jitter is forgiven only when *both* sides have the same feature nearby.
    let differing = if radius == 0 || strict_count == 0 {
        strict_count
    } else {
        let mut count: u32 = 0;
        for y in 0..height {
            for x in 0..width {
                let idx = (y as usize) * (width as usize) + x as usize;
                if !strict_mask[idx] {
                    continue;
                }
                let off = idx * stride;
                let pa = &a.pixels[off..off + stride];
                let pg = &g.pixels[off..off + stride];
                let a_match_in_g =
                    neighborhood_has_match(pa, &g.pixels, width, height, x, y, radius, stride, tolerance);
                let g_match_in_a =
                    neighborhood_has_match(pg, &a.pixels, width, height, x, y, radius, stride, tolerance);
                if !(a_match_in_g && g_match_in_a) {
                    count = count.saturating_add(1);
                }
            }
        }
        count
    };

    Ok(DiffReport {
        width,
        height,
        total_pixels,
        strict_differing: strict_count,
        differing_pixels: differing,
        max_channel_delta: max_delta,
        radius,
    })
}

#[inline]
fn pixel_max_delta(a: &[u8], b: &[u8]) -> u8 {
    let mut m: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        let d = x.abs_diff(*y);
        if d > m {
            m = d;
        }
    }
    m
}

#[allow(clippy::too_many_arguments)]
fn neighborhood_has_match(
    probe: &[u8],
    other: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    radius: u32,
    stride: usize,
    tolerance: u8,
) -> bool {
    let x0 = x.saturating_sub(radius);
    let y0 = y.saturating_sub(radius);
    let x1 = (x + radius).min(width - 1);
    let y1 = (y + radius).min(height - 1);
    for ny in y0..=y1 {
        for nx in x0..=x1 {
            let off = ((ny as usize) * (width as usize) + nx as usize) * stride;
            let q = &other[off..off + stride];
            if pixel_max_delta(probe, q) <= tolerance {
                return true;
            }
        }
    }
    false
}

/// asserts the diff ratio is within bounds; panics with the full report on
/// failure so flaky goldens are diagnosable from CI logs.
pub fn assert_within_tolerance(report: &DiffReport, max_diff_ratio: f32) {
    let ratio = report.diff_ratio();
    assert!(
        ratio <= max_diff_ratio,
        "image diff exceeds tolerance: ratio={:.6} > {:.6}; {}",
        ratio,
        max_diff_ratio,
        report,
    );
}

#[cfg(test)]
mod tests {
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
}
