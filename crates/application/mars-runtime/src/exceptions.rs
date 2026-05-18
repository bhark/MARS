//! WMS EXCEPTIONS-mode image producers.
//!
//! `EXCEPTIONS=BLANK` and `EXCEPTIONS=INIMAGE` need a well-formed image
//! response even when no manifest is loaded, so these helpers bypass
//! `RuntimeState` and operate purely on the dep set + plan. The
//! corresponding `Runtime::blank_image` / `Runtime::inimage_error`
//! methods are thin shims over the free fns here.

use std::sync::Arc;

use mars_render_port::{Canvas, DrawOp, Pixmap};

use crate::{Deps, RenderPlan, RuntimeError};

/// Encode a fully-transparent image of the plan's dimensions and format.
pub(crate) fn blank_image_bytes(deps: &Deps, pixel_budget: u32, plan: &RenderPlan) -> Result<Vec<u8>, RuntimeError> {
    let pixels = (plan.width as usize)
        .checked_mul(plan.height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(RuntimeError::PixelBudgetExceeded {
            requested: u64::from(plan.width).saturating_mul(u64::from(plan.height)),
            budget: pixel_budget,
        })?;
    let pixmap = Pixmap {
        width: plan.width,
        height: plan.height,
        premultiplied_rgba: vec![0u8; pixels],
    };
    Ok(deps.encoder.encode(&pixmap, plan.format)?)
}

/// Render an error message as text centred on a transparent image of the
/// plan's dimensions and format.
pub(crate) fn inimage_error_bytes(
    deps: &Deps,
    pixel_budget: u32,
    plan: &RenderPlan,
    message: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let pixels = u64::from(plan.width).saturating_mul(u64::from(plan.height));
    if pixels > u64::from(pixel_budget) {
        return Err(RuntimeError::PixelBudgetExceeded {
            requested: pixels,
            budget: pixel_budget,
        });
    }
    // very small canvases cannot fit even one glyph at the chosen size;
    // fall through to a blank image so the response stays well-formed.
    if plan.width < 16 || plan.height < 16 {
        return blank_image_bytes(deps, pixel_budget, plan);
    }
    let style = Arc::new(
        mars_style::LabelStyle {
            font_family: "DejaVu Sans".to_owned(),
            font_size: 14.0.into(),
            fill: mars_style::Colour::rgba(180, 20, 20, 255),
            halo: Some(mars_style::Halo {
                colour: mars_style::Colour::rgba(255, 255, 255, 230),
                width: 1.5,
            }),
            priority: 0,
            min_distance: 0.0,
            position: mars_style::AnchorPosition::default(),
            offset_px: (0.0, 0.0),
            angle: None,
            partials: false,
            force: false,
        }
        .resolve(0),
    );
    // single-line truncation; multi-line wrap belongs in the label
    // renderer, not here.
    let text = truncate_message(message, 80);
    let anchor = (plan.width as f32 / 2.0, plan.height as f32 / 2.0);
    let ops = [DrawOp::Label {
        anchor,
        text,
        style,
        angle_rad: 0.0,
    }];
    let canvas = Canvas {
        width: plan.width,
        height: plan.height,
        background: None,
    };
    let pixmap = deps.renderer.render(canvas, &ops)?;
    Ok(deps.encoder.encode(&pixmap, plan.format)?)
}

/// Trim `msg` to at most `max_chars` characters (not bytes), appending an
/// ellipsis when truncated. Operates on `char` boundaries so the result is
/// valid UTF-8 regardless of input.
fn truncate_message(msg: &str, max_chars: usize) -> String {
    let chars: Vec<char> = msg.chars().collect();
    if chars.len() <= max_chars {
        return chars.into_iter().collect();
    }
    let mut out: String = chars.into_iter().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests;
