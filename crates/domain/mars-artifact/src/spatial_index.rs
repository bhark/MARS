//! packed hilbert r-tree. flatbush-style mmap-friendly layout.
//!
//! reference algorithm from flatbush (vladimir agafonkin, isc license).
//! all parsing avoids `unsafe` and uses `from_le_bytes` over byte slices.
//! the section payload (lives at `SectionKind::SpatialIndex`) is:
//!
//! ```text
//! +------------------------------------+   header (36 bytes)
//! | magic [4]u8  = b"SPIX"             |
//! | version u8   = 1                   |
//! | flags u8     = 0 (reserved)        |
//! | node_size u16                      |
//! | num_items u32                      |
//! | num_nodes u32                      |
//! | num_levels u32                     |
//! | bbox [4]f32                        |
//! +------------------------------------+
//! | level_offsets [num_levels + 1]u32  |   byte offsets within the nodes
//! |                                    |   region; trailing sentinel = end
//! +------------------------------------+
//! | nodes (level 0 = leaves first,     |
//! |        then level 1, ... root)     |
//! |   per node: bbox [4]f32 (16 B)     |
//! |             child u32  (4 B)       |   leaf:    caller-supplied index
//! |                                    |   internal byte offset of first
//! |                                    |   child within nodes region
//! +------------------------------------+
//! ```

use bytes::Bytes;

use crate::ArtifactError;

/// default node size (children per node). flatbush default; per-binding
/// configurable per binding.
pub const DEFAULT_NODE_SIZE: u16 = 16;

const MAGIC: &[u8; 4] = b"SPIX";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 36;
const NODE_LEN: usize = 20;
const HILBERT_GRID: u32 = 1 << 16;

/// build-side accumulator. emits a packed hilbert r-tree on `finish()`.
#[derive(Debug)]
pub struct SpatialIndexBuilder {
    node_size: u16,
    items: Vec<(u32, [f32; 4])>,
}

impl SpatialIndexBuilder {
    /// `node_size` must be in `2..=u16::MAX`. typical: `DEFAULT_NODE_SIZE`.
    pub fn new(node_size: u16) -> Result<Self, ArtifactError> {
        if node_size < 2 {
            return Err(ArtifactError::InvalidWriterState(
                "spatial index node_size must be >= 2",
            ));
        }
        Ok(Self {
            node_size,
            items: Vec::new(),
        })
    }

    /// caller-supplied `idx` is opaque to the codec; the same value is what
    /// each `query` returns. typical usage: feature index into the geometry
    /// payload.
    pub fn add(&mut self, idx: u32, bbox: [f32; 4]) -> &mut Self {
        self.items.push((idx, bbox));
        self
    }

    pub fn finish(self) -> Result<Bytes, ArtifactError> {
        let Self { node_size, items } = self;
        let n = items.len();
        let n_u32: u32 = n
            .try_into()
            .map_err(|_| ArtifactError::Malformed("spatial index: too many items"))?;

        // empty index: single sentinel level offset, no nodes.
        if n == 0 {
            let mut out = Vec::with_capacity(HEADER_LEN + 4);
            write_header(
                &mut out,
                node_size,
                0,
                0,
                0,
                [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY],
            );
            out.extend_from_slice(&0u32.to_le_bytes());
            return Ok(Bytes::from(out));
        }

        // reject non-finite item bboxes up front so hilbert + min/max stay sane.
        for (_, b) in &items {
            if !b[0].is_finite() || !b[1].is_finite() || !b[2].is_finite() || !b[3].is_finite() {
                return Err(ArtifactError::Malformed("spatial index: non-finite bbox"));
            }
        }

        // global extent, precise enough for hilbert quantisation.
        let mut g = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
        for (_, b) in &items {
            if b[0] < g[0] {
                g[0] = b[0];
            }
            if b[1] < g[1] {
                g[1] = b[1];
            }
            if b[2] > g[2] {
                g[2] = b[2];
            }
            if b[3] > g[3] {
                g[3] = b[3];
            }
        }

        // hilbert key per item; sort by it.
        let width = f64::from(g[2]) - f64::from(g[0]);
        let height = f64::from(g[3]) - f64::from(g[1]);
        let scale = f64::from(HILBERT_GRID - 1);
        let mut keyed: Vec<(u32, u32, [f32; 4])> = Vec::with_capacity(n);
        for &(orig_idx, b) in &items {
            let cx = (f64::from(b[0]) + f64::from(b[2])) * 0.5;
            let cy = (f64::from(b[1]) + f64::from(b[3])) * 0.5;
            let hx = if width > 0.0 {
                (((cx - f64::from(g[0])) / width) * scale).clamp(0.0, scale) as u32
            } else {
                0
            };
            let hy = if height > 0.0 {
                (((cy - f64::from(g[1])) / height) * scale).clamp(0.0, scale) as u32
            } else {
                0
            };
            keyed.push((hilbert_xy_to_index(hx, hy), orig_idx, b));
        }
        keyed.sort_by_key(|&(k, _, _)| k);

        // bottom-up pack. levels[0] = leaves, levels[L-1] = root.
        let node_size_usize = node_size as usize;
        let mut cur: Vec<Node> = keyed
            .into_iter()
            .map(|(_, idx, bbox)| Node { bbox, child: idx })
            .collect();

        let mut prev_byte_start: u32 = 0;
        let mut byte_off: u32 = u32_from_usize(cur.len() * NODE_LEN)?;
        let mut level_byte_starts: Vec<u32> = vec![0];
        let mut levels: Vec<Vec<Node>> = Vec::new();

        while cur.len() > 1 {
            let prev_count = cur.len();
            let parents_count = prev_count.div_ceil(node_size_usize);
            let mut parents: Vec<Node> = Vec::with_capacity(parents_count);
            for p in 0..parents_count {
                let first = p * node_size_usize;
                let last = (first + node_size_usize).min(prev_count);
                let mut pbb = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
                for c in &cur[first..last] {
                    if c.bbox[0] < pbb[0] {
                        pbb[0] = c.bbox[0];
                    }
                    if c.bbox[1] < pbb[1] {
                        pbb[1] = c.bbox[1];
                    }
                    if c.bbox[2] > pbb[2] {
                        pbb[2] = c.bbox[2];
                    }
                    if c.bbox[3] > pbb[3] {
                        pbb[3] = c.bbox[3];
                    }
                }
                let child_byte_off = prev_byte_start
                    .checked_add(u32_from_usize(first * NODE_LEN)?)
                    .ok_or(ArtifactError::Malformed("spatial index: child offset overflow"))?;
                parents.push(Node {
                    bbox: pbb,
                    child: child_byte_off,
                });
            }
            level_byte_starts.push(byte_off);
            prev_byte_start = byte_off;
            byte_off = byte_off
                .checked_add(u32_from_usize(parents_count * NODE_LEN)?)
                .ok_or(ArtifactError::Malformed("spatial index: byte offset overflow"))?;
            levels.push(std::mem::replace(&mut cur, parents));
        }
        levels.push(cur);
        level_byte_starts.push(byte_off);

        let num_levels: u32 = u32_from_usize(levels.len())?;
        let total_nodes: usize = levels.iter().map(Vec::len).sum();
        let total_nodes_u32: u32 = u32_from_usize(total_nodes)?;

        let mut out = Vec::with_capacity(HEADER_LEN + level_byte_starts.len() * 4 + total_nodes * NODE_LEN);
        write_header(&mut out, node_size, n_u32, total_nodes_u32, num_levels, g);
        for &v in &level_byte_starts {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for level in &levels {
            for n in level {
                for v in n.bbox {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                out.extend_from_slice(&n.child.to_le_bytes());
            }
        }

        Ok(Bytes::from(out))
    }
}

#[derive(Debug, Clone, Copy)]
struct Node {
    bbox: [f32; 4],
    child: u32,
}

fn write_header(out: &mut Vec<u8>, node_size: u16, num_items: u32, num_nodes: u32, num_levels: u32, bbox: [f32; 4]) {
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(0);
    out.extend_from_slice(&node_size.to_le_bytes());
    out.extend_from_slice(&num_items.to_le_bytes());
    out.extend_from_slice(&num_nodes.to_le_bytes());
    out.extend_from_slice(&num_levels.to_le_bytes());
    for v in bbox {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

fn u32_from_usize(v: usize) -> Result<u32, ArtifactError> {
    v.try_into()
        .map_err(|_| ArtifactError::Malformed("spatial index: size exceeds u32"))
}

/// query-side handle. zero-copy over the section payload.
#[derive(Debug, Clone)]
pub struct SpatialIndex {
    bytes: Bytes,
    node_size: u16,
    num_items: u32,
    num_levels: u32,
    bbox: [f32; 4],
    nodes_offset: usize,
    level_starts: Vec<u32>, // byte offsets within nodes region; len == num_levels + 1
}

impl SpatialIndex {
    pub fn open(bytes: Bytes) -> Result<Self, ArtifactError> {
        if bytes.len() < HEADER_LEN {
            return Err(ArtifactError::Truncated);
        }
        let h = &bytes[..HEADER_LEN];
        if &h[..4] != MAGIC {
            return Err(ArtifactError::Malformed("spatial index: bad magic"));
        }
        if h[4] != VERSION {
            return Err(ArtifactError::Malformed("spatial index: unsupported version"));
        }
        if h[5] != 0 {
            return Err(ArtifactError::Malformed("spatial index: nonzero flags"));
        }
        let node_size = u16::from_le_bytes([h[6], h[7]]);
        if node_size < 2 {
            return Err(ArtifactError::Malformed("spatial index: node_size < 2"));
        }
        let num_items = u32::from_le_bytes([h[8], h[9], h[10], h[11]]);
        let num_nodes = u32::from_le_bytes([h[12], h[13], h[14], h[15]]);
        let num_levels = u32::from_le_bytes([h[16], h[17], h[18], h[19]]);
        let bbox = [
            f32::from_le_bytes([h[20], h[21], h[22], h[23]]),
            f32::from_le_bytes([h[24], h[25], h[26], h[27]]),
            f32::from_le_bytes([h[28], h[29], h[30], h[31]]),
            f32::from_le_bytes([h[32], h[33], h[34], h[35]]),
        ];

        // empty: num_items == 0 implies num_nodes == 0, num_levels == 0,
        // exactly one sentinel level offset == 0.
        if num_items == 0 {
            if num_nodes != 0 || num_levels != 0 {
                return Err(ArtifactError::Malformed("spatial index: empty header inconsistent"));
            }
            let need = HEADER_LEN + 4;
            if bytes.len() < need {
                return Err(ArtifactError::Truncated);
            }
            let sentinel = u32::from_le_bytes([
                bytes[HEADER_LEN],
                bytes[HEADER_LEN + 1],
                bytes[HEADER_LEN + 2],
                bytes[HEADER_LEN + 3],
            ]);
            if sentinel != 0 {
                return Err(ArtifactError::Malformed("spatial index: empty sentinel nonzero"));
            }
            return Ok(Self {
                bytes,
                node_size,
                num_items: 0,
                num_levels: 0,
                bbox,
                nodes_offset: HEADER_LEN + 4,
                level_starts: vec![0],
            });
        }

        if num_levels == 0 {
            return Err(ArtifactError::Malformed("spatial index: zero levels with items"));
        }

        // level_offsets table: (num_levels + 1) u32 entries
        let levels_table_len = (num_levels as usize)
            .checked_add(1)
            .and_then(|v| v.checked_mul(4))
            .ok_or(ArtifactError::Malformed("spatial index: levels table overflow"))?;
        let table_off = HEADER_LEN;
        let nodes_offset = table_off
            .checked_add(levels_table_len)
            .ok_or(ArtifactError::Malformed("spatial index: nodes offset overflow"))?;
        if bytes.len() < nodes_offset {
            return Err(ArtifactError::Truncated);
        }

        let mut level_starts: Vec<u32> = Vec::with_capacity(num_levels as usize + 1);
        for i in 0..=num_levels as usize {
            let o = table_off + i * 4;
            level_starts.push(u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]));
        }

        // monotonic, first == 0, last == num_nodes * NODE_LEN.
        if level_starts[0] != 0 {
            return Err(ArtifactError::Malformed("spatial index: level_starts[0] != 0"));
        }
        let nodes_total_bytes = (num_nodes as u64).saturating_mul(NODE_LEN as u64);
        if u64::from(*level_starts.last().unwrap_or(&0)) != nodes_total_bytes {
            return Err(ArtifactError::Malformed("spatial index: level sentinel mismatch"));
        }
        for w in level_starts.windows(2) {
            if w[0] >= w[1] {
                return Err(ArtifactError::Malformed("spatial index: non-monotonic level_starts"));
            }
            // each level must contain a whole number of NODE_LEN-sized nodes.
            if (w[1] - w[0]) % (NODE_LEN as u32) != 0 {
                return Err(ArtifactError::Malformed("spatial index: level span not aligned"));
            }
        }
        // top level must have exactly one node (the root).
        let top_idx = num_levels as usize - 1;
        if level_starts[top_idx + 1] - level_starts[top_idx] != NODE_LEN as u32 {
            return Err(ArtifactError::Malformed("spatial index: top level not a single node"));
        }

        // total node-region bytes must fit in the buffer.
        let nodes_region_end = nodes_offset
            .checked_add(usize::try_from(nodes_total_bytes).map_err(|_| ArtifactError::Truncated)?)
            .ok_or(ArtifactError::Malformed("spatial index: nodes region overflow"))?;
        if bytes.len() < nodes_region_end {
            return Err(ArtifactError::Truncated);
        }

        Ok(Self {
            bytes,
            node_size,
            num_items,
            num_levels,
            bbox,
            nodes_offset,
            level_starts,
        })
    }

    #[must_use]
    pub fn len(&self) -> u32 {
        self.num_items
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.num_items == 0
    }

    #[must_use]
    pub fn node_size(&self) -> u16 {
        self.node_size
    }

    #[must_use]
    pub fn root_bbox(&self) -> [f32; 4] {
        self.bbox
    }

    /// returns indices of items whose stored bbox intersects `qbb`. allocates
    /// nothing beyond growing `out`.
    pub fn query(&self, qbb: [f32; 4], out: &mut Vec<u32>) {
        self.query_visit(qbb, |idx| out.push(idx));
    }

    /// visitor form. avoids the result-vec when the caller folds inline.
    pub fn query_visit(&self, qbb: [f32; 4], mut visit: impl FnMut(u32)) {
        if self.num_items == 0 {
            return;
        }
        let leaf_level_end = self.level_starts[1]; // first internal-level start, i.e. end of leaves
        let mut stack: Vec<u32> = Vec::with_capacity(32);
        // root is the last node; it sits at level_starts[num_levels - 1]
        let root_off = self.level_starts[self.num_levels as usize - 1];
        stack.push(root_off);

        while let Some(off) = stack.pop() {
            let node = self.read_node(off);
            if !bbox_intersects(node.bbox, qbb) {
                continue;
            }
            if off < leaf_level_end {
                visit(node.child);
                continue;
            }
            // internal node: descend. children sit contiguously at byte
            // offset `node.child` within nodes region; up to node_size of
            // them, capped by the next sibling group / level boundary.
            let max_children_bytes = u32::from(self.node_size) * (NODE_LEN as u32);
            let level_end = self.level_end_containing(node.child);
            let children_end = node.child.saturating_add(max_children_bytes).min(level_end);
            let mut c = node.child;
            while c < children_end {
                stack.push(c);
                c += NODE_LEN as u32;
            }
        }
    }

    fn read_node(&self, off: u32) -> Node {
        let abs = self.nodes_offset + off as usize;
        let s = &self.bytes[abs..abs + NODE_LEN];
        Node {
            bbox: [
                f32::from_le_bytes([s[0], s[1], s[2], s[3]]),
                f32::from_le_bytes([s[4], s[5], s[6], s[7]]),
                f32::from_le_bytes([s[8], s[9], s[10], s[11]]),
                f32::from_le_bytes([s[12], s[13], s[14], s[15]]),
            ],
            child: u32::from_le_bytes([s[16], s[17], s[18], s[19]]),
        }
    }

    /// returns the `level_starts[i+1]` boundary for the level that contains
    /// the byte offset `off`. used to cap how far children may extend.
    fn level_end_containing(&self, off: u32) -> u32 {
        // level_starts are strictly monotonic. find the first start that is
        // strictly > off; its index is `level + 1`.
        let mut lo = 0usize;
        let mut hi = self.level_starts.len();
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.level_starts[mid] <= off {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        // off is in [level_starts[lo], level_starts[lo + 1]); end is
        // level_starts[lo + 1]. saturate at the last entry for malformed
        // descent (defensive - open() already validated bounds).
        *self.level_starts.get(lo + 1).unwrap_or(&self.level_starts[lo])
    }
}

#[inline]
fn bbox_intersects(a: [f32; 4], b: [f32; 4]) -> bool {
    a[0] <= b[2] && a[1] <= b[3] && a[2] >= b[0] && a[3] >= b[1]
}

/// hilbert key over a 16-bit grid (flatbush convention). bit-twiddled
/// implementation of the d2xy inverse; safe over u32 with all wrapping ops.
fn hilbert_xy_to_index(x: u32, y: u32) -> u32 {
    let mut a = x ^ y;
    let mut b = 0xFFFF ^ a;
    let mut c = 0xFFFF ^ (x | y);
    let mut d = x & (y ^ 0xFFFF);

    let mut na = a | (b >> 1);
    let mut nb = (a >> 1) ^ a;
    let mut nc = ((c >> 1) ^ (b & (d >> 1))) ^ c;
    let mut nd = ((a & (c >> 1)) ^ (d >> 1)) ^ d;

    a = na;
    b = nb;
    c = nc;
    d = nd;
    na = (a & (a >> 2)) ^ (b & (b >> 2));
    nb = (a & (b >> 2)) ^ (b & ((a ^ b) >> 2));
    nc ^= (a & (c >> 2)) ^ (b & (d >> 2));
    nd ^= (b & (c >> 2)) ^ ((a ^ b) & (d >> 2));

    a = na;
    b = nb;
    c = nc;
    d = nd;
    na = (a & (a >> 4)) ^ (b & (b >> 4));
    nb = (a & (b >> 4)) ^ (b & ((a ^ b) >> 4));
    nc ^= (a & (c >> 4)) ^ (b & (d >> 4));
    nd ^= (b & (c >> 4)) ^ ((a ^ b) & (d >> 4));

    a = na;
    b = nb;
    c = nc;
    d = nd;
    nc ^= (a & (c >> 8)) ^ (b & (d >> 8));
    nd ^= (b & (c >> 8)) ^ ((a ^ b) & (d >> 8));

    a = nc ^ (nc >> 1);
    b = nd ^ (nd >> 1);

    let mut i0 = x ^ y;
    let mut i1 = b | (0xFFFF ^ (i0 | a));

    i0 = (i0 | (i0 << 8)) & 0x00FF_00FF;
    i0 = (i0 | (i0 << 4)) & 0x0F0F_0F0F;
    i0 = (i0 | (i0 << 2)) & 0x3333_3333;
    i0 = (i0 | (i0 << 1)) & 0x5555_5555;

    i1 = (i1 | (i1 << 8)) & 0x00FF_00FF;
    i1 = (i1 | (i1 << 4)) & 0x0F0F_0F0F;
    i1 = (i1 | (i1 << 2)) & 0x3333_3333;
    i1 = (i1 | (i1 << 1)) & 0x5555_5555;

    (i1 << 1) | i0
}
