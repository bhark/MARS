//! topology-simplifier harness entrypoint.
//!
//! workspace-excluded operator tool; see README. the binary scaffolding lives
//! here from commit 1 — subsequent commits flesh out ingest, graph, dp,
//! reassembly, verifier, timing, and image cross-check modules.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "topology-simplifier", about = "Phase 0 topology-aware simplification spike")]
struct Args {
    /// path to TSV (id\thex_ewkb) polygon fixture dump
    #[arg(long)]
    fixture: std::path::PathBuf,

    /// output directory for gate report artifacts
    #[arg(long)]
    out: std::path::PathBuf,

    /// quantisation grid in millimetres (canonical-CRS units)
    #[arg(long, default_value_t = 1)]
    quantise_mm: u32,

    /// per-arc DP tolerances in metres (one per level); applied in order
    #[arg(long, value_delimiter = ',', default_values_t = [1.0_f64, 5.0, 25.0])]
    tolerance_m: Vec<f64>,

    /// max acceptable degenerate-reassembly fraction (per gate)
    #[arg(long, default_value_t = 0.001)]
    degenerate_threshold: f64,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    eprintln!(
        "topology-simplifier scaffold: fixture={} out={} quantise={}mm tolerances={:?}",
        args.fixture.display(),
        args.out.display(),
        args.quantise_mm,
        args.tolerance_m,
    );
    eprintln!("(scaffold only — pipeline modules land in subsequent commits)");
    let _ = args.degenerate_threshold;
    Ok(())
}
