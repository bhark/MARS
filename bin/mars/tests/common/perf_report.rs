//! perf-table emission for the MARS-vs-MapServer harness.
//!
//! deserialises the `timings.json` produced by mars-diff-capture and renders
//! a markdown table comparing per-case p50/p95 between MARS and MapServer.
//! the host harness writes this to stdout and (when `MARS_PERF_REPORT=<path>`
//! is set) to a file.

use std::fmt::Write as _;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Bundle {
    pub run: RunMeta,
    pub cases: Vec<CaseResult>,
}

#[derive(Debug, Deserialize)]
pub struct RunMeta {
    #[serde(default)]
    pub started_at_unix: i64,
    #[serde(default)]
    pub finished_at_unix: i64,
    #[serde(default)]
    pub samples: usize,
    #[serde(default)]
    pub warmup: usize,
    #[serde(default)]
    pub mapserver_url: String,
    #[serde(default)]
    pub mapserver_image: String,
    #[serde(default)]
    pub mapfile_sha: String,
    #[serde(default)]
    pub postgis_lsn_start: String,
    #[serde(default)]
    pub postgis_lsn_end: String,
    #[serde(default)]
    pub host: String,
}

#[derive(Debug, Deserialize)]
pub struct CaseResult {
    pub name: String,
    pub layers: Vec<String>,
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub crs: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub scale_denom: Option<u64>,
    #[serde(default)]
    pub tolerance: u8,
    #[serde(default)]
    pub max_diff_ratio: f32,
    pub mars: SideTimings,
    pub mapserver: SideTimings,
}

#[derive(Debug, Deserialize)]
pub struct SideTimings {
    #[serde(default)]
    pub samples_ms: Vec<f64>,
    #[serde(default)]
    pub p50_ms: f64,
    #[serde(default)]
    pub p95_ms: f64,
    #[serde(default)]
    pub failures: usize,
}

/// render a markdown table comparing MARS vs MapServer per-case timings.
pub fn render_markdown(bundle: &Bundle) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "## MARS vs MapServer perf — {} cases × {} samples (warmup {})",
        bundle.cases.len(),
        bundle.run.samples,
        bundle.run.warmup,
    );
    if !bundle.run.mapserver_image.is_empty() {
        let _ = writeln!(out, "- mapserver: `{}`", bundle.run.mapserver_image);
    }
    if !bundle.run.mapfile_sha.is_empty() {
        let _ = writeln!(
            out,
            "- mapfile sha: `{}`",
            &bundle.run.mapfile_sha[..bundle.run.mapfile_sha.len().min(12)]
        );
    }
    if !bundle.run.postgis_lsn_start.is_empty() {
        let _ = writeln!(
            out,
            "- postgis LSN: `{}` → `{}`{}",
            bundle.run.postgis_lsn_start,
            bundle.run.postgis_lsn_end,
            if bundle.run.postgis_lsn_start == bundle.run.postgis_lsn_end {
                " (no drift)"
            } else {
                " (drifted during run)"
            },
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| case | layers | mars p50 | mars p95 | ms p50 | ms p95 | mars/ms p50 | mars failures | ms failures |"
    );
    let _ = writeln!(
        out,
        "|------|--------|---------:|---------:|-------:|-------:|------------:|--------------:|------------:|"
    );
    for c in &bundle.cases {
        let ratio = if c.mapserver.p50_ms > 0.0 {
            format!("{:.2}×", c.mars.p50_ms / c.mapserver.p50_ms)
        } else {
            "n/a".to_owned()
        };
        let _ = writeln!(
            out,
            "| {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} |",
            c.name,
            c.layers.join("+"),
            c.mars.p50_ms,
            c.mars.p95_ms,
            c.mapserver.p50_ms,
            c.mapserver.p95_ms,
            ratio,
            c.mars.failures,
            c.mapserver.failures,
        );
    }
    out
}
