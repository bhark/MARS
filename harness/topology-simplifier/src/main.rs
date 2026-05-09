//! topology-simplifier harness entrypoint.
//!
//! workspace-excluded operator tool; see README. successive commits flesh out
//! ingest, graph, dp, reassembly, verifier, timing, and image cross-check
//! modules.

use clap::Parser;

mod dp;
mod graph;
mod ingest;
mod reassemble;

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
        "topology-simplifier: fixture={} out={} quantise={}mm tolerances={:?}",
        args.fixture.display(),
        args.out.display(),
        args.quantise_mm,
        args.tolerance_m,
    );
    let _ = args.degenerate_threshold;

    let (geoms, stats) = ingest::load_fixture(&args.fixture)?;
    eprintln!(
        "ingest: lines={} kept={} skipped_non_polygon={} skipped_bad_line={} skipped_bad_hex={} skipped_bad_wkb={}",
        stats.lines_read,
        stats.kept,
        stats.skipped_non_polygon,
        stats.skipped_bad_line,
        stats.skipped_bad_hex,
        stats.skipped_bad_wkb,
    );
    let (topo, gstats) = graph::build_topology(&geoms, args.quantise_mm);
    eprintln!(
        "graph: features={} rings={} vertices={} edges={} junctions={} arcs={} shared={} islands={}",
        gstats.feature_count,
        gstats.ring_count,
        gstats.vertex_count,
        gstats.edge_count,
        gstats.junction_count,
        gstats.arc_count,
        gstats.shared_arc_count,
        gstats.island_arc_count,
    );
    for (i, tol) in args.tolerance_m.iter().enumerate() {
        let simp = dp::simplify_arcs(&topo, *tol);
        let (_out, rstats) = reassemble::reassemble(&geoms, &topo, &simp);
        eprintln!(
            "level {} (tolerance {} m): rings={} collapsed_ring={} collapsed_arc={} invalid_hole={} self_isect={}",
            i + 1,
            tol,
            rstats.rings_total,
            rstats.collapsed_ring_count,
            rstats.collapsed_arc_count,
            rstats.invalid_reassembly_count,
            rstats.self_intersection_count,
        );
    }
    eprintln!("(verify module lands in next commit)");
    Ok(())
}
