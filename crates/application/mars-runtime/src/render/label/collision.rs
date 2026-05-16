//! greedy collision pass + bbox overlap math.

use mars_render_port::DrawOp;

use super::candidate::{PreparedLabel, PreparedPlacement};
use super::geometry::apply_offset;

/// pixel side length of one collision-grid cell. balances cell-coverage cost
/// (small labels span few cells) against per-cell candidate-list length
/// (avg label hits ~1-4 cells at this granularity). 64px also matches the
/// typical glyph cluster size for label fonts at 12-16pt.
const COLLISION_GRID_CELL_PX: f32 = 64.0;

/// run a greedy collision pass over the accumulated label set and return
/// the surviving `DrawOp::Label` ops in placement order. each placed label
/// remembers its `min_distance`; collision against a candidate uses the
/// max of the two values, so the wider neighbour wins per pair (mirrors
/// mapserver's `MINDISTANCE`, post-7.2 pixel semantics). AUTO-positioned
/// labels try each candidate placement in mapserver order; the first
/// non-colliding one is placed.
///
/// uses a uniform grid to skip the O(N) scan over previously-placed labels.
/// the grid is a conservative filter (it can over-report candidates) and
/// each hit is confirmed by the same exact `bboxes_within` check used
/// before, so the picked set is byte-identical to a brute-force scan.
#[doc(hidden)]
#[allow(unreachable_pub)]
pub fn collide_and_emit_labels(mut labels: Vec<PreparedLabel>, w: u32, h: u32) -> Vec<DrawOp> {
    if labels.is_empty() {
        return Vec::new();
    }
    // force-first, then priority desc. forced labels are placed regardless
    // of collision; sorting them up front means subsequent labels see them
    // as obstacles and can dodge.
    labels.sort_by_key(|l| (std::cmp::Reverse(l.style.force), std::cmp::Reverse(l.priority)));
    let mut placed: Vec<PlacedFootprint> = Vec::with_capacity(labels.len());
    let mut ops = Vec::with_capacity(labels.len());
    let mut grid = CollisionGrid::new(w, h, COLLISION_GRID_CELL_PX);
    for label in labels {
        let cand_md = label.style.min_distance.max(0.0);
        let force = label.style.force;
        // Follow labels emit a different DrawOp variant, so split the
        // collision + emit path off here. block placements (Fixed/Auto)
        // share the existing pick_placement helper.
        match label.placement {
            PreparedPlacement::Follow {
                polyline_px,
                start_arc_px,
                bbox_px,
            } => {
                if !force && collides(bbox_px, cand_md, &placed, &grid) {
                    continue;
                }
                let idx = u32::try_from(placed.len()).unwrap_or(u32::MAX);
                placed.push(PlacedFootprint {
                    bbox: bbox_px,
                    min_distance: cand_md,
                });
                grid.insert(idx, bbox_px, cand_md);
                ops.push(DrawOp::FollowLabel {
                    polyline_px,
                    start_arc_px,
                    text: label.text,
                    style: label.style,
                });
            }
            placement @ (PreparedPlacement::Fixed { .. } | PreparedPlacement::Auto { .. }) => {
                let chosen = if force {
                    force_pick(&placement)
                } else {
                    pick_placement(&placement, cand_md, &placed, &grid)
                };
                let Some((anchor_offset, bbox_used)) = chosen else {
                    continue;
                };
                let anchor = apply_offset(label.raw_anchor_px, anchor_offset, label.angle_rad);
                let idx = u32::try_from(placed.len()).unwrap_or(u32::MAX);
                placed.push(PlacedFootprint {
                    bbox: bbox_used,
                    min_distance: cand_md,
                });
                grid.insert(idx, bbox_used, cand_md);
                ops.push(DrawOp::Label {
                    anchor,
                    text: label.text,
                    style: label.style,
                    angle_rad: label.angle_rad,
                });
            }
        }
    }
    ops
}

/// FORCE bypass for block (non-FOLLOW) placements: take the only `Fixed`
/// slot or the first `Auto` candidate. no collision test, no AUTO dodging.
/// mirrors mapserver `FORCE`.
fn force_pick(placement: &PreparedPlacement) -> Option<ChosenPlacement> {
    match placement {
        PreparedPlacement::Fixed {
            anchor_offset_px,
            bbox_px,
        } => Some((*anchor_offset_px, *bbox_px)),
        PreparedPlacement::Auto { candidates } => candidates.first().map(|c| (c.anchor_offset_px, c.bbox_px)),
        // Follow handled inline in the loop above; should never reach here.
        PreparedPlacement::Follow { .. } => None,
    }
}

/// chosen placement output: `(label-local-frame anchor offset, bbox in
/// canvas frame)`.
type ChosenPlacement = ((f32, f32), (f32, f32, f32, f32));

/// pick the first non-colliding candidate from a [`PreparedPlacement`].
/// returns `(label-local-frame anchor offset, bbox in canvas frame)` for
/// the chosen placement, or `None` when no candidate fits.
fn pick_placement(
    placement: &PreparedPlacement,
    cand_md: f32,
    placed: &[PlacedFootprint],
    grid: &CollisionGrid,
) -> Option<ChosenPlacement> {
    match placement {
        PreparedPlacement::Fixed {
            anchor_offset_px,
            bbox_px,
        } => {
            if collides(*bbox_px, cand_md, placed, grid) {
                None
            } else {
                Some((*anchor_offset_px, *bbox_px))
            }
        }
        PreparedPlacement::Auto { candidates } => candidates
            .iter()
            .find(|c| !collides(c.bbox_px, cand_md, placed, grid))
            .map(|c| (c.anchor_offset_px, c.bbox_px)),
        // Follow is handled inline in `collide_and_emit_labels`; should
        // never reach pick_placement.
        PreparedPlacement::Follow { .. } => None,
    }
}

fn collides(
    bbox: (f32, f32, f32, f32),
    cand_md: f32,
    placed: &[PlacedFootprint],
    grid: &CollisionGrid,
) -> bool {
    let mut hit = false;
    grid.for_each_candidate(bbox, cand_md, |idx| {
        let p = &placed[idx as usize];
        if bboxes_within(bbox, p.bbox, cand_md.max(p.min_distance)) {
            hit = true;
            return false;
        }
        true
    });
    hit
}

struct PlacedFootprint {
    bbox: (f32, f32, f32, f32),
    min_distance: f32,
}

/// uniform spatial grid keyed on canvas pixel coordinates. each cell holds
/// indices into the placed-labels vec for every label whose padded bbox
/// touches that cell. lookup expands the candidate's bbox by its own padding
/// and walks just those cells; the grid is a conservative filter, so each
/// hit is still confirmed by the exact `bboxes_within` predicate.
///
/// false positives are bounded (a label spans at most ceil(span/cell)
/// rows*cols, typically 1-4 cells), so the same placed-label index may be
/// visited a handful of times per candidate. that constant factor is the
/// trade-off for skipping the O(N) scan.
struct CollisionGrid {
    cell_size: f32,
    cols: usize,
    rows: usize,
    cells: Vec<Vec<u32>>,
}

impl CollisionGrid {
    fn new(w: u32, h: u32, cell_size: f32) -> Self {
        let cell_size = cell_size.max(1.0);
        let cols = (((w as f32) / cell_size).ceil() as usize).max(1);
        let rows = (((h as f32) / cell_size).ceil() as usize).max(1);
        Self {
            cell_size,
            cols,
            rows,
            cells: vec![Vec::new(); cols * rows],
        }
    }

    /// inclusive (x0, y0, x1, y1) cell range covered by `bbox` inflated by
    /// `pad` on every side, clamped to the grid bounds. labels extending
    /// outside the canvas (or beyond pad) collapse onto the edge row/col,
    /// which is fine: collisions are still found and exact check filters.
    fn cell_range(&self, bbox: (f32, f32, f32, f32), pad: f32) -> (usize, usize, usize, usize) {
        let pad = pad.max(0.0);
        let cols_max = self.cols.saturating_sub(1) as f32;
        let rows_max = self.rows.saturating_sub(1) as f32;
        let x0 = ((bbox.0 - pad) / self.cell_size).floor().clamp(0.0, cols_max);
        let y0 = ((bbox.1 - pad) / self.cell_size).floor().clamp(0.0, rows_max);
        let x1 = ((bbox.2 + pad) / self.cell_size).floor().clamp(0.0, cols_max);
        let y1 = ((bbox.3 + pad) / self.cell_size).floor().clamp(0.0, rows_max);
        (x0 as usize, y0 as usize, x1 as usize, y1 as usize)
    }

    fn insert(&mut self, idx: u32, bbox: (f32, f32, f32, f32), pad: f32) {
        let (x0, y0, x1, y1) = self.cell_range(bbox, pad);
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.cells[y * self.cols + x].push(idx);
            }
        }
    }

    /// invoke `visit` with every index whose stored cells overlap the
    /// lookup region. duplicate visits are possible (a label touching N
    /// lookup cells appears N times); callers handle short-circuit via the
    /// return value (`false` stops iteration). no per-call allocation.
    fn for_each_candidate(&self, bbox: (f32, f32, f32, f32), pad: f32, mut visit: impl FnMut(u32) -> bool) {
        let (x0, y0, x1, y1) = self.cell_range(bbox, pad);
        for y in y0..=y1 {
            for x in x0..=x1 {
                for &idx in &self.cells[y * self.cols + x] {
                    if !visit(idx) {
                        return;
                    }
                }
            }
        }
    }
}

/// `true` when `a` inflated by `pad` on every side overlaps `b`. equivalent
/// to "the gap between the bboxes is < pad", so passing `pad == 0` reduces
/// to a plain overlap test.
fn bboxes_within(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32), pad: f32) -> bool {
    let pad = pad.max(0.0);
    let inflated = (a.0 - pad, a.1 - pad, a.2 + pad, a.3 + pad);
    pixel_bbox_overlaps(inflated, b)
}

fn pixel_bbox_overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.2 && a.2 > b.0 && a.1 < b.3 && a.3 > b.1
}
