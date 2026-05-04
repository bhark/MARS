//! image encoding. PNG only at Phase 0; JPEG deferred to Phase 1.

use mars_render_port::RenderError;
use tiny_skia::Pixmap;

pub(crate) fn encode_png(pm: &Pixmap) -> Result<Vec<u8>, RenderError> {
    let mut out = Vec::with_capacity(pm.data().len() / 2);
    {
        let mut enc = png::Encoder::new(&mut out, pm.width(), pm.height());
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Default);
        let mut writer = enc
            .write_header()
            .map_err(|e| RenderError::Encode(format!("png header: {e}")))?;
        // tiny-skia stores premultiplied rgba; demultiply for spec-correct png.
        let demul = demultiply(pm.data());
        writer
            .write_image_data(&demul)
            .map_err(|e| RenderError::Encode(format!("png write: {e}")))?;
    }
    Ok(out)
}

fn demultiply(premul: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(premul.len());
    for px in premul.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            out.extend_from_slice(&[r, g, b, a]);
        } else {
            let inv = 255.0 / a as f32;
            out.push(((r as f32 * inv).round().min(255.0)) as u8);
            out.push(((g as f32 * inv).round().min(255.0)) as u8);
            out.push(((b as f32 * inv).round().min(255.0)) as u8);
            out.push(a);
        }
    }
    out
}
