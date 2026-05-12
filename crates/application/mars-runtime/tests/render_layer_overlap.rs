//! ζ.1: per-layer rendering overlaps via FuturesUnordered, but the assembled
//! draw-op stream stays in plan order regardless of completion order.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use mars_render_port::DrawOp;
use mars_store::ObjectStore;

use common::{SleepingStore, build_multi_layer_fixture};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn render_assembles_layers_in_plan_order_under_skewed_completion() {
    // 3-layer plan; layer 0 is delayed the most, layer 2 returns immediately.
    // if the runtime preserved arrival order naively, ops would come back as
    // [L2, L1, L0]; the FuturesUnordered + index-keyed reassembly is what
    // restores plan order.
    let fix = build_multi_layer_fixture(3, |inner, page_keys| {
        let mut delays: HashMap<_, _> = HashMap::new();
        for (i, key) in page_keys.iter().enumerate() {
            // layer 0: 120ms, layer 1: 60ms, layer 2: 0ms
            let ms = 60 * (page_keys.len() - 1 - i) as u64;
            delays.insert(key.clone(), Duration::from_millis(ms));
        }
        Arc::new(SleepingStore::new(inner, delays)) as Arc<dyn ObjectStore>
    })
    .await;

    let plan = fix.render_plan();
    let _ = fix.runtime.render(&plan).await.expect("render");

    let log = fix.render_log.lock().unwrap();
    let path_fills: Vec<u8> = log
        .iter()
        .filter_map(|op| match op {
            DrawOp::Path { style, .. } => match style.fill {
                Some(mars_style::FillPaint::Solid(c)) => Some(c.r),
                _ => None,
            },
            _ => None,
        })
        .collect();

    // each layer emits exactly one path; fill.r encodes layer index via
    // 10 * (i + 1). plan-order is [L0, L1, L2] -> [10, 20, 30].
    assert_eq!(
        path_fills,
        vec![10u8, 20u8, 30u8],
        "expected paths in plan order regardless of fetch completion order"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn render_assembles_layers_in_plan_order_without_skew() {
    // baseline: no per-layer delay, just confirm the new pipeline still emits
    // every layer exactly once in plan order.
    let fix = build_multi_layer_fixture(4, |inner, _| inner).await;

    let plan = fix.render_plan();
    let _ = fix.runtime.render(&plan).await.expect("render");

    let log = fix.render_log.lock().unwrap();
    let path_fills: Vec<u8> = log
        .iter()
        .filter_map(|op| match op {
            DrawOp::Path { style, .. } => match style.fill {
                Some(mars_style::FillPaint::Solid(c)) => Some(c.r),
                _ => None,
            },
            _ => None,
        })
        .collect();

    assert_eq!(path_fills, vec![10u8, 20u8, 30u8, 40u8]);
}
