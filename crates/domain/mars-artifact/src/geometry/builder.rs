use bytes::Bytes;

use crate::ArtifactError;

use super::{
    codec::{quantize, write_geom, geom_type_byte},
    FeatureGeom, GeomType, FEATURE_INDEX_ENTRY_LEN,
};

/// encode features into the geometry-payload section bytes.
///
/// Caller is responsible for the deterministic feature ordering — the encoder
/// trusts the input order (the compiler computes a stable
/// `(hilbert_key, user_id, row_fingerprint)` tuple before calling). `user_id`
/// is permitted to repeat: it is data, not a key. Position-in-input becomes
/// the substrate primary key (`feature_idx`).
pub fn encode_geometry_payload(features: &[FeatureGeom]) -> Result<Bytes, ArtifactError> {
    let mut b = GeomPayloadBuilder::new();
    for f in features {
        b.push_feature(f)?;
    }
    b.finish()
}

/// Streaming geometry-payload builder. Lets producers (e.g. a WKB decoder)
/// emit features one coord at a time, avoiding the `Vec<FeatureGeom>` +
/// `Vec<Coord>`-per-ring intermediate that [`encode_geometry_payload`]
/// requires. The on-wire bytes are byte-identical to the bulk encoder for
/// the same logical feature stream.
pub struct GeomPayloadBuilder {
    body: Vec<u8>,
    spans: Vec<(u32, u32)>,
    index: Vec<(u64, [f32; 4], u8)>,
}

/// In-progress feature handed back from [`GeomPayloadBuilder::begin`].
/// Caller drives the format's structure: `count` writes a uvarint sub-count
/// (rings, parts, multi-counts), `reset_ring` restarts delta state at a new
/// ring boundary, `coord_delta` and `coord_abs` push coordinates. `end`
/// commits the feature; dropping without `end` rolls it back.
pub struct FeatureWriter<'a> {
    builder: &'a mut GeomPayloadBuilder,
    geom_type: u8,
    body_start: u32,
    px: i64,
    py: i64,
    have_anchor: bool,
    bbox: BboxAcc,
    user_id: u64,
    ended: bool,
}

#[derive(Default)]
struct BboxAcc {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
    seen: bool,
}

impl BboxAcc {
    fn fold(&mut self, x: f64, y: f64) {
        if !self.seen {
            self.min_x = x;
            self.min_y = y;
            self.max_x = x;
            self.max_y = y;
            self.seen = true;
            return;
        }
        if x < self.min_x {
            self.min_x = x;
        }
        if y < self.min_y {
            self.min_y = y;
        }
        if x > self.max_x {
            self.max_x = x;
        }
        if y > self.max_y {
            self.max_y = y;
        }
    }
    fn snapshot(&self) -> [f32; 4] {
        if !self.seen {
            return [0.0, 0.0, 0.0, 0.0];
        }
        [
            self.min_x as f32,
            self.min_y as f32,
            self.max_x as f32,
            self.max_y as f32,
        ]
    }
}

impl Default for GeomPayloadBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GeomPayloadBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            body: Vec::new(),
            spans: Vec::new(),
            index: Vec::new(),
        }
    }

    /// Start writing a feature with the given user_id and geom type. Returns
    /// a stateful writer the caller drives. The slot index (`feature_idx`,
    /// the per-page primary key) is the position at which the feature is
    /// appended; `user_id` is non-key data and may repeat.
    pub fn begin(&mut self, user_id: u64, geom_type: GeomType) -> Result<FeatureWriter<'_>, ArtifactError> {
        let body_start =
            u32::try_from(self.body.len()).map_err(|_| ArtifactError::Malformed("geometry payload too large"))?;
        Ok(FeatureWriter {
            builder: self,
            geom_type: geom_type.byte(),
            body_start,
            px: 0,
            py: 0,
            have_anchor: false,
            bbox: BboxAcc::default(),
            user_id,
            ended: false,
        })
    }

    /// Convenience: append a fully-formed [`FeatureGeom`]. Used by the bulk
    /// encoder; producers that already have rows materialised may call this
    /// directly.
    pub fn push_feature(&mut self, f: &FeatureGeom) -> Result<(), ArtifactError> {
        let start =
            u32::try_from(self.body.len()).map_err(|_| ArtifactError::Malformed("geometry payload too large"))?;
        write_geom(&mut self.body, &f.geom)?;
        let len = u32::try_from(self.body.len() - start as usize)
            .map_err(|_| ArtifactError::Malformed("geometry section too large"))?;
        self.spans.push((start, len));
        self.index.push((f.user_id, f.bbox, geom_type_byte(&f.geom)));
        Ok(())
    }

    /// Finalise: emit the count + index header followed by the body bytes.
    pub fn finish(self) -> Result<Bytes, ArtifactError> {
        let count = u32::try_from(self.index.len()).map_err(|_| ArtifactError::Malformed("too many features"))?;
        let header_len = 4usize
            .checked_add(
                self.index
                    .len()
                    .checked_mul(FEATURE_INDEX_ENTRY_LEN)
                    .ok_or(ArtifactError::Malformed("geometry payload too large"))?,
            )
            .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
        let total_len = header_len
            .checked_add(self.body.len())
            .ok_or(ArtifactError::Malformed("geometry payload too large"))?;
        let mut out = Vec::with_capacity(total_len);
        out.extend_from_slice(&count.to_le_bytes());
        for ((user_id, bbox, geom_type), (off, len)) in self.index.iter().zip(&self.spans) {
            out.extend_from_slice(&user_id.to_le_bytes());
            for v in bbox {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.push(*geom_type);
            out.extend_from_slice(&off.to_le_bytes());
            out.extend_from_slice(&len.to_le_bytes());
        }
        out.extend_from_slice(&self.body);
        Ok(Bytes::from(out))
    }
}

impl<'a> FeatureWriter<'a> {
    /// Push a uvarint sub-count (ring length, ring count, multi-count).
    pub fn count(&mut self, n: usize) -> Result<(), ArtifactError> {
        let v = u64::try_from(n).map_err(|_| ArtifactError::Malformed("count exceeds u64"))?;
        crate::varint::write_uvarint(&mut self.builder.body, v);
        Ok(())
    }

    /// Reset delta state at a new ring boundary. The next `coord_delta` will
    /// be written as an absolute (zigzag-encoded) ivarint.
    pub fn reset_ring(&mut self) {
        self.have_anchor = false;
        self.px = 0;
        self.py = 0;
    }

    /// Push a coordinate, delta-encoded against the previous coord in the
    /// current ring. The first coord in a ring is written absolute.
    pub fn coord_delta(&mut self, x: f64, y: f64) -> Result<(), ArtifactError> {
        let qx = quantize(x)?;
        let qy = quantize(y)?;
        if !self.have_anchor {
            crate::varint::write_ivarint(&mut self.builder.body, qx);
            crate::varint::write_ivarint(&mut self.builder.body, qy);
            self.px = qx;
            self.py = qy;
            self.have_anchor = true;
        } else {
            let dx = qx.checked_sub(self.px).ok_or(ArtifactError::CoordOutOfRange(x))?;
            let dy = qy.checked_sub(self.py).ok_or(ArtifactError::CoordOutOfRange(y))?;
            crate::varint::write_ivarint(&mut self.builder.body, dx);
            crate::varint::write_ivarint(&mut self.builder.body, dy);
            self.px = qx;
            self.py = qy;
        }
        self.bbox.fold(x, y);
        Ok(())
    }

    /// Push an absolute (non-delta) coordinate. Used by Point and MultiPoint
    /// per the on-wire layout.
    pub fn coord_abs(&mut self, x: f64, y: f64) -> Result<(), ArtifactError> {
        let qx = quantize(x)?;
        let qy = quantize(y)?;
        crate::varint::write_ivarint(&mut self.builder.body, qx);
        crate::varint::write_ivarint(&mut self.builder.body, qy);
        self.bbox.fold(x, y);
        Ok(())
    }

    /// Commit this feature to the index.
    pub fn end(mut self) -> Result<(), ArtifactError> {
        let body_end = u32::try_from(self.builder.body.len())
            .map_err(|_| ArtifactError::Malformed("geometry payload too large"))?;
        let len = body_end
            .checked_sub(self.body_start)
            .ok_or(ArtifactError::Malformed("geometry section too large"))?;
        self.builder.spans.push((self.body_start, len));
        self.builder
            .index
            .push((self.user_id, self.bbox.snapshot(), self.geom_type));
        self.ended = true;
        Ok(())
    }
}

impl Drop for FeatureWriter<'_> {
    fn drop(&mut self) {
        if !self.ended {
            // roll back any body bytes the caller wrote; the index entry was
            // never appended, so dropping mid-feature leaves the builder
            // consistent for retry.
            self.builder.body.truncate(self.body_start as usize);
        }
    }
}
