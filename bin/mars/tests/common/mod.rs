//! shared helpers for image-diff tests. included via `mod common;` from
//! integration tests; not compiled as a standalone test target.

#![allow(dead_code, unreachable_pub)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[cfg(feature = "mapserver-diff")]
pub mod perf_report;

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
    DimensionMismatch {
        aw: u32,
        ah: u32,
        gw: u32,
        gh: u32,
    },
    FormatMismatch {
        a: Channels,
        g: Channels,
    },
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
    let frame = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("png frame: {e}"))?;
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
    let info = dec
        .info()
        .ok_or_else(|| "jpeg decode produced no info".to_string())?;
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

/// pixel-by-pixel comparison. tolerance is the per-channel delta beyond which a
/// pixel is counted as differing (any channel exceeding `tolerance` => differs).
pub fn diff_pngs(actual: &[u8], golden: &[u8], tolerance: u8) -> Result<DiffReport, DiffError> {
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
