//! rebalance stage 2: collect candidate ops across every (binding, level)
//! from the current manifest. pure - no I/O, no allocation beyond the
//! returned vec.

use mars_types::PageEntry;

use crate::rebalance::{RebalanceOp, rebalance_candidates};
use crate::stages::ctx::RebalanceCtx;

pub(crate) fn collect(ctx: &RebalanceCtx) -> Vec<RebalanceOp> {
    let mut ops: Vec<RebalanceOp> = Vec::new();
    for binding_meta in &ctx.prior.bindings {
        let Some(binding_plan) = ctx
            .plan
            .bindings
            .iter()
            .find(|b| b.binding_id == binding_meta.binding_id)
        else {
            continue;
        };
        for level in &binding_meta.levels {
            let level_pages: Vec<PageEntry> = ctx
                .prior
                .pages
                .iter()
                .filter(|p| p.key.binding_id == binding_meta.binding_id && p.key.level == level.level)
                .cloned()
                .collect();
            ops.extend(rebalance_candidates(
                level,
                &level_pages,
                binding_plan.page_size_target_bytes,
            ));
        }
    }
    ops
}
