//! shared helpers for image-diff tests. included via `mod common;` from
//! integration tests; not compiled as a standalone test target.

#![allow(dead_code, unreachable_pub)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fmt;

#[derive(Debug, Clone)]
pub struct DiffReport {
    pub width: u32,
    pub height: u32,
    pub total_pixels: u32,
    pub differing_pixels: u32,
    pub max_channel_delta: u8,
}

impl DiffReport {
    pub fn diff_ratio(&self) -> f32 {
        if self.total_pixels == 0 {
            0.0
        } else {
            self.differing_pixels as f32 / self.total_pixels as f32
        }
    }
}

impl fmt::Display for DiffReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DiffReport {{ {}x{} px, differing={}/{} ({:.4}%), max_channel_delta={} }}",
            self.width,
            self.height,
            self.differing_pixels,
            self.total_pixels,
            self.diff_ratio() * 100.0,
            self.max_channel_delta,
        )
    }
}

#[derive(Debug)]
pub enum DiffError {
    DecodeActual(png::DecodingError),
    DecodeGolden(png::DecodingError),
    DimensionMismatch {
        aw: u32,
        ah: u32,
        gw: u32,
        gh: u32,
    },
    FormatMismatch {
        a: png::ColorType,
        abd: u8,
        g: png::ColorType,
        gbd: u8,
    },
    UnsupportedColor(png::ColorType),
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DecodeActual(e) => write!(f, "decode actual png: {e}"),
            Self::DecodeGolden(e) => write!(f, "decode golden png: {e}"),
            Self::DimensionMismatch { aw, ah, gw, gh } => {
                write!(f, "dimension mismatch: actual {aw}x{ah}, golden {gw}x{gh}")
            }
            Self::FormatMismatch { a, abd, g, gbd } => write!(
                f,
                "color/format mismatch: actual {a:?} {abd}-bit, golden {g:?} {gbd}-bit"
            ),
            Self::UnsupportedColor(c) => write!(f, "unsupported color type: {c:?}"),
        }
    }
}

impl std::error::Error for DiffError {}

struct Decoded {
    width: u32,
    height: u32,
    color: png::ColorType,
    bit_depth: u8,
    pixels: Vec<u8>,
}

fn decode(bytes: &[u8]) -> Result<Decoded, png::DecodingError> {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info()?;
    let info = reader.info().clone();
    let mut buf = vec![0u8; reader.output_buffer_size().unwrap_or(0)];
    let frame = reader.next_frame(&mut buf)?;
    buf.truncate(frame.buffer_size());
    Ok(Decoded {
        width: info.width,
        height: info.height,
        color: info.color_type,
        bit_depth: info.bit_depth as u8,
        pixels: buf,
    })
}

/// pixel-by-pixel comparison. tolerance is the per-channel delta beyond which a
/// pixel is counted as differing (any channel exceeding `tolerance` => differs).
pub fn diff_pngs(actual: &[u8], golden: &[u8], tolerance: u8) -> Result<DiffReport, DiffError> {
    let a = decode(actual).map_err(DiffError::DecodeActual)?;
    let g = decode(golden).map_err(DiffError::DecodeGolden)?;

    if a.width != g.width || a.height != g.height {
        return Err(DiffError::DimensionMismatch {
            aw: a.width,
            ah: a.height,
            gw: g.width,
            gh: g.height,
        });
    }
    if a.color != g.color || a.bit_depth != g.bit_depth {
        return Err(DiffError::FormatMismatch {
            a: a.color,
            abd: a.bit_depth,
            g: g.color,
            gbd: g.bit_depth,
        });
    }

    let stride = match a.color {
        png::ColorType::Rgb => 3usize,
        png::ColorType::Rgba => 4usize,
        other => return Err(DiffError::UnsupportedColor(other)),
    };

    let total_pixels = a.width.saturating_mul(a.height);
    let mut differing: u32 = 0;
    let mut max_delta: u8 = 0;

    for (pa, pg) in a.pixels.chunks_exact(stride).zip(g.pixels.chunks_exact(stride)) {
        let mut pixel_max: u8 = 0;
        for c in 0..stride {
            let d = pa[c].abs_diff(pg[c]);
            if d > pixel_max {
                pixel_max = d;
            }
        }
        if pixel_max > max_delta {
            max_delta = pixel_max;
        }
        if pixel_max > tolerance {
            differing = differing.saturating_add(1);
        }
    }

    Ok(DiffReport {
        width: a.width,
        height: a.height,
        total_pixels,
        differing_pixels: differing,
        max_channel_delta: max_delta,
    })
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
