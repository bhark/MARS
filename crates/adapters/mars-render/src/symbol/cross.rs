//! cross (+) marker. `size` is the arm length end-to-end; arm thickness
//! is `size/3` (mapserver SYMBOL TYPE VECTOR cross convention). One
//! closed 12-vertex polygon so the even-odd fill rule paints the whole
//! silhouette in a single subpath without hole artefacts.

use mars_render_port::{Path as PortPath, Subpath};

pub(crate) fn build_path(size: f32) -> PortPath {
    let half = size / 2.0;
    let t = size / 6.0; // arm half-thickness
    PortPath {
        subpaths: vec![Subpath {
            // traverse the + silhouette ccw starting at the top-arm's top-left
            points: vec![
                (-t, -half),
                (t, -half),
                (t, -t),
                (half, -t),
                (half, t),
                (t, t),
                (t, half),
                (-t, half),
                (-t, t),
                (-half, t),
                (-half, -t),
                (-t, -t),
            ],
            closed: true,
        }],
    }
}

#[cfg(test)]
mod tests;
