//! x marker. Same silhouette as [`super::cross`] rotated 45° about the
//! origin, so `size` here is the diagonal arm length and arm thickness
//! is `size/3` measured along the rotated frame.

use mars_render_port::Path as PortPath;

pub(crate) fn build_path(size: f32) -> PortPath {
    let mut path = super::cross::build_path(size);
    let c = std::f32::consts::FRAC_1_SQRT_2;
    for sub in &mut path.subpaths {
        for p in &mut sub.points {
            let (x, y) = *p;
            *p = (c * (x - y), c * (x + y));
        }
    }
    path
}

#[cfg(test)]
mod tests;
