//! Thin shim around `mars_artifact::wkb` for the pgoutput translator.
//!
//! Phase G removes this module outright and the translator imports
//! `mars_artifact::wkb` directly. We keep the shim through the rest of
//! Phase C so the translator's call sites and any remaining replication
//! tests do not churn alongside the substrate cut.

pub(crate) use mars_artifact::{wkb_bbox as bbox_of, wkb_centroid as centroid_of};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn point_le(x: f64, y: f64) -> Vec<u8> {
        let mut v = vec![1u8];
        v.extend_from_slice(&1u32.to_le_bytes());
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v
    }

    #[test]
    fn shim_delegates_to_mars_artifact() {
        let bb = bbox_of(&point_le(3.0, 4.0)).unwrap();
        assert_eq!((bb.min_x, bb.max_x), (3.0, 3.0));
        let c = centroid_of(&point_le(7.0, 7.0)).unwrap();
        assert_eq!(c, [7.0, 7.0]);
    }

    #[test]
    fn shim_propagates_errors() {
        // truncated wkb is rejected by the canonical walker; the shim must
        // not swallow it.
        let mut v = point_le(0.0, 0.0);
        v.truncate(v.len() - 4);
        assert!(matches!(bbox_of(&v), Err(mars_artifact::WkbError::Truncated)));
    }
}
