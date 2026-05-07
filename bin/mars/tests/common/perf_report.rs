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
    /// fraction (0..=1) of pixels carrying visible content. None on capture
    /// failure or older bundles that pre-date validity-aware assertions.
    #[serde(default)]
    pub mars_coverage: Option<f64>,
    #[serde(default)]
    pub mapserver_coverage: Option<f64>,
}

/// validity classification for a case based on the gap between MARS and
/// MapServer coverage. used by the host harness to decide whether a pixel-
/// diff overrun should fail the test or be reported as a soft warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validity {
    /// coverage gap < 2 pp; pixel-diff comparison is honest. budget enforced.
    Fair,
    /// 2-10 pp gap; comparison is suspect (likely partial layer exclusion).
    /// reported as a warning; not enforced.
    Suspect,
    /// gap above 10 pp, or MS effectively empty (under 0.5%). reported as
    /// info; not enforced. this is the "MapServer can't render this" bucket.
    Invalid,
    /// coverage missing on either side (older bundle / capture failure).
    Unknown,
}

impl Validity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fair => "fair",
            Self::Suspect => "suspect",
            Self::Invalid => "invalid",
            Self::Unknown => "unknown",
        }
    }
}

pub fn classify(c: &CaseResult) -> Validity {
    let (Some(mars), Some(ms)) = (c.mars_coverage, c.mapserver_coverage) else {
        return Validity::Unknown;
    };
    if ms < 0.005 {
        return Validity::Invalid;
    }
    let gap = (mars - ms).abs();
    if gap > 0.10 {
        Validity::Invalid
    } else if gap > 0.02 {
        Validity::Suspect
    } else {
        Validity::Fair
    }
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
        "| case | layers | validity | mars cov | ms cov | mars p50 | mars p95 | ms p50 | ms p95 | mars/ms p50 | mars fails | ms fails |"
    );
    let _ = writeln!(
        out,
        "|------|--------|----------|---------:|-------:|---------:|---------:|-------:|-------:|------------:|-----------:|---------:|"
    );
    for c in &bundle.cases {
        let ratio = if c.mapserver.p50_ms > 0.0 {
            format!("{:.2}×", c.mars.p50_ms / c.mapserver.p50_ms)
        } else {
            "n/a".to_owned()
        };
        let validity = classify(c).label();
        let fmt_cov = |v: Option<f64>| match v {
            Some(x) => format!("{:.3}", x),
            None => "-".to_owned(),
        };
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} |",
            c.name,
            c.layers.join("+"),
            validity,
            fmt_cov(c.mars_coverage),
            fmt_cov(c.mapserver_coverage),
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
